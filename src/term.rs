use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender, SyncSender};
use std::thread;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::Paragraph;
use tui_term::widget::{Cursor, PseudoTerminal};

use crate::app::{Event, TabHits, tab_line};

pub const HINTS: &str =
    "Alt+T new  Alt+W close  Alt+PgUp/PgDn tabs  Alt+S scroll mode  wheel scrolls  drag copies";
pub const SCROLL_HINTS: &str = "SCROLL MODE  ↑↓ PgUp/PgDn Home/End move  Esc leaves";

const SCROLLBACK: usize = 10_000;
const READ_CHUNK: usize = 8192;

/// Answers the questions programs ask their terminal. Without this, anything that
/// queries the cursor position or terminal type stalls until it times out.
#[derive(Default)]
struct Hooks {
    reply: Vec<u8>,
    title: Option<String>,
}

impl vt100::Callbacks for Hooks {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        let text = String::from_utf8_lossy(title);
        self.title = Some(text.chars().filter(|c| !c.is_control()).take(24).collect());
    }

    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        _: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let first = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        match (i1, c, first) {
            (None, 'n', 6) => {
                let (row, col) = screen.cursor_position();
                self.reply
                    .extend(format!("\x1b[{};{}R", row + 1, col + 1).bytes());
            }
            (None, 'n', 5) => self.reply.extend(b"\x1b[0n"),
            (None, 'c', 0) => self.reply.extend(b"\x1b[?62;22c"),
            (Some(b'>'), 'c', 0) => self.reply.extend(b"\x1b[>1;10;0c"),
            _ => {}
        }
    }
}

pub struct Term {
    id: u64,
    number: u64,
    parser: vt100::Parser<Hooks>,
    input: Sender<Vec<u8>>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    size: (u16, u16),
}

impl Drop for Term {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

impl Term {
    fn title(&self) -> String {
        match &self.parser.callbacks().title {
            Some(title) if !title.is_empty() => title.clone(),
            _ => format!("term {}", self.number),
        }
    }

    fn send(&mut self, bytes: Vec<u8>) {
        let _ = self.input.send(bytes);
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        if self.size != (rows, cols) {
            self.size = (rows, cols);
            let size = PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            };
            let _ = self.master.resize(size);
            self.parser.screen_mut().set_size(rows, cols);
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
        let reply = std::mem::take(&mut self.parser.callbacks_mut().reply);
        if !reply.is_empty() {
            self.send(reply);
        }
    }

    fn scroll(&mut self, delta: isize) {
        let current = self.parser.screen().scrollback() as isize;
        self.parser
            .screen_mut()
            .set_scrollback((current + delta).max(0) as usize);
    }
}

fn spawn(
    id: u64,
    number: u64,
    root: &PathBuf,
    size: (u16, u16),
    tx: SyncSender<Event>,
) -> Result<Term, String> {
    fn text(err: impl std::fmt::Display) -> String {
        err.to_string()
    }
    let (rows, cols) = size;
    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(text)?;
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned());
    let mut command = CommandBuilder::new(shell);
    command.cwd(root);
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    command.env("TERM_PROGRAM", "kalipso");
    let child = pair.slave.spawn_command(command).map_err(text)?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().map_err(text)?;
    let mut writer = pair.master.take_writer().map_err(text)?;

    // Writes go through their own thread: a program that isn't reading its input must
    // never be able to block the UI, for example while pasting a lot of text.
    let (input, queued) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        while let Ok(bytes) = queued.recv() {
            if writer
                .write_all(&bytes)
                .and_then(|()| writer.flush())
                .is_err()
            {
                break;
            }
        }
    });
    thread::spawn(move || {
        let mut chunk = [0; READ_CHUNK];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let bytes = chunk[..n].to_vec();
                    if tx.send(Event::PtyOutput { id, bytes }).is_err() {
                        return;
                    }
                }
            }
        }
        let _ = tx.send(Event::PtyExit { id });
    });

    Ok(Term {
        id,
        number,
        parser: vt100::Parser::new_with_callbacks(rows, cols, SCROLLBACK, Hooks::default()),
        input,
        master: pair.master,
        child,
        size,
    })
}

pub struct Terminals {
    pub tabs: Vec<Term>,
    pub active: usize,
    next_id: u64,
    next_number: u64,
    root: PathBuf,
    tx: SyncSender<Event>,
    started: bool,
    pub scroll_mode: bool,
    last_size: (u16, u16),
    hits: TabHits,
}

impl Terminals {
    pub fn new(root: PathBuf, tx: SyncSender<Event>) -> Terminals {
        Terminals {
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
            next_number: 1,
            root,
            tx,
            started: false,
            scroll_mode: false,
            last_size: (24, 80),
            hits: TabHits::default(),
        }
    }

    pub fn new_tab(&mut self) -> Result<(), String> {
        let term = spawn(
            self.next_id,
            self.next_number,
            &self.root,
            self.last_size,
            self.tx.clone(),
        )?;
        self.next_id += 1;
        self.next_number += 1;
        self.tabs.push(term);
        self.active = self.tabs.len() - 1;
        Ok(())
    }

    pub fn close_tab(&mut self) {
        if self.active < self.tabs.len() {
            self.tabs.remove(self.active);
            self.active = self.active.min(self.tabs.len().saturating_sub(1));
            self.scroll_mode = false;
        }
    }

    pub fn cycle(&mut self, delta: isize) {
        if !self.tabs.is_empty() {
            let count = self.tabs.len() as isize;
            self.active = (self.active as isize + delta).rem_euclid(count) as usize;
            self.scroll_mode = false;
        }
    }

    pub fn on_output(&mut self, id: u64, bytes: &[u8]) {
        if let Some(term) = self.tabs.iter_mut().find(|term| term.id == id) {
            term.feed(bytes);
        }
    }

    pub fn on_exit(&mut self, id: u64) {
        if let Some(index) = self.tabs.iter().position(|term| term.id == id) {
            let _ = self.tabs[index].child.try_wait();
            self.tabs.remove(index);
            self.active = self.active.min(self.tabs.len().saturating_sub(1));
            self.scroll_mode = false;
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        let Some(term) = self.tabs.get_mut(self.active) else {
            return;
        };
        if self.scroll_mode {
            let page = term.size.0 as isize;
            match key.code {
                KeyCode::Up => term.scroll(1),
                KeyCode::Down => term.scroll(-1),
                KeyCode::PageUp => term.scroll(page),
                KeyCode::PageDown => term.scroll(-page),
                KeyCode::Home => term.scroll(SCROLLBACK as isize),
                KeyCode::End => term.scroll(-(SCROLLBACK as isize)),
                KeyCode::Esc | KeyCode::Char('q') => {
                    term.parser.screen_mut().set_scrollback(0);
                    self.scroll_mode = false;
                }
                _ => {}
            }
            return;
        }
        if key.modifiers == KeyModifiers::ALT && key.code == KeyCode::Char('s') {
            self.scroll_mode = true;
            return;
        }
        let bytes = key_to_bytes(key, term.parser.screen().application_cursor());
        if !bytes.is_empty() {
            term.parser.screen_mut().set_scrollback(0);
            term.send(bytes);
        }
    }

    pub fn paste(&mut self, text: &str) {
        let Some(term) = self.tabs.get_mut(self.active) else {
            return;
        };
        // Removes control characters, so pasted text can't end the bracket early and run commands.
        let clean: String = text
            .replace("\r\n", "\r")
            .replace('\n', "\r")
            .chars()
            .filter(|c| !c.is_control() || matches!(c, '\r' | '\t'))
            .collect();
        let bytes = if term.parser.screen().bracketed_paste() {
            format!("\x1b[200~{clean}\x1b[201~")
        } else {
            clean
        };
        term.parser.screen_mut().set_scrollback(0);
        term.send(bytes.into_bytes());
    }

    pub fn on_mouse(&mut self, mouse: MouseEvent) {
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
            && let Some(index) = self.hits.at(mouse.column, mouse.row)
        {
            self.active = index;
            self.scroll_mode = false;
            return;
        }
        let Some(term) = self.tabs.get_mut(self.active) else {
            return;
        };
        let up = match mouse.kind {
            MouseEventKind::ScrollUp => true,
            MouseEventKind::ScrollDown => false,
            _ => return,
        };
        if term.parser.screen().alternate_screen() {
            // Full-screen programs like less have no scrollback, so the wheel moves them instead.
            let code = if up { KeyCode::Up } else { KeyCode::Down };
            let arrow = key_to_bytes(
                KeyEvent::new(code, KeyModifiers::NONE),
                term.parser.screen().application_cursor(),
            );
            term.send(arrow.repeat(3));
        } else {
            term.scroll(if up { 3 } else { -3 });
        }
    }

    pub fn hints(&self) -> &'static str {
        if self.scroll_mode {
            SCROLL_HINTS
        } else {
            HINTS
        }
    }

    /// Returns an error to show if the first shell could not start.
    pub fn draw(&mut self, frame: &mut Frame, area: Rect, focused: bool) -> Option<String> {
        if area.height < 2 || area.width == 0 {
            return None;
        }
        let body = Rect {
            y: area.y + 1,
            height: area.height - 1,
            ..area
        };
        self.last_size = (body.height, body.width);
        let mut error = None;
        if !self.started {
            self.started = true;
            error = self
                .new_tab()
                .err()
                .map(|err| format!("Can't start a shell: {err}"));
        }
        if self.tabs.is_empty() {
            let hint = Paragraph::new("No terminal open. Press Alt+T for a new one.")
                .style(Style::new().fg(Color::DarkGray));
            frame.render_widget(hint, area);
            return error;
        }
        let titles: Vec<String> = self.tabs.iter().map(Term::title).collect();
        let (tabs, hits) = tab_line(&titles, self.active, area);
        self.hits = hits;
        frame.render_widget(Paragraph::new(tabs), Rect { height: 1, ..area });

        let term = &mut self.tabs[self.active];
        term.resize(body.height, body.width);
        let screen = term.parser.screen();
        // The widget draws its own cursor block. The real one looks right in every terminal.
        let mut hidden = Cursor::default();
        hidden.hide();
        frame.render_widget(PseudoTerminal::new(screen).cursor(hidden), body);
        if focused && !screen.hide_cursor() && screen.scrollback() == 0 && !self.scroll_mode {
            let (row, col) = screen.cursor_position();
            frame.set_cursor_position((
                body.x + col.min(body.width - 1),
                body.y + row.min(body.height - 1),
            ));
        }
        error
    }
}

/// The bytes a terminal sends for a key press.
pub fn key_to_bytes(key: KeyEvent, application_cursor: bool) -> Vec<u8> {
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    // xterm's modifier parameter: 1 + shift + 2*alt + 4*ctrl.
    let modifier = 1 + shift as u8 + 2 * alt as u8 + 4 * ctrl as u8;
    let csi_letter = |letter: char| -> Vec<u8> {
        if modifier > 1 {
            format!("\x1b[1;{modifier}{letter}").into_bytes()
        } else if application_cursor {
            format!("\x1bO{letter}").into_bytes()
        } else {
            format!("\x1b[{letter}").into_bytes()
        }
    };
    let csi_tilde = |number: u8| -> Vec<u8> {
        if modifier > 1 {
            format!("\x1b[{number};{modifier}~").into_bytes()
        } else {
            format!("\x1b[{number}~").into_bytes()
        }
    };
    let with_alt = |mut bytes: Vec<u8>| -> Vec<u8> {
        if alt {
            bytes.insert(0, 0x1b);
        }
        bytes
    };
    match key.code {
        KeyCode::Char(c) if ctrl => {
            let control = match c.to_ascii_lowercase() {
                c @ 'a'..='z' => c as u8 - b'a' + 1,
                ' ' | '@' | '2' => 0,
                '[' | '3' => 0x1b,
                '\\' | '4' => 0x1c,
                ']' | '5' => 0x1d,
                '^' | '6' => 0x1e,
                '_' | '-' | '7' => 0x1f,
                '?' | '8' => 0x7f,
                _ => return Vec::new(),
            };
            with_alt(vec![control])
        }
        KeyCode::Char(c) => with_alt(c.to_string().into_bytes()),
        KeyCode::Enter => with_alt(vec![b'\r']),
        KeyCode::Tab => with_alt(vec![b'\t']),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace if ctrl => with_alt(vec![0x08]),
        KeyCode::Backspace => with_alt(vec![0x7f]),
        KeyCode::Esc => with_alt(vec![0x1b]),
        KeyCode::Up => csi_letter('A'),
        KeyCode::Down => csi_letter('B'),
        KeyCode::Right => csi_letter('C'),
        KeyCode::Left => csi_letter('D'),
        KeyCode::Home => csi_letter('H'),
        KeyCode::End => csi_letter('F'),
        KeyCode::Insert => csi_tilde(2),
        KeyCode::Delete => csi_tilde(3),
        KeyCode::PageUp => csi_tilde(5),
        KeyCode::PageDown => csi_tilde(6),
        KeyCode::F(n @ 1..=4) => {
            let letter = (b'P' + n - 1) as char;
            if modifier > 1 {
                format!("\x1b[1;{modifier}{letter}").into_bytes()
            } else {
                format!("\x1bO{letter}").into_bytes()
            }
        }
        KeyCode::F(n @ 5..=12) => csi_tilde([15, 17, 18, 19, 20, 21, 23, 24][n as usize - 5]),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Vec<u8> {
        key_to_bytes(KeyEvent::new(code, modifiers), false)
    }

    #[test]
    fn keys_map_to_the_bytes_a_terminal_sends() {
        const NONE: KeyModifiers = KeyModifiers::NONE;
        const CTRL: KeyModifiers = KeyModifiers::CONTROL;
        const ALT: KeyModifiers = KeyModifiers::ALT;
        const SHIFT: KeyModifiers = KeyModifiers::SHIFT;
        assert_eq!(key(KeyCode::Char('a'), NONE), b"a");
        assert_eq!(key(KeyCode::Char('é'), NONE), "é".as_bytes());
        assert_eq!(key(KeyCode::Char('c'), CTRL), [3]);
        assert_eq!(key(KeyCode::Char('w'), CTRL), [0x17]);
        assert_eq!(key(KeyCode::Char('C'), CTRL), [3]);
        assert_eq!(key(KeyCode::Char(' '), CTRL), [0]);
        assert_eq!(key(KeyCode::Char('b'), ALT), b"\x1bb");
        assert_eq!(key(KeyCode::Enter, NONE), b"\r");
        assert_eq!(key(KeyCode::Backspace, NONE), [0x7f]);
        assert_eq!(key(KeyCode::Backspace, ALT), b"\x1b\x7f");
        assert_eq!(key(KeyCode::Esc, NONE), [0x1b]);
        assert_eq!(key(KeyCode::Up, NONE), b"\x1b[A");
        assert_eq!(key(KeyCode::Left, CTRL), b"\x1b[1;5D");
        assert_eq!(key(KeyCode::Right, SHIFT), b"\x1b[1;2C");
        assert_eq!(key(KeyCode::Delete, NONE), b"\x1b[3~");
        assert_eq!(key(KeyCode::PageUp, SHIFT), b"\x1b[5;2~");
        assert_eq!(key(KeyCode::F(1), NONE), b"\x1bOP");
        assert_eq!(key(KeyCode::F(5), NONE), b"\x1b[15~");
        assert_eq!(key(KeyCode::F(12), NONE), b"\x1b[24~");
        assert_eq!(
            key_to_bytes(KeyEvent::new(KeyCode::Up, NONE), true),
            b"\x1bOA"
        );
    }

    fn parser() -> vt100::Parser<Hooks> {
        vt100::Parser::new_with_callbacks(24, 80, 0, Hooks::default())
    }

    #[test]
    fn terminal_queries_get_answers_even_when_split_across_reads() {
        let mut p = parser();
        p.process(b"\x1b[3;7H\x1b[");
        p.process(b"6n");
        assert_eq!(p.callbacks().reply, b"\x1b[3;7R");

        let mut p = parser();
        p.process(b"\x1b[c\x1b[>c\x1b[5n");
        assert_eq!(p.callbacks().reply, b"\x1b[?62;22c\x1b[>1;10;0c\x1b[0n");
    }

    #[test]
    fn window_titles_are_stripped_of_control_characters() {
        let mut p = parser();
        p.process(b"\x1b]2;hello\x07");
        assert_eq!(p.callbacks().title.as_deref(), Some("hello"));
    }

    /// Spawns the real shell and reads until `wanted` is on screen.
    fn wait_for(terms: &mut Terminals, rx: &mpsc::Receiver<Event>, wanted: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Event::PtyOutput { id, bytes }) => terms.on_output(id, &bytes),
                Ok(Event::PtyExit { id }) => terms.on_exit(id),
                _ => {}
            }
            if terms
                .tabs
                .first()
                .is_some_and(|t| t.parser.screen().contents().contains(wanted))
            {
                return true;
            }
        }
        false
    }

    fn type_line(terms: &mut Terminals, text: &str) {
        for c in text.chars() {
            terms.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        terms.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    }

    #[test]
    fn real_shell_runs_commands_and_exiting_closes_the_tab() {
        // SAFETY: this is the only test that reads SHELL, and it sets it before spawning.
        unsafe { std::env::set_var("SHELL", "/bin/sh") };
        let (tx, rx) = mpsc::sync_channel(64);
        let mut terms = Terminals::new(std::env::temp_dir(), tx);
        terms.new_tab().unwrap();
        type_line(&mut terms, "echo $((40+2))");
        assert!(
            wait_for(&mut terms, &rx, "42"),
            "command output should reach the screen"
        );

        terms.tabs[0].resize(10, 40);
        type_line(&mut terms, "stty size");
        assert!(
            wait_for(&mut terms, &rx, "10 40"),
            "the shell should see the new size"
        );

        type_line(&mut terms, "exit");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !terms.tabs.is_empty() && Instant::now() < deadline {
            if let Ok(Event::PtyExit { id }) = rx.recv_timeout(Duration::from_millis(100)) {
                terms.on_exit(id);
            }
        }
        assert!(terms.tabs.is_empty(), "shell exit should close its tab");
    }

    #[test]
    fn pasted_text_cannot_break_out_of_bracketed_paste() {
        let (tx, _rx) = mpsc::sync_channel(64);
        let (input, sent) = mpsc::channel();
        let mut terms = Terminals::new(std::env::temp_dir(), tx);
        terms.tabs.push(fake_term(input));
        terms.tabs[0].parser.process(b"\x1b[?2004h");
        terms.paste("safe\x1b[201~rm -rf /\nnext");
        let bytes = sent.try_recv().unwrap();
        assert_eq!(bytes, b"\x1b[200~safe[201~rm -rf /\rnext\x1b[201~");
    }

    fn fake_term(input: Sender<Vec<u8>>) -> Term {
        let pair = native_pty_system().openpty(PtySize::default()).unwrap();
        let child = pair
            .slave
            .spawn_command(CommandBuilder::new("/bin/true"))
            .unwrap();
        Term {
            id: 1,
            number: 1,
            parser: vt100::Parser::new_with_callbacks(24, 80, 100, Hooks::default()),
            input,
            master: pair.master,
            child,
            size: (24, 80),
        }
    }
}
