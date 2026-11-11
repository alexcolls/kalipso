use std::fs;
use std::path::PathBuf;
use std::sync::mpsc::SyncSender;
use std::thread;

use crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::Frame;
use ratatui::buffer::Buffer as ScreenBuffer;
use ratatui::layout::{Constraint, Layout, Margin, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::chat::{AGENTS, ChatCmd, ChatEvent, Chats, EditorContext, ModelsFor, Resumable};
use crate::editor::{self, Buffer, Editor, EditorCmd, SELECTION_BG, col_to_display, render_line};
use crate::files::{self, FileTree, FilesCmd, safe_join};
use crate::git;
use crate::llm::{ModelInfo, Provider};
use crate::lsp::{self, LspEvent, SERVERS, Servers};
use crate::term::Terminals;
use crate::tools;
use crate::vendors::{VENDORS, Vendor};

/// The most selected text sent along with a chat message.
const MAX_CONTEXT_BYTES: usize = 32 * 1024;
const MAX_SEARCH_HITS: usize = 1000;
/// Seconds between checks of git's state.
const GIT_CHECK_EVERY: u32 = 2;

/// Everything the main thread reacts to. Worker threads only ever send these.
pub enum Event {
    Input(TermEvent),
    Tick,
    PtyOutput {
        id: u64,
        bytes: Vec<u8>,
    },
    PtyExit {
        id: u64,
    },
    Chat {
        chat: u64,
        generation: u64,
        event: ChatEvent,
    },
    Models {
        provider: Provider,
        purpose: ModelsFor,
        result: Result<Vec<ModelInfo>, String>,
    },
    Git(Option<git::Snapshot>),
    Lsp {
        server: usize,
        message: serde_json::Value,
    },
    LspExit {
        server: usize,
        reason: String,
    },
    SearchDone {
        query: String,
        hits: Vec<tools::SearchHit>,
        more: bool,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Files,
    Terminal,
    Editor,
    Chat,
}

impl Focus {
    fn title(self) -> &'static str {
        match self {
            Focus::Files => " 1 Files ",
            Focus::Terminal => " 2 Terminal ",
            Focus::Editor => " 3 Editor ",
            Focus::Chat => " 4 Chat ",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

/// Where a directional move lands. `middle` is the Editor or Terminal, whichever
/// was focused last, so moving out of the middle column and back is reversible.
pub fn neighbor(from: Focus, dir: Dir, middle: Focus) -> Focus {
    match (from, dir) {
        (Focus::Files, Dir::Right) | (Focus::Chat, Dir::Left) => middle,
        (Focus::Editor | Focus::Terminal, Dir::Left) => Focus::Files,
        (Focus::Editor | Focus::Terminal, Dir::Right) => Focus::Chat,
        (Focus::Editor, Dir::Down) => Focus::Terminal,
        (Focus::Terminal, Dir::Up) => Focus::Editor,
        (same, _) => same,
    }
}

/// Screen regions. A pane is an empty `Rect` when another pane is zoomed.
#[derive(Clone, Copy, Default, Debug)]
pub struct Areas {
    pub files: Rect,
    pub terminal: Rect,
    pub editor: Rect,
    pub chat: Rect,
    pub status: Rect,
}

impl Areas {
    pub fn compute(area: Rect, zoomed: Option<Focus>) -> Areas {
        let [main, status] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(area);
        let mut areas = Areas {
            status,
            ..Areas::default()
        };
        if let Some(pane) = zoomed {
            *areas.get_mut(pane) = main;
            return areas;
        }
        let [files, middle, chat] = Layout::horizontal([
            Constraint::Percentage(18),
            Constraint::Min(0),
            Constraint::Percentage(30),
        ])
        .areas(main);
        let [editor, terminal] =
            Layout::vertical([Constraint::Percentage(65), Constraint::Percentage(35)])
                .areas(middle);
        Areas {
            files,
            terminal,
            editor,
            chat,
            status,
        }
    }

    pub fn get(&self, pane: Focus) -> Rect {
        match pane {
            Focus::Files => self.files,
            Focus::Terminal => self.terminal,
            Focus::Editor => self.editor,
            Focus::Chat => self.chat,
        }
    }

    fn get_mut(&mut self, pane: Focus) -> &mut Rect {
        match pane {
            Focus::Files => &mut self.files,
            Focus::Terminal => &mut self.terminal,
            Focus::Editor => &mut self.editor,
            Focus::Chat => &mut self.chat,
        }
    }

    pub fn pane_at(&self, column: u16, row: u16) -> Option<Focus> {
        let pos = Position::new(column, row);
        [Focus::Files, Focus::Terminal, Focus::Editor, Focus::Chat]
            .into_iter()
            .find(|&pane| self.get(pane).contains(pos))
    }
}

/// Clickable tab positions from the last draw.
#[derive(Default)]
pub struct TabHits(Vec<(usize, Rect)>);

impl TabHits {
    pub fn at(&self, column: u16, row: u16) -> Option<usize> {
        let pos = Position::new(column, row);
        self.0
            .iter()
            .find(|(_, rect)| rect.contains(pos))
            .map(|(index, _)| *index)
    }
}

/// A row of tab titles, scrolled so the active one is always visible.
pub fn tab_line(titles: &[String], active: usize, area: Rect) -> (Line<'static>, TabHits) {
    let width_of = |title: &String| title.width() + 2;
    let mut first = 0;
    while first < active
        && titles[first..=active].iter().map(width_of).sum::<usize>() > area.width as usize
    {
        first += 1;
    }
    let mut spans = Vec::new();
    let mut hits = Vec::new();
    let mut x = area.x;
    for (index, title) in titles.iter().enumerate().skip(first) {
        let width = width_of(title) as u16;
        if x + width > area.right() {
            break;
        }
        let style = if index == active {
            Style::new().fg(Color::Black).bg(Color::Gray)
        } else {
            Style::new().fg(Color::Gray)
        };
        spans.push(Span::styled(format!(" {title} "), style));
        hits.push((index, Rect::new(x, area.y, width, 1)));
        x += width;
    }
    (Line::from(spans), TabHits(hits))
}

/// What to do when a popup is answered.
pub enum Then {
    NewFile(PathBuf),
    NewDir(PathBuf),
    Rename(PathBuf),
    Delete(PathBuf),
    SaveAs,
    Overwrite,
    CloseDoc(usize),
    Find,
    GoToLine,
    Quit,
    CloseChat(u64),
    ChatAgent(Vec<Resumable>),
    ChatProvider(usize),
    ChatModel(usize, Provider),
    SwitchModel(u64),
    OpenFile,
    SearchProject,
    OpenMatch(String),
    StartServer(usize),
}

pub enum Modal {
    Input {
        title: String,
        input: Buffer,
        then: Then,
    },
    Confirm {
        title: String,
        then: Then,
    },
    Pick {
        title: String,
        items: Vec<String>,
        filter: String,
        /// Indexes into `items` that match the filter, best first.
        shown: Vec<usize>,
        selected: usize,
        then: Then,
    },
}

impl Modal {
    /// A text prompt with `value` selected, so typing replaces it.
    fn input(title: impl Into<String>, value: &str, then: Then) -> Modal {
        let mut input = Buffer::from_text(value);
        input.edit_key(
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
            1,
            "",
        );
        Modal::Input {
            title: title.into(),
            input,
            then,
        }
    }

    fn pick(title: impl Into<String>, items: Vec<String>, then: Then) -> Modal {
        let shown = (0..items.len()).collect();
        Modal::Pick {
            title: title.into(),
            items,
            filter: String::new(),
            shown,
            selected: 0,
            then,
        }
    }
}

/// How well `query` matches `item`, or `None` if its letters don't appear in order. Ignores
/// case, and favours runs of letters, word starts, the file name, and short items.
fn fuzzy_score(item: &str, query: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let item = item.to_lowercase();
    let query = query.to_lowercase();
    let mut wanted = query.chars().peekable();
    let mut score = 0;
    let mut previous = None;
    let mut previous_matched = false;
    for c in item.chars() {
        let matched = wanted.peek() == Some(&c);
        if matched {
            wanted.next();
            score += 1;
            if previous_matched {
                score += 4;
            }
            if previous.is_none_or(|p: char| !p.is_alphanumeric()) {
                score += 6;
            }
        }
        previous_matched = matched;
        previous = Some(c);
    }
    if wanted.peek().is_some() {
        return None;
    }
    let name_start = item.rfind('/').map_or(0, |slash| slash + 1);
    if item[name_start..].contains(&query) {
        score += 40;
    } else if item.contains(&query) {
        score += 20;
    }
    Some(score * 16 - item.chars().count() as i64)
}

/// Indexes of the items matching `filter`, best first. Ties keep the original order.
fn rank(items: &[String], filter: &str) -> Vec<usize> {
    let mut scored: Vec<(i64, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| fuzzy_score(item, filter).map(|score| (score, index)))
        .collect();
    scored.sort_by_key(|&(score, _)| std::cmp::Reverse(score));
    scored.into_iter().map(|(_, index)| index).collect()
}

/// What to do once a language server the user just okayed is running.
#[derive(Clone, Copy)]
enum Retry {
    Definition,
    Problems,
}

/// A mouse selection over what's on screen in the terminal or chat pane.
struct TextSelection {
    pane: Focus,
    start: (u16, u16),
    end: (u16, u16),
    /// Set when the mouse is released. The next draw reads the text and copies it.
    copy: bool,
}

/// Highlights the text between two screen positions and returns it. It runs like text:
/// to the end of the first row, whole rows, then up to the end position.
fn read_selection(buffer: &mut ScreenBuffer, rect: Rect, a: (u16, u16), b: (u16, u16)) -> String {
    let row_col = |(x, y): (u16, u16)| {
        (
            y.clamp(rect.y, rect.bottom() - 1),
            x.clamp(rect.x, rect.right() - 1),
        )
    };
    let (a, b) = (row_col(a), row_col(b));
    let (start, end) = if a <= b { (a, b) } else { (b, a) };
    let mut text = String::new();
    for row in start.0..=end.0 {
        let from = if row == start.0 { start.1 } else { rect.x };
        let to = if row == end.0 {
            end.1
        } else {
            rect.right() - 1
        };
        let mut line = String::new();
        let mut skip = 0;
        for col in from..=to {
            let cell = &mut buffer[(col, row)];
            cell.set_bg(SELECTION_BG);
            if skip > 0 {
                skip -= 1; // The second half of a wide character.
                continue;
            }
            line.push_str(cell.symbol());
            skip = cell.symbol().width().saturating_sub(1);
        }
        text.push_str(line.trim_end());
        if row != end.0 {
            text.push('\n');
        }
    }
    text
}

pub struct App {
    pub root: PathBuf,
    tx: SyncSender<Event>,
    pub focus: Focus,
    /// Last focused of Editor/Terminal, see [`neighbor`].
    pub middle: Focus,
    pub zoom: bool,
    pub areas: Areas,
    pub needs_draw: bool,
    pub should_quit: bool,
    pub files: FileTree,
    pub editor: Editor,
    pub terminals: Terminals,
    pub chats: Chats,
    pub servers: Servers,
    pub modal: Option<Modal>,
    /// Shown in the status line until the next key press.
    pub message: Option<String>,
    pub clipboard: String,
    selection: Option<TextSelection>,
    last_search: String,
    ticks: u32,
    git_running: bool,
    pending_retry: Option<Retry>,
    /// Where the user was when they asked for a definition.
    definition_asked: Option<(usize, u64, editor::Pos)>,
}

impl App {
    pub fn new(root: PathBuf, tx: SyncSender<Event>) -> App {
        App {
            files: FileTree::new(root.clone()),
            editor: Editor::default(),
            terminals: Terminals::new(root.clone(), tx.clone()),
            chats: Chats::new(root.clone(), tx.clone()),
            servers: Servers::new(root.clone(), tx.clone()),
            root,
            tx,
            focus: Focus::Files,
            middle: Focus::Editor,
            zoom: false,
            areas: Areas::default(),
            needs_draw: true,
            should_quit: false,
            modal: None,
            message: None,
            clipboard: String::new(),
            selection: None,
            last_search: String::new(),
            ticks: 0,
            git_running: false,
            pending_retry: None,
            definition_asked: None,
        }
    }

    /// The running server for the active file. If there isn't one, this explains why or asks
    /// to start one, since language servers run code from the project they open.
    fn server_for_active_doc(&mut self, retry: Retry) -> Option<usize> {
        let path = self.editor.docs.get(self.editor.active)?.path.clone()?;
        let Some(index) = self.servers.index_for(&path) else {
            let names = lsp::server_names_for(&path);
            self.message = Some(if names.is_empty() {
                "No language server is known for this kind of file.".to_owned()
            } else {
                format!(
                    "No language server found. Install {} to use this.",
                    names.join(" or ")
                )
            });
            return None;
        };
        if self.servers.is_running(index) {
            return Some(index);
        }
        self.pending_retry = Some(retry);
        let title = format!(
            "Start {}? It can run code from this project.",
            SERVERS[index].name
        );
        self.modal = Some(Modal::Confirm {
            title,
            then: Then::StartServer(index),
        });
        None
    }

    fn go_to_definition(&mut self) {
        if let Some(index) = self.server_for_active_doc(Retry::Definition) {
            self.servers.sync(&self.editor.docs);
            let doc = &self.editor.docs[self.editor.active];
            self.servers.definition(index, doc);
            self.message = Some("Looking for the definition...".to_owned());
            self.definition_asked = self.editor_position();
        }
    }

    /// Where the user is in the editor, or `None` if they're not there.
    fn editor_position(&self) -> Option<(usize, u64, editor::Pos)> {
        if self.focus != Focus::Editor || self.modal.is_some() {
            return None;
        }
        let doc = self.editor.docs.get(self.editor.active)?;
        Some((self.editor.active, doc.buf.revision(), doc.buf.cursor))
    }

    fn next_problem(&mut self) {
        if self.server_for_active_doc(Retry::Problems).is_some() {
            self.message = Some(self.editor.next_problem());
        }
    }

    fn start_server(&mut self, index: usize) {
        let name = SERVERS[index].name;
        if let Err(err) = self.servers.start(index) {
            self.message = Some(format!("Can't start {name}: {err}"));
        }
    }

    /// Starts servers this project has okayed, and keeps all of them up to date.
    fn tend_servers(&mut self) {
        for path in self
            .editor
            .docs
            .iter()
            .filter(|doc| !doc.is_proposal())
            .filter_map(|doc| doc.path.clone())
            .collect::<Vec<_>>()
        {
            if let Some(index) = self.servers.index_for(&path)
                && !self.servers.is_running(index)
                && !self.servers.has_failed(index)
                && self.chats.trusts_server(SERVERS[index].name)
            {
                self.start_server(index);
            }
        }
        self.servers.sync(&self.editor.docs);
    }

    fn on_lsp(&mut self, server: usize, message: serde_json::Value) {
        match self.servers.on_message(server, message) {
            LspEvent::None => {}
            LspEvent::Diagnostics { path, diagnostics } => {
                self.editor.diagnostics.insert(path, diagnostics);
                self.needs_draw = true;
            }
            LspEvent::NoDefinition(working_on) => {
                self.message = Some(match working_on {
                    Some(work) => format!(
                        "Nothing found yet, but {} is still working ({work}). Try again soon.",
                        SERVERS[server].name
                    ),
                    None => "No definition found.".to_owned(),
                });
                self.needs_draw = true;
            }
            LspEvent::Problem(problem) => {
                self.message = Some(format!("{}: {problem}", SERVERS[server].name));
                self.needs_draw = true;
            }
            LspEvent::Definition {
                path,
                line,
                character,
                utf8,
            } => {
                self.needs_draw = true;
                self.message = None;
                // A slow server can answer long after the question. If the user has moved on,
                // jumping would take the cursor, or even their keystrokes, somewhere unexpected.
                if self.definition_asked.take() != self.editor_position() {
                    return;
                }
                let column = |text: &str| lsp::to_byte_column(text, character, utf8);
                if let Err(err) = self.editor.jump_to(&path, line, column) {
                    self.message = Some(format!("Can't open {}: {err}", self.display_path(&path)));
                }
            }
        }
    }

    /// Checks git's state on a worker thread, one check at a time.
    fn check_git(&mut self) {
        if self.git_running {
            return;
        }
        self.git_running = true;
        let (tx, root, known) = (
            self.tx.clone(),
            self.root.clone(),
            self.editor.known_bases(),
        );
        thread::spawn(move || {
            let _ = tx.send(Event::Git(git::snapshot(&root, &known)));
        });
    }

    fn on_git(&mut self, snapshot: Option<git::Snapshot>) {
        self.git_running = false;
        let Some(snapshot) = snapshot else {
            self.needs_draw |= self.files.set_changes(Default::default());
            return;
        };
        self.needs_draw |= !snapshot.bases.is_empty();
        self.editor.apply_bases(&snapshot.head, &snapshot.bases);
        self.needs_draw |= self.files.set_changes(snapshot.changes);
    }

    fn search_prompt(&mut self) {
        let selected = self
            .editor
            .doc()
            .and_then(|doc| doc.buf.selected_text())
            .filter(|text| !text.contains('\n'));
        let query = selected.unwrap_or_else(|| self.last_search.clone());
        self.modal = Some(Modal::input("Search project", &query, Then::SearchProject));
    }

    fn start_search(&mut self, query: &str) {
        self.last_search = query.to_owned();
        let Some(pattern) = editor::literal_regex(query) else {
            return;
        };
        self.message = Some(format!("Searching for {query}..."));
        let (tx, root, query) = (self.tx.clone(), self.root.clone(), query.to_owned());
        thread::spawn(move || {
            let (hits, more) = tools::search_files(&root, &root, &pattern, MAX_SEARCH_HITS);
            let _ = tx.send(Event::SearchDone { query, hits, more });
        });
    }

    fn on_search_done(&mut self, query: String, hits: Vec<tools::SearchHit>, more: bool) {
        self.needs_draw = true;
        self.message = None;
        if hits.is_empty() {
            self.message = Some(format!("No matches for {query}."));
        } else if self.modal.is_none() {
            let title = if more {
                format!("First {} matches for {query}", hits.len())
            } else {
                format!(
                    "{} match{} for {query}",
                    hits.len(),
                    if hits.len() == 1 { "" } else { "es" }
                )
            };
            let items = hits
                .iter()
                .map(|hit| {
                    format!(
                        "{}:{}: {}",
                        hit.path,
                        hit.line,
                        hit.text.trim().replace('\t', " ")
                    )
                })
                .collect();
            self.modal = Some(Modal::pick(title, items, Then::OpenMatch(query)));
        }
    }

    pub fn handle(&mut self, event: Event) {
        match event {
            Event::Input(TermEvent::Key(key)) if key.kind != KeyEventKind::Release => {
                self.on_key(key)
            }
            Event::Input(TermEvent::Mouse(mouse)) => self.on_mouse(mouse),
            Event::Input(TermEvent::Paste(text)) => self.on_paste(&text),
            Event::Input(TermEvent::Resize(..)) => self.needs_draw = true,
            Event::Input(_) => {}
            Event::Tick => {
                let tree = self.files.check_disk();
                let docs = self.editor.check_disk();
                self.needs_draw |= tree || docs;
                self.tend_servers();
                if self.ticks.is_multiple_of(GIT_CHECK_EVERY) {
                    self.check_git();
                }
                self.ticks = self.ticks.wrapping_add(1);
            }
            Event::Git(snapshot) => self.on_git(snapshot),
            Event::Lsp { server, message } => self.on_lsp(server, message),
            Event::LspExit { server, reason } => {
                if self.servers.on_exit(server) {
                    let why = if reason.is_empty() {
                        String::new()
                    } else {
                        format!(": {reason}")
                    };
                    self.message = Some(format!("{} stopped{why}", SERVERS[server].name));
                    self.needs_draw = true;
                }
            }
            Event::SearchDone { query, hits, more } => self.on_search_done(query, hits, more),
            Event::PtyOutput { id, bytes } => {
                if !self.chats.on_pty_output(id, &bytes) {
                    self.terminals.on_output(id, &bytes);
                }
                self.needs_draw = true;
            }
            Event::PtyExit { id } => {
                self.chats.on_pty_exit(id);
                self.terminals.on_exit(id);
                self.needs_draw = true;
            }
            Event::Chat {
                chat,
                generation,
                event,
            } => {
                if self.chats.on_event(chat, generation, event) {
                    // Show what the agent just did without waiting for the next tick.
                    self.files.refresh();
                    self.editor.check_disk();
                }
                self.needs_draw = true;
            }
            Event::Models {
                provider,
                purpose,
                result,
            } => self.on_models(provider, purpose, result),
        }
        let chats = &self.chats;
        self.editor
            .retain_proposals(|chat, call| chats.approving_call(chat) == Some(call));
    }

    fn set_focus(&mut self, pane: Focus) {
        self.focus = pane;
        if matches!(pane, Focus::Editor | Focus::Terminal) {
            self.middle = pane;
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        self.needs_draw = true;
        self.message = None;
        self.selection = None;
        if self.modal.is_some() {
            self.on_modal_key(key);
        } else if !self.on_global_key(key) {
            match self.focus {
                Focus::Files => {
                    let cmd = self.files.on_key(key);
                    self.run_files_cmd(cmd);
                }
                Focus::Editor => {
                    if !self.answer_proposal(key) {
                        let cmd = self.editor.on_key(key, &self.clipboard);
                        self.run_editor_cmd(cmd);
                    }
                }
                Focus::Terminal => self.terminals.on_key(key),
                Focus::Chat => {
                    let sending = key.code == KeyCode::Enter && key.modifiers.is_empty();
                    let context = if sending { self.editor_context() } else { None };
                    let cmd = self.chats.on_key(key, &self.clipboard, context);
                    self.run_chat_cmd(cmd);
                }
            }
        }
    }

    /// In a proposal tab, y, n and a answer the agent's question and Esc goes back to the chat.
    fn answer_proposal(&mut self, key: KeyEvent) -> bool {
        let Some(chat) = self.editor.active_proposal() else {
            return false;
        };
        if key.code == KeyCode::Esc {
            self.set_focus(Focus::Chat);
            return true;
        }
        if !key.modifiers.difference(KeyModifiers::SHIFT).is_empty() {
            return false;
        }
        let KeyCode::Char(c) = key.code else {
            return false;
        };
        let answer = c.to_ascii_lowercase();
        if !matches!(answer, 'y' | 'n' | 'a') {
            return false;
        }
        self.chats.answer(chat, answer);
        self.set_focus(Focus::Chat);
        true
    }

    /// What's open in the editor, so "this file" means something to an agent. Selected text
    /// from private files stays out: only the agent's read tool, which asks first, may send those.
    fn editor_context(&self) -> Option<EditorContext> {
        let doc = self.editor.docs.get(self.editor.active)?;
        let line = doc.buf.cursor.line + 1;
        let (name, private) = match &doc.path {
            Some(path) => {
                let shown = self.display_path(path);
                let private = tools::resolve(&self.root, &shown)
                    .map_or(true, |real| tools::is_sensitive(&self.root, &real));
                (shown, private)
            }
            None => ("an untitled buffer".to_owned(), false),
        };
        let mut label = format!("with {name}:{line}");
        let mut for_model = format!(
            "[Editor state, for context only: {name} is open with the cursor on line {line}."
        );
        if doc.buf.is_dirty() {
            for_model.push_str(" It has unsaved changes, so the file on disk differs.");
        }
        if let (Some((start, end)), Some(selected)) = (doc.buf.selection(), doc.buf.selected_text())
        {
            let count = end.line - start.line + 1;
            if private {
                label.push_str(", selection not sent because this file may be private");
            } else {
                let mut cut = selected.len().min(MAX_CONTEXT_BYTES);
                while !selected.is_char_boundary(cut) {
                    cut -= 1;
                }
                let clipped = if cut < selected.len() {
                    "\n[The selection is cut off here.]"
                } else {
                    ""
                };
                label.push_str(&format!(", {count} selected lines"));
                for_model.push_str(&format!(
                    "\nSelected lines {}-{}:\n<selection>\n{}{clipped}\n</selection>",
                    start.line + 1,
                    end.line + 1,
                    &selected[..cut]
                ));
            }
        }
        for_model.push(']');
        Some(EditorContext { label, for_model })
    }

    fn run_chat_cmd(&mut self, cmd: ChatCmd) {
        match cmd {
            ChatCmd::None => {}
            ChatCmd::NewChat => self.new_chat(),
            ChatCmd::Copied(text) => self.copy(text),
            ChatCmd::CopiedReply(text) => {
                self.copy(text);
                self.message = Some("Copied the last reply.".to_owned());
            }
            ChatCmd::Message(text) => self.message = Some(text),
            ChatCmd::ViewProposal {
                chat,
                call,
                path,
                current,
                proposed,
            } => {
                self.editor
                    .open_proposal(chat, call, &path, &current, &proposed);
                self.set_focus(Focus::Editor);
            }
            ChatCmd::PickModel(chat, provider) => {
                self.message = Some(format!("Loading {} models...", provider.name()));
                self.chats.load_models(provider, ModelsFor::Switch(chat));
            }
        }
    }

    fn new_chat(&mut self) {
        let saved = self.chats.resumable();
        let vendors = VENDORS.iter().map(Vendor::label);
        let agents = AGENTS
            .iter()
            .map(|agent| format!("{}: {}", agent.name, agent.about));
        let items = vendors
            .chain(agents)
            .chain(saved.iter().map(|chat| chat.label.clone()))
            .collect();
        self.modal = Some(Modal::pick(
            "New chat: pick an agent",
            items,
            Then::ChatAgent(saved),
        ));
    }

    fn on_models(
        &mut self,
        provider: Provider,
        purpose: ModelsFor,
        result: Result<Vec<ModelInfo>, String>,
    ) {
        self.needs_draw = true;
        let models = match result {
            Ok(models) => models,
            Err(err) => {
                // Only an optimisation: without this, requests use API defaults.
                if purpose != ModelsFor::Features {
                    self.message = Some(format!("Can't list {} models: {err}", provider.name()));
                }
                return;
            }
        };
        self.chats.remember_models(models.clone());
        if purpose == ModelsFor::Features {
            return;
        }
        self.message = None;
        if models.is_empty() {
            self.message = Some(format!("{} offered no models.", provider.name()));
            return;
        }
        let then = match purpose {
            ModelsFor::NewChat(agent) => Then::ChatModel(agent, provider),
            ModelsFor::Switch(chat) => Then::SwitchModel(chat),
            ModelsFor::Features => return,
        };
        let items = models.into_iter().map(|model| model.id).collect();
        self.modal = Some(Modal::pick(
            format!("{} model", provider.name()),
            items,
            then,
        ));
    }

    fn load_model_limits(&self, provider: Provider) {
        // These two publish model limits. OpenAI's and Grok's lists don't.
        if matches!(provider, Provider::Claude | Provider::OpenRouter) {
            self.chats.load_models(provider, ModelsFor::Features);
        }
    }

    fn create_chat(&mut self, agent: usize, provider: Provider, model: String) {
        self.load_model_limits(provider);
        self.chats.create(agent, provider, model);
        self.set_focus(Focus::Chat);
    }

    fn find_file(&mut self) {
        let items = files::all_files(&self.root);
        if items.is_empty() {
            self.message = Some("No files found in this project.".to_owned());
        } else {
            self.modal = Some(Modal::pick("Open file", items, Then::OpenFile));
        }
    }

    /// Keys handled before the focused pane sees them, including the terminal pane.
    /// Whether a program in a terminal has focus: a shell, or a vendor's CLI in the chat pane.
    fn program_focused(&self) -> bool {
        self.focus == Focus::Terminal || (self.focus == Focus::Chat && self.chats.vendor_active())
    }

    fn on_global_key(&mut self, key: KeyEvent) -> bool {
        // Not where a program has focus, as Ctrl+P belongs to it.
        if key.modifiers == KeyModifiers::CONTROL
            && key.code == KeyCode::Char('p')
            && !self.program_focused()
        {
            self.find_file();
            return true;
        }
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('q') {
            let worry = match (self.editor.has_unsaved(), self.chats.any_busy()) {
                (true, true) => {
                    Some("Unsaved changes will be lost and running agents stopped. Quit?")
                }
                (true, false) => Some("Unsaved changes will be lost. Quit?"),
                (false, true) => Some("Running agents will be stopped. Quit?"),
                (false, false) => None,
            };
            match worry {
                Some(title) => {
                    self.modal = Some(Modal::Confirm {
                        title: title.into(),
                        then: Then::Quit,
                    })
                }
                None => self.quit(),
            }
            return true;
        }
        if key.modifiers != KeyModifiers::ALT {
            return false;
        }
        let dir = match key.code {
            KeyCode::Left => Some(Dir::Left),
            KeyCode::Right => Some(Dir::Right),
            KeyCode::Up => Some(Dir::Up),
            KeyCode::Down => Some(Dir::Down),
            _ => None,
        };
        if let Some(dir) = dir {
            self.set_focus(neighbor(self.focus, dir, self.middle));
            return true;
        }
        match key.code {
            KeyCode::Char('1') => self.set_focus(Focus::Files),
            KeyCode::Char('2') => self.set_focus(Focus::Terminal),
            KeyCode::Char('3') => self.set_focus(Focus::Editor),
            KeyCode::Char('4') => self.set_focus(Focus::Chat),
            KeyCode::Char('z') => self.zoom = !self.zoom,
            // Not where a program has focus, as Alt+F belongs to it.
            KeyCode::Char('f') if !self.program_focused() => self.search_prompt(),
            KeyCode::Char('t') => self.new_tab(),
            KeyCode::Char('w') => self.close_tab(),
            KeyCode::PageUp => self.cycle_tab(-1),
            KeyCode::PageDown => self.cycle_tab(1),
            _ => return false,
        }
        true
    }

    fn quit(&mut self) {
        self.chats.stop_all();
        self.should_quit = true;
    }

    fn new_tab(&mut self) {
        match self.focus {
            Focus::Editor => self.editor.new_untitled(),
            Focus::Terminal => {
                if let Err(err) = self.terminals.new_tab() {
                    self.message = Some(format!("Can't start a shell: {err}"));
                }
            }
            Focus::Chat => self.new_chat(),
            Focus::Files => {}
        }
    }

    fn close_tab(&mut self) {
        match self.focus {
            Focus::Editor => {
                let index = self.editor.active;
                if self.editor.doc().is_some_and(|doc| doc.buf.is_dirty()) {
                    self.modal = Some(Modal::Confirm {
                        title: "Discard unsaved changes in this tab?".into(),
                        then: Then::CloseDoc(index),
                    });
                } else {
                    self.editor.close(index);
                }
            }
            Focus::Terminal => self.terminals.close_tab(),
            Focus::Chat if self.chats.vendor_active() => self.chats.close_vendor(),
            Focus::Chat => {
                let Some(chat) = self.chats.active_chat() else {
                    return;
                };
                let (id, busy) = (chat.id, chat.is_busy());
                if busy {
                    self.modal = Some(Modal::Confirm {
                        title: "This agent is still working. Stop it and close?".into(),
                        then: Then::CloseChat(id),
                    });
                } else {
                    self.chats.close(id);
                }
            }
            Focus::Files => {}
        }
    }

    fn cycle_tab(&mut self, delta: isize) {
        match self.focus {
            Focus::Editor => self.editor.cycle(delta),
            Focus::Terminal => self.terminals.cycle(delta),
            Focus::Chat => self.chats.cycle(delta),
            Focus::Files => {}
        }
    }

    fn open_file(&mut self, path: &std::path::Path) {
        match self.editor.open(path) {
            Ok(()) => self.set_focus(Focus::Editor),
            Err(err) => {
                self.message = Some(format!("Can't open {}: {err}", self.display_path(path)))
            }
        }
    }

    fn display_path(&self, path: &std::path::Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .display()
            .to_string()
    }

    fn run_files_cmd(&mut self, cmd: FilesCmd) {
        let modal = match cmd {
            FilesCmd::None => return,
            FilesCmd::Open(path) => return self.open_file(&path),
            FilesCmd::NewFile(dir) => Modal::input(
                format!("New file in {}/", self.display_path(&dir)),
                "",
                Then::NewFile(dir),
            ),
            FilesCmd::NewDir(dir) => Modal::input(
                format!("New folder in {}/", self.display_path(&dir)),
                "",
                Then::NewDir(dir),
            ),
            FilesCmd::Rename(path) => {
                let name = path
                    .file_name()
                    .map_or(String::new(), |n| n.to_string_lossy().into_owned());
                Modal::input(format!("Rename {name} to"), &name, Then::Rename(path))
            }
            FilesCmd::Delete(path) => {
                let name = self.display_path(&path);
                let is_dir = fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir());
                let title = if is_dir {
                    format!(
                        "Delete folder {name} and the {} items inside?",
                        files::count_inside(&path)
                    )
                } else {
                    format!("Delete {name}?")
                };
                Modal::Confirm {
                    title,
                    then: Then::Delete(path),
                }
            }
        };
        self.modal = Some(modal);
    }

    fn run_editor_cmd(&mut self, cmd: EditorCmd) {
        match cmd {
            EditorCmd::None => {}
            EditorCmd::Message(text) => self.message = Some(text),
            EditorCmd::Definition => self.go_to_definition(),
            EditorCmd::NextProblem => self.next_problem(),
            EditorCmd::Copied(text) => self.copy(text),
            EditorCmd::SaveAs => {
                self.modal = Some(Modal::input(
                    "Save as (relative to project)",
                    "",
                    Then::SaveAs,
                ))
            }
            EditorCmd::ConfirmOverwrite => {
                self.modal = Some(Modal::Confirm {
                    title: "File changed on disk. Overwrite it with your version?".into(),
                    then: Then::Overwrite,
                });
            }
            EditorCmd::GoToLine => {
                self.modal = Some(Modal::input("Go to line", "", Then::GoToLine))
            }
            EditorCmd::Find => {
                let selected = self
                    .editor
                    .doc()
                    .and_then(|doc| doc.buf.selected_text())
                    .filter(|text| !text.contains('\n'));
                let query = selected.unwrap_or_else(|| self.editor.find_query.clone());
                self.modal = Some(Modal::input("Find", &query, Then::Find));
            }
        }
    }

    /// Copies to the internal clipboard and, through the terminal, the system one.
    fn copy(&mut self, text: String) {
        #[cfg(not(test))] // Running the tests shouldn't overwrite the clipboard.
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::clipboard::CopyToClipboard::to_clipboard_from(&text)
        );
        self.clipboard = text;
    }

    fn on_modal_key(&mut self, key: KeyEvent) {
        let Some(modal) = self.modal.as_mut() else {
            return;
        };
        match modal {
            Modal::Confirm { .. } => match key.code {
                KeyCode::Char('y' | 'Y') => {
                    if let Some(Modal::Confirm { then, .. }) = self.modal.take() {
                        self.finish(then, String::new());
                    }
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => self.modal = None,
                _ => {}
            },
            Modal::Input { input, .. } => match key.code {
                KeyCode::Esc => self.modal = None,
                KeyCode::Enter => {
                    if let Some(Modal::Input { input, then, .. }) = self.modal.take() {
                        self.finish(then, input.text());
                    }
                }
                _ => {
                    input.edit_key(key, 1, &self.clipboard.replace('\n', " "));
                }
            },
            Modal::Pick {
                items,
                filter,
                shown,
                selected,
                ..
            } => {
                let last = shown.len().saturating_sub(1);
                match key.code {
                    KeyCode::Esc => self.modal = None,
                    KeyCode::Up => *selected = selected.saturating_sub(1),
                    KeyCode::Down => *selected = (*selected + 1).min(last),
                    KeyCode::PageUp => *selected = selected.saturating_sub(10),
                    KeyCode::PageDown => *selected = (*selected + 10).min(last),
                    KeyCode::Backspace => {
                        filter.pop();
                        *shown = rank(items, filter);
                        *selected = 0;
                    }
                    KeyCode::Char(c)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        filter.push(c);
                        *shown = rank(items, filter);
                        *selected = 0;
                    }
                    KeyCode::Enter => {
                        let choice = shown.get(*selected).map(|&index| items[index].clone());
                        if let Some(choice) = choice
                            && let Some(Modal::Pick { then, .. }) = self.modal.take()
                        {
                            self.finish(then, choice);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// Runs an answered popup. Failures keep the user's data and report in the status line.
    fn finish(&mut self, then: Then, value: String) {
        if let Err(err) = self.try_finish(then, &value) {
            self.message = Some(err);
        }
    }

    fn try_finish(&mut self, then: Then, value: &str) -> Result<(), String> {
        let text = |err: std::io::Error| err.to_string();
        match then {
            Then::Quit => self.quit(),
            Then::StartServer(index) => {
                self.chats
                    .trust_server(SERVERS[index].name)
                    .map_err(|err| format!("Couldn't save that choice: {err}"))?;
                self.start_server(index);
                match self.pending_retry.take() {
                    Some(Retry::Definition) => self.go_to_definition(),
                    Some(Retry::Problems) => {
                        self.message = Some(format!(
                            "{} is starting. F8 will list problems once it has found them.",
                            SERVERS[index].name
                        ))
                    }
                    None => {}
                }
            }
            Then::OpenFile => {
                let path = self.root.join(value);
                self.files.reveal(&path);
                self.open_file(&path);
            }
            Then::SearchProject => {
                if !value.trim().is_empty() {
                    self.start_search(value);
                }
            }
            Then::OpenMatch(query) => {
                let pattern =
                    regex::Regex::new(r"^(.*?):(\d+): ").map_err(|err| err.to_string())?;
                let found = pattern
                    .captures(value)
                    .ok_or("That match can't be opened")?;
                let line = found[2].parse().map_err(|_| "Bad line number")?;
                let path = self.root.join(&found[1]);
                self.files.reveal(&path);
                self.open_file(&path);
                self.editor.find_query = query;
                self.editor.go_to_line(line);
                let cmd = self.editor.find_next(true);
                self.run_editor_cmd(cmd);
            }
            Then::CloseDoc(index) => self.editor.close(index),
            Then::CloseChat(id) => self.chats.close(id),
            Then::ChatAgent(saved) => {
                if let Some(chat) = saved.iter().find(|chat| chat.label == value) {
                    let provider = self.chats.resume(&chat.path)?;
                    self.load_model_limits(provider);
                    self.set_focus(Focus::Chat);
                    return Ok(());
                }
                if let Some(vendor) = VENDORS.iter().find(|vendor| vendor.label() == value) {
                    self.chats
                        .open_vendor(vendor)
                        .map_err(|err| format!("Can't start {}: {err}", vendor.name))?;
                    self.set_focus(Focus::Chat);
                    return Ok(());
                }
                // With the colon, so "Codex: ..." isn't taken for the Code agent.
                let agent = AGENTS
                    .iter()
                    .position(|agent| value.starts_with(&format!("{}: ", agent.name)))
                    .ok_or("Unknown agent")?;
                let ready: Vec<Provider> = Provider::ALL
                    .into_iter()
                    .filter(|p| p.api_key().is_some())
                    .collect();
                match ready.as_slice() {
                    [] => {
                        let vars: Vec<&str> = Provider::ALL.iter().map(|p| p.key_env()).collect();
                        return Err(format!(
                            "No API keys found. Set one of {} and restart.",
                            vars.join(", ")
                        ));
                    }
                    [only] => self.chat_provider_chosen(agent, *only),
                    many => {
                        let items = many.iter().map(|p| p.name().to_owned()).collect();
                        self.modal = Some(Modal::pick(
                            "Pick a provider",
                            items,
                            Then::ChatProvider(agent),
                        ));
                    }
                }
            }
            Then::ChatProvider(agent) => {
                let provider = Provider::ALL
                    .into_iter()
                    .find(|p| p.name() == value)
                    .ok_or("Unknown provider")?;
                self.chat_provider_chosen(agent, provider);
            }
            Then::ChatModel(agent, provider) => {
                self.chats
                    .save_model(provider, value)
                    .map_err(|err| format!("Couldn't save the model choice: {err}"))?;
                self.create_chat(agent, provider, value.to_owned());
            }
            Then::SwitchModel(id) => {
                let chat = self
                    .chats
                    .tabs
                    .iter_mut()
                    .find(|chat| chat.id == id)
                    .ok_or("That chat is closed")?;
                chat.model = value.to_owned();
                let provider = chat.provider;
                self.chats
                    .save_model(provider, value)
                    .map_err(|err| format!("Couldn't save the model choice: {err}"))?;
            }
            Then::GoToLine => {
                let line = value
                    .trim()
                    .parse()
                    .map_err(|_| format!("Not a line number: {value}"))?;
                self.editor.go_to_line(line);
            }
            Then::Find => {
                self.editor.find_query = value.to_owned();
                let cmd = self.editor.find_next(true);
                self.run_editor_cmd(cmd);
            }
            Then::Overwrite => {
                let doc = self.editor.doc().ok_or("No file open")?;
                let path = doc.path.clone().ok_or("No file path")?;
                doc.save_to(&path).map_err(text)?;
            }
            Then::SaveAs => {
                let path = safe_join(&self.root, value)?;
                if path.exists() {
                    return Err(format!("{value} already exists"));
                }
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(text)?;
                }
                self.editor
                    .doc()
                    .ok_or("No file open")?
                    .save_to(&path)
                    .map_err(text)?;
                self.files.reveal(&path);
            }
            Then::NewFile(dir) => {
                let path = safe_join(&dir, value)?;
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(text)?;
                }
                fs::File::create_new(&path).map_err(|err| format!("{value}: {err}"))?;
                self.files.reveal(&path);
                self.open_file(&path);
            }
            Then::NewDir(dir) => {
                let path = safe_join(&dir, value)?;
                if path.exists() {
                    return Err(format!("{value} already exists"));
                }
                fs::create_dir_all(&path).map_err(text)?;
                self.files.reveal(&path);
            }
            Then::Rename(old) => {
                let parent = old.parent().ok_or("Can't rename the project root")?;
                let new = safe_join(parent, value)?;
                if new == old {
                    return Ok(());
                }
                if new.exists() {
                    return Err(format!("{value} already exists"));
                }
                if let Some(folder) = new.parent() {
                    fs::create_dir_all(folder).map_err(text)?;
                }
                fs::rename(&old, &new).map_err(text)?;
                self.editor.path_renamed(&old, &new);
                self.files.reveal(&new);
            }
            Then::Delete(path) => {
                files::delete(&path).map_err(text)?;
                self.files.refresh();
                self.editor.check_disk();
            }
        }
        Ok(())
    }

    fn chat_provider_chosen(&mut self, agent: usize, provider: Provider) {
        match self.chats.saved_model(provider) {
            Some(model) => self.create_chat(agent, provider, model),
            None => {
                self.message = Some(format!("Loading {} models...", provider.name()));
                self.chats.load_models(provider, ModelsFor::NewChat(agent));
            }
        }
    }

    fn on_paste(&mut self, text: &str) {
        self.needs_draw = true;
        match &mut self.modal {
            Some(Modal::Input { input, .. }) => input.insert(&text.replace(['\r', '\n'], " ")),
            Some(Modal::Pick {
                items,
                filter,
                shown,
                selected,
                ..
            }) => {
                filter.push_str(&text.replace(['\r', '\n'], " "));
                *shown = rank(items, filter);
                *selected = 0;
            }
            Some(Modal::Confirm { .. }) => {}
            None if self.focus == Focus::Editor => self.editor.paste(text),
            None if self.focus == Focus::Terminal => self.terminals.paste(text),
            None if self.focus == Focus::Chat => self.chats.paste(text),
            None => {}
        }
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        if mouse.kind == MouseEventKind::Moved || self.modal.is_some() {
            return;
        }
        self.needs_draw = true;
        let under = self.areas.pane_at(mouse.column, mouse.row);
        self.track_selection(mouse, under);
        let target = match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(pane) = under {
                    self.set_focus(pane);
                }
                under
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => under,
            _ => Some(self.focus),
        };
        match target {
            Some(Focus::Files) => {
                let cmd = self.files.on_mouse(mouse);
                self.run_files_cmd(cmd);
            }
            Some(Focus::Editor) => self.editor.on_mouse(mouse),
            Some(Focus::Terminal) => self.terminals.on_mouse(mouse),
            Some(Focus::Chat) => self.chats.on_mouse(mouse),
            None => {}
        }
    }

    /// Dragging in the terminal or chat pane selects text. The editor has its own selection.
    fn track_selection(&mut self, mouse: MouseEvent, under: Option<Focus>) {
        let position = (mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let pane = under.filter(|pane| matches!(pane, Focus::Terminal | Focus::Chat));
                self.selection = pane.map(|pane| TextSelection {
                    pane,
                    start: position,
                    end: position,
                    copy: false,
                });
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(selection) = &mut self.selection {
                    selection.end = position;
                }
            }
            MouseEventKind::Up(MouseButton::Left) => match &mut self.selection {
                Some(selection) if selection.start != selection.end => selection.copy = true,
                _ => self.selection = None,
            },
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => self.selection = None,
            _ => {}
        }
    }

    fn draw_selection(&mut self, frame: &mut Frame) {
        let Some(selection) = &mut self.selection else {
            return;
        };
        let rect = self.areas.get(selection.pane).inner(Margin::new(1, 1));
        if rect.is_empty() {
            return;
        }
        let text = read_selection(frame.buffer_mut(), rect, selection.start, selection.end);
        if std::mem::take(&mut selection.copy) && !text.trim().is_empty() {
            self.message = Some(format!("Copied {} characters.", text.chars().count()));
            self.copy(text);
        }
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        self.areas = Areas::compute(frame.area(), self.zoom.then_some(self.focus));
        for pane in [Focus::Files, Focus::Terminal, Focus::Editor, Focus::Chat] {
            let area = self.areas.get(pane);
            if area.is_empty() {
                continue;
            }
            let focused = self.focus == pane && self.modal.is_none();
            let block = pane_block(pane, self.focus == pane);
            let inner = block.inner(area);
            frame.render_widget(block, area);
            match pane {
                Focus::Files => self.files.draw(frame, inner, focused),
                Focus::Editor => self.editor.draw(frame, inner, focused),
                Focus::Terminal => {
                    if let Some(err) = self.terminals.draw(frame, inner, focused) {
                        self.message = Some(err);
                    }
                }
                Focus::Chat => self.chats.draw(frame, inner, focused),
            }
        }
        self.draw_selection(frame);
        frame.render_widget(Paragraph::new(self.status_line()), self.areas.status);
        if let Some(modal) = &self.modal {
            draw_modal(frame, modal);
        }
    }

    fn status_line(&self) -> Line<'static> {
        let (text, style) = match &self.message {
            Some(message) => (message.clone(), Style::new().fg(Color::Yellow)),
            // Not while reviewing a proposal, whose hints already say how to answer.
            None if self.focus != Focus::Chat
                && self.chats.any_approving()
                && !(self.focus == Focus::Editor && self.editor.active_proposal().is_some()) =>
            {
                (
                    "An agent is waiting for your approval. Alt+4 to answer.".to_owned(),
                    Style::new().fg(Color::Black).bg(Color::Yellow),
                )
            }
            None => {
                let hints = match self.focus {
                    Focus::Files => files::HINTS,
                    Focus::Editor => self.editor.hints(),
                    Focus::Terminal => self.terminals.hints(),
                    Focus::Chat => self.chats.hints(),
                };
                (hints.to_owned(), Style::new().fg(Color::DarkGray))
            }
        };
        Line::from(vec![
            Span::styled(
                self.focus.title(),
                Style::new().fg(Color::Black).bg(Color::Cyan),
            ),
            Span::styled(format!(" {text}"), style),
        ])
    }
}

/// The bordered frame around a pane, highlighted when focused.
pub fn pane_block(pane: Focus, focused: bool) -> Block<'static> {
    let (border, title) = if focused {
        (
            Style::new().fg(Color::Cyan),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )
    } else {
        (
            Style::new().fg(Color::DarkGray),
            Style::new().fg(Color::Gray),
        )
    };
    Block::bordered()
        .border_style(border)
        .title(Span::styled(pane.title(), title))
}

fn draw_modal(frame: &mut Frame, modal: &Modal) {
    let screen = frame.area();
    let (title, list_rows) = match modal {
        Modal::Input { title, .. } | Modal::Confirm { title, .. } => (title, 0),
        Modal::Pick { title, shown, .. } => (title, shown.len().clamp(1, 14) as u16 + 1),
    };
    // Only rows that can be visible, because a file picker may hold thousands of items.
    let wanted = match modal {
        Modal::Pick { items, shown, .. } => shown
            .iter()
            .take(100)
            .map(|&index| items[index].width().min(200) as u16 + 4)
            .max()
            .unwrap_or(0),
        _ => 0,
    };
    let width = (title.width() as u16 + 6)
        .max(wanted)
        .clamp(44, 96)
        .min(screen.width);
    let height = (3 + list_rows).min(screen.height);
    let area = Rect {
        x: screen.x + (screen.width - width) / 2,
        y: screen.y + (screen.height - height) / 3,
        width,
        height,
    };
    let block = Block::bordered()
        .border_style(Style::new().fg(Color::Yellow))
        .title(Span::styled(
            format!(" {title} "),
            Style::new().fg(Color::Yellow),
        ));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return; // Too small to show anything inside the border.
    }
    match modal {
        Modal::Confirm { .. } => {
            frame.render_widget(Paragraph::new("y yes    n no"), inner);
        }
        Modal::Input { input, .. } => {
            let line = &input.lines()[0];
            let display = col_to_display(line, input.cursor.col);
            let left = (display + 1).saturating_sub(inner.width as usize);
            let selected = input.selection().map(|(start, end)| (start.col, end.col));
            frame.render_widget(
                Paragraph::new(render_line(line, &[], selected, left, inner.width as usize)),
                inner,
            );
            frame.set_cursor_position((inner.x + (display - left) as u16, inner.y));
        }
        Modal::Pick {
            items,
            filter,
            shown,
            selected,
            ..
        } => {
            let rows = inner.height.saturating_sub(1) as usize;
            let top = (*selected + 1).saturating_sub(rows);
            let mut lines = vec![Line::from(vec![
                Span::styled("filter: ", Style::new().fg(Color::DarkGray)),
                Span::raw(filter.clone()),
            ])];
            if shown.is_empty() {
                lines.push(Line::from(Span::styled(
                    "(no matches)",
                    Style::new().fg(Color::DarkGray),
                )));
            }
            for (row, &index) in shown.iter().enumerate().skip(top).take(rows) {
                let style = if row == *selected {
                    Style::new().fg(Color::Black).bg(Color::Yellow)
                } else {
                    Style::new()
                };
                lines.push(Line::from(Span::styled(items[index].clone(), style)));
            }
            frame.render_widget(Paragraph::new(lines), inner);
            frame.set_cursor_position((inner.x + 8 + filter.width() as u16, inner.y));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::sync::mpsc;

    #[test]
    fn navigation_is_spatial_and_returns_to_last_middle_pane() {
        use Dir::*;
        use Focus::*;
        assert_eq!(neighbor(Files, Right, Editor), Editor);
        assert_eq!(neighbor(Files, Right, Terminal), Terminal);
        assert_eq!(neighbor(Chat, Left, Terminal), Terminal);
        assert_eq!(neighbor(Editor, Left, Editor), Files);
        assert_eq!(neighbor(Terminal, Right, Terminal), Chat);
        assert_eq!(neighbor(Editor, Down, Editor), Terminal);
        assert_eq!(neighbor(Terminal, Up, Terminal), Editor);
        // Moves with nothing in that direction stay put.
        assert_eq!(neighbor(Files, Left, Editor), Files);
        assert_eq!(neighbor(Chat, Right, Editor), Chat);
        assert_eq!(neighbor(Files, Up, Editor), Files);
        assert_eq!(neighbor(Editor, Up, Editor), Editor);
        assert_eq!(neighbor(Terminal, Down, Terminal), Terminal);
    }

    fn test_app() -> App {
        let (tx, _rx) = mpsc::sync_channel(8);
        App::new(PathBuf::from("."), tx)
    }

    fn app_in(name: &str) -> (App, PathBuf) {
        let dir = std::env::temp_dir().join(format!("kalipso-app-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let (tx, _rx) = mpsc::sync_channel(8);
        (App::new(dir.clone(), tx), dir)
    }

    fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
        app.handle(Event::Input(TermEvent::Key(KeyEvent::new(code, modifiers))));
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c), KeyModifiers::NONE);
        }
    }

    #[test]
    fn new_file_prompt_creates_and_opens_it_then_save_writes_to_disk() {
        let (mut app, dir) = app_in("newfile");
        type_text(&mut app, "n");
        type_text(&mut app, "sub/hello.txt");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.focus, Focus::Editor);
        assert!(dir.join("sub/hello.txt").is_file());
        type_text(&mut app, "hi");
        press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(fs::read_to_string(dir.join("sub/hello.txt")).unwrap(), "hi");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn new_file_and_rename_refuse_to_overwrite_or_escape() {
        let (mut app, dir) = app_in("refuse");
        fs::write(dir.join("a.txt"), "A").unwrap();
        fs::write(dir.join("b.txt"), "B").unwrap();
        app.files.refresh();

        type_text(&mut app, "n");
        type_text(&mut app, "a.txt");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.message.is_some());
        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "A");

        type_text(&mut app, "r");
        type_text(&mut app, "b.txt");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.message.as_deref().unwrap().contains("already exists"));
        assert_eq!(fs::read_to_string(dir.join("b.txt")).unwrap(), "B");

        type_text(&mut app, "n");
        type_text(&mut app, "../escaped.txt");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(!dir.parent().unwrap().join("escaped.txt").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rename_moves_the_open_buffer_and_delete_needs_confirmation() {
        let (mut app, dir) = app_in("rename");
        fs::write(dir.join("old.txt"), "text").unwrap();
        app.files.refresh();
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('1'), KeyModifiers::ALT);

        type_text(&mut app, "r");
        type_text(&mut app, "new.txt");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(dir.join("new.txt").is_file() && !dir.join("old.txt").exists());
        assert_eq!(
            app.editor.docs[0].path.as_deref(),
            Some(dir.join("new.txt").as_path())
        );

        type_text(&mut app, "d");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(
            dir.join("new.txt").is_file(),
            "Enter and Esc must not confirm a delete"
        );
        type_text(&mut app, "dy");
        assert!(!dir.join("new.txt").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn quit_and_close_ask_only_when_there_are_unsaved_changes() {
        let (mut app, dir) = app_in("quit");
        press(&mut app, KeyCode::Char('3'), KeyModifiers::ALT);
        press(&mut app, KeyCode::Char('t'), KeyModifiers::ALT);
        press(&mut app, KeyCode::Char('w'), KeyModifiers::ALT);
        assert!(
            app.modal.is_none() && app.editor.docs.is_empty(),
            "clean tab closes straight away"
        );

        press(&mut app, KeyCode::Char('t'), KeyModifiers::ALT);
        type_text(&mut app, "x");
        press(&mut app, KeyCode::Char('w'), KeyModifiers::ALT);
        assert!(app.modal.is_some());
        type_text(&mut app, "n");
        assert_eq!(app.editor.docs.len(), 1, "n keeps the tab");

        press(&mut app, KeyCode::Char('q'), KeyModifiers::CONTROL);
        assert!(!app.should_quit && app.modal.is_some());
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(!app.should_quit);
        press(&mut app, KeyCode::Char('q'), KeyModifiers::CONTROL);
        type_text(&mut app, "y");
        assert!(app.should_quit);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tab_line_keeps_the_active_tab_visible_when_crowded() {
        let titles: Vec<String> = (0..10).map(|i| format!("tab-number-{i}")).collect();
        let area = Rect::new(5, 2, 40, 1);
        let (line, hits) = tab_line(&titles, 9, area);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("tab-number-9"));
        assert!(!text.contains("tab-number-0"));
        assert_eq!(
            hits.at(5, 2),
            Some(8),
            "14-column tabs: only 8 and 9 fit in 40 columns"
        );
        assert_eq!(hits.at(5, 3), None);
    }

    fn alt(code: KeyCode) -> Event {
        Event::Input(TermEvent::Key(KeyEvent::new(code, KeyModifiers::ALT)))
    }

    #[test]
    fn terminal_pane_regains_focus_after_visiting_chat() {
        let mut app = test_app();
        app.handle(alt(KeyCode::Char('2')));
        app.handle(alt(KeyCode::Right));
        assert_eq!(app.focus, Focus::Chat);
        app.handle(alt(KeyCode::Left));
        assert_eq!(app.focus, Focus::Terminal);
    }

    #[test]
    fn zoom_gives_focused_pane_everything_except_the_status_line() {
        let area = Rect::new(0, 0, 100, 40);
        let areas = Areas::compute(area, Some(Focus::Chat));
        assert_eq!(areas.chat, Rect::new(0, 0, 100, 39));
        assert!(areas.files.is_empty() && areas.editor.is_empty() && areas.terminal.is_empty());
    }

    fn screen_text(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn the_empty_chat_pane_offers_vendor_clis_before_api_key_agents() {
        let mut app = test_app();
        app.zoom = true;
        app.set_focus(Focus::Chat);
        let text = screen_text(&mut app);
        let plan = text.find("With your own plan").expect("vendor section");
        let key = text.find("With an API key").expect("agent section");
        assert!(plan < key);
        for vendor in &VENDORS {
            assert!(text[plan..key].contains(vendor.name), "{}", vendor.name);
        }
    }

    fn vendor_screen(app: &App) -> String {
        app.chats
            .vendors
            .first()
            .map_or_else(String::new, |tab| tab.term.contents())
    }

    #[test]
    fn picking_a_vendor_runs_its_cli_in_a_chat_tab_that_gets_the_keys() {
        let (_, dir) = app_in("vendor");
        let (tx, rx) = mpsc::sync_channel(256);
        let mut app = App::new(dir.clone(), tx);
        // A fake codex where its installer puts it, and a PATH without the real one.
        let home = dir.join("home");
        let bin = home.join(".local/bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(
            bin.join("codex"),
            "#!/bin/sh\necho \"codex in $(pwd)\"\nread -r line\necho \"got $line\"\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(bin.join("codex"), fs::Permissions::from_mode(0o755)).unwrap();
        app.chats.vendor_env = vec![
            ("HOME".to_owned(), home.display().to_string()),
            ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            ("SHELL".to_owned(), "/bin/sh".to_owned()),
        ];

        app.handle(alt(KeyCode::Char('4')));
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        type_text(&mut app, "Codex");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.chats.vendor_active(), "{:?}", app.message);
        assert!(app.chats.tabs.is_empty(), "Codex isn't the Code agent");
        assert!(screen_text(&mut app).contains("Codex"));
        let wanted = format!("codex in {}", dir.display());
        pump(&mut app, &rx, |app| vendor_screen(app).contains(&wanted));

        // A Kalipso chat goes before the vendor tab, and the vendor tab is one step on.
        app.chats.create(0, Provider::Claude, "m".to_owned());
        assert!(!app.chats.vendor_active());
        app.handle(alt(KeyCode::PageDown));
        assert!(app.chats.vendor_active());

        press(&mut app, KeyCode::Char('p'), KeyModifiers::CONTROL);
        assert!(
            app.modal.is_none(),
            "Ctrl+P goes to the CLI, not the file finder"
        );
        type_text(&mut app, "hi");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        pump(&mut app, &rx, |app| vendor_screen(app).contains("got hi"));

        // The fake exits after one line, which closes its tab and leaves the chat active.
        pump(&mut app, &rx, |app| app.chats.vendors.is_empty());
        assert!(!app.chats.vendor_active());
        assert!(app.chats.active_chat().is_some());
    }

    #[test]
    fn ctrl_q_asks_while_a_vendor_tab_is_open_and_alt_w_closes_it() {
        let (_, dir) = app_in("vendor-close");
        let (tx, _rx) = mpsc::sync_channel(256);
        let mut app = App::new(dir.clone(), tx);
        app.chats.vendor_env = vec![
            ("HOME".to_owned(), dir.display().to_string()),
            ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            ("SHELL".to_owned(), "/bin/sh".to_owned()),
        ];
        app.chats.open_vendor(&VENDORS[0]).unwrap();
        app.set_focus(Focus::Chat);
        press(&mut app, KeyCode::Char('q'), KeyModifiers::CONTROL);
        assert!(
            !app.should_quit && app.modal.is_some(),
            "the CLI may be mid-task"
        );
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(!app.should_quit && app.modal.is_none());

        app.handle(alt(KeyCode::Char('w')));
        assert!(app.chats.vendors.is_empty());
        press(&mut app, KeyCode::Char('q'), KeyModifiers::CONTROL);
        assert!(app.should_quit, "nothing is left to stop");
    }

    #[test]
    fn all_panes_render_with_titles() {
        let mut app = test_app();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        for title in ["1 Files", "2 Terminal", "3 Editor", "4 Chat"] {
            assert!(text.contains(title), "missing {title}");
        }
    }

    #[test]
    fn project_search_opens_the_match_with_it_selected() {
        let (mut app, dir) = app_in("search");
        let (tx, rx) = mpsc::sync_channel(8);
        app.tx = tx;
        fs::create_dir(dir.join("src")).unwrap();
        fs::write(dir.join("src/a.rs"), "first\nthe needle: here\n").unwrap();
        fs::write(dir.join(".gitignore"), "skipped.txt\n").unwrap();
        fs::write(dir.join("skipped.txt"), "needle").unwrap();
        fs::write(dir.join(".env"), "needle=secret").unwrap();
        app.files.refresh();

        press(&mut app, KeyCode::Char('2'), KeyModifiers::ALT);
        press(&mut app, KeyCode::Char('f'), KeyModifiers::ALT);
        assert!(app.modal.is_none(), "the shell uses Alt+F");
        press(&mut app, KeyCode::Char('1'), KeyModifiers::ALT);
        press(&mut app, KeyCode::Char('f'), KeyModifiers::ALT);
        type_text(&mut app, "needle");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        app.handle(rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap());

        let Some(Modal::Pick { items, title, .. }) = &app.modal else {
            panic!("expected results")
        };
        assert_eq!(title, "1 match for needle");
        assert_eq!(
            items,
            &["src/a.rs:2: the needle: here"],
            "ignored and private files stay out"
        );
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.focus, Focus::Editor);
        assert_eq!(
            app.editor.doc().unwrap().buf.selected_text().as_deref(),
            Some("needle")
        );
        assert_eq!(app.editor.doc().unwrap().buf.selection().unwrap().0.line, 1);

        type_text(&mut app, "x");
        press(&mut app, KeyCode::Char('f'), KeyModifiers::ALT);
        type_text(&mut app, "zzzzz");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        app.handle(rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap());
        assert!(app.modal.is_none());
        assert_eq!(app.message.as_deref(), Some("No matches for zzzzz."));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn git_state_reaches_the_tree_and_the_gutter() {
        let (mut app, dir) = app_in("gitmarks");
        fs::create_dir(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/a.txt"), "one\nnew line\nthree\n").unwrap();
        fs::write(dir.join("b.txt"), "b\n").unwrap();
        app.files.refresh();
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.editor.known_bases(), [(dir.join("sub/a.txt"), None)]);

        let changes = [
            (dir.join("sub/a.txt"), git::Change::Modified),
            (dir.join("b.txt"), git::Change::Added),
        ]
        .into_iter()
        .collect();
        let snapshot = git::Snapshot {
            head: Some("abc".to_owned()),
            changes,
            bases: vec![(dir.join("sub/a.txt"), Some("one\ntwo\nthree\n".to_owned()))],
        };
        app.handle(Event::Git(Some(snapshot)));
        assert!(!app.git_running);
        assert_eq!(
            app.editor.known_bases(),
            [(dir.join("sub/a.txt"), Some("abc".to_owned()))],
            "not reloaded until HEAD moves"
        );

        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let rows: Vec<String> = (0..30)
            .map(|y| {
                (0..120)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol().to_owned())
                    .collect()
            })
            .collect();
        let row_with = |text: &str| {
            rows.iter()
                .find(|row| row.contains(text))
                .unwrap_or_else(|| panic!("{text}: {rows:#?}"))
        };
        assert!(
            row_with("sub").contains("sub •"),
            "folders show changes inside"
        );
        assert!(rows.iter().any(|row| row.contains("a.txt M")), "{rows:#?}");
        assert!(rows.iter().any(|row| row.contains("b.txt A")), "{rows:#?}");
        assert!(
            row_with("new line").contains("2▎new line"),
            "the changed line is marked"
        );
        assert!(row_with("one").contains("1 one"), "unchanged lines aren't");

        app.handle(Event::Git(None));
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(
            !text.contains("a.txt M"),
            "marks clear if the repository goes away"
        );
        drop(app);
        fs::remove_dir_all(&dir).unwrap();
    }

    fn server_rig(name: &str) -> (App, PathBuf, std::sync::mpsc::Receiver<Event>, PathBuf) {
        let (mut app, dir) = app_in(name);
        let (tx, rx) = mpsc::sync_channel(256);
        app.chats = Chats::with_config(dir.clone(), tx.clone(), dir.join("config.json"));
        app.servers = Servers::new(dir.clone(), tx);
        let log = dir.join("server.log");
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/mock_lsp.py");
        let command = format!("MOCK_LSP_LOG='{}' exec python3 '{script}'", log.display());
        app.servers.command_override = Some(vec!["sh".to_owned(), "-c".to_owned(), command]);
        (app, dir, rx, log)
    }

    /// Handles worker messages until `done` says so.
    fn pump(app: &mut App, rx: &std::sync::mpsc::Receiver<Event>, done: impl Fn(&App) -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if done(app) {
                return;
            }
            if let Ok(event) = rx.recv_timeout(std::time::Duration::from_millis(50)) {
                app.handle(event);
            }
        }
        panic!("timed out waiting; message: {:?}", app.message);
    }

    fn logged_methods(log: &std::path::Path) -> Vec<String> {
        fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter_map(|entry| entry["method"].as_str().map(str::to_owned))
            .collect()
    }

    /// A column in a line with a character outside the BMP, where bytes, chars and UTF-16 all differ.
    const CODE: &str = "fn target() {}\nfn main() { let s = \"😀\"; target(); }\nBAD line\n";

    fn open_code(app: &mut App, dir: &std::path::Path) -> PathBuf {
        let path = dir.join("code.rs");
        fs::write(&path, CODE).unwrap();
        app.open_file(&path);
        path
    }

    fn cursor_on_call(app: &mut App) {
        let line = CODE.lines().nth(1).unwrap();
        let col = line.find("target();").unwrap();
        app.editor
            .doc()
            .unwrap()
            .buf
            .set_cursor(crate::editor::Pos { line: 1, col }, false);
    }

    #[test]
    fn language_servers_never_start_until_the_user_says_so_and_it_is_remembered() {
        let (mut app, dir, rx, log) = server_rig("trust");
        open_code(&mut app, &dir);
        for _ in 0..3 {
            app.handle(Event::Tick);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            !log.exists(),
            "nothing may start just because a file is open"
        );

        press(&mut app, KeyCode::F(12), KeyModifiers::NONE);
        let Some(Modal::Confirm { title, .. }) = &app.modal else {
            panic!("expected a question")
        };
        assert!(
            title.contains("rust-analyzer") && title.contains("run code"),
            "{title}"
        );
        press(&mut app, KeyCode::Char('n'), KeyModifiers::NONE);
        app.handle(Event::Tick);
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!log.exists(), "n means no");
        assert!(!app.chats.trusts_server("rust-analyzer"));

        press(&mut app, KeyCode::F(8), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
        assert!(app.servers.is_running(0));
        assert!(app.chats.trusts_server("rust-analyzer"));
        assert!(
            fs::read_to_string(dir.join("config.json"))
                .unwrap()
                .contains(dir.to_str().unwrap()),
            "saved for this project"
        );

        // Another run in this project starts it without asking; another project still asks.
        let (tx, _rx2) = mpsc::sync_channel(8);
        let again = Chats::with_config(dir.clone(), tx.clone(), dir.join("config.json"));
        assert!(again.trusts_server("rust-analyzer"));
        let elsewhere = Chats::with_config(
            PathBuf::from("/some/other/project"),
            tx,
            dir.join("config.json"),
        );
        assert!(!elsewhere.trusts_server("rust-analyzer"));
        drop(rx);
        drop(app);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_trusted_server_starts_by_itself_and_reports_problems_and_saves() {
        let (mut app, dir, rx, log) = server_rig("diagnostics");
        app.chats.trust_server("rust-analyzer").unwrap();
        let path = open_code(&mut app, &dir);
        app.handle(Event::Tick);
        assert!(app.servers.is_running(0));
        pump(&mut app, &rx, |app| {
            app.editor
                .diagnostics
                .get(&path)
                .is_some_and(|list| !list.is_empty())
        });

        let problems = &app.editor.diagnostics[&path];
        assert_eq!((problems[0].line, problems[0].severity), (2, 1));
        assert_eq!(
            problems[0].message, "mock problem: BAD",
            "only the first line of a message"
        );

        let mut terminal = Terminal::new(TestBackend::new(150, 30)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(
            text.contains("BAD line   mock problem: BAD"),
            "shown at the end of the line"
        );

        press(&mut app, KeyCode::F(8), KeyModifiers::NONE);
        assert_eq!(
            app.editor.doc().unwrap().buf.cursor.line,
            2,
            "F8 jumps to it"
        );
        assert_eq!(app.message.as_deref(), Some("1 problem: mock problem: BAD"));

        // Edits reach the server. Fixing the line clears the problem.
        let doc = app.editor.doc().unwrap();
        doc.buf.replace_all("fn target() {}\nfine\n");
        app.handle(Event::Tick);
        pump(&mut app, &rx, |app| {
            app.editor.diagnostics[&path].is_empty()
        });

        // Saves reach the server, which is when rust-analyzer runs cargo check.
        press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
        app.handle(Event::Tick);
        pump(&mut app, &rx, |app| {
            app.editor.diagnostics[&path]
                .iter()
                .any(|problem| problem.message == "mock saw the save")
        });

        // Closing the tab tells the server too, and the server sends the whole file on change.
        press(&mut app, KeyCode::Char('w'), KeyModifiers::ALT);
        app.handle(Event::Tick);
        pump(&mut app, &rx, |_| {
            logged_methods(&log).contains(&"textDocument/didClose".to_owned())
        });
        let methods = logged_methods(&log);
        assert_eq!(methods[0], "initialize", "the handshake comes first");
        assert_eq!(methods[1], "initialized");
        let reply = fs::read_to_string(&log).unwrap();
        assert!(
            reply
                .lines()
                .any(|line| line.contains("\"id\":900") || line.contains("\"id\": 900")),
            "the server's own request got an answer"
        );
        drop(app);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn go_to_definition_converts_columns_and_shift_f12_comes_back() {
        let (mut app, dir, rx, _log) = server_rig("definition");
        let path = open_code(&mut app, &dir);
        cursor_on_call(&mut app);
        press(&mut app, KeyCode::F(12), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
        pump(&mut app, &rx, |app| app.editor.docs[0].buf.cursor.line == 0);
        let cursor = app.editor.docs[0].buf.cursor;
        assert_eq!(cursor.col, 3, "on the name of `fn target`");
        assert_eq!(app.message, None);

        press(&mut app, KeyCode::F(12), KeyModifiers::SHIFT);
        let back = app.editor.docs[0].buf.cursor;
        assert_eq!(
            (back.line, back.col),
            (1, CODE.lines().nth(1).unwrap().find("target();").unwrap()),
            "back where it started"
        );
        press(&mut app, KeyCode::F(12), KeyModifiers::SHIFT);
        assert_eq!(app.message.as_deref(), Some("Nowhere to go back to."));

        // A word with no definition, and a jump into another file.
        app.editor
            .doc()
            .unwrap()
            .buf
            .set_cursor(crate::editor::Pos { line: 2, col: 0 }, false);
        press(&mut app, KeyCode::F(12), KeyModifiers::NONE);
        pump(&mut app, &rx, |app| {
            app.message.as_deref() == Some("No definition found.")
        });

        let other = dir.join("other.rs");
        fs::write(&other, "// a comment first\nfn elsewhere() {}\n").unwrap();
        app.open_file(&other);
        app.handle(Event::Tick);
        app.open_file(&path);
        let doc = app.editor.doc().unwrap();
        doc.buf.replace_all("elsewhere();\n");
        doc.buf
            .set_cursor(crate::editor::Pos { line: 0, col: 0 }, false);
        press(&mut app, KeyCode::F(12), KeyModifiers::NONE);
        pump(&mut app, &rx, |app| {
            app.editor.docs[app.editor.active].path.as_deref() == Some(other.as_path())
        });
        assert_eq!(app.editor.docs[app.editor.active].buf.cursor.line, 1);
        press(&mut app, KeyCode::F(12), KeyModifiers::SHIFT);
        assert_eq!(
            app.editor.docs[app.editor.active].path.as_deref(),
            Some(path.as_path()),
            "back across files"
        );
        drop(app);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_definition_says_when_the_server_is_still_loading() {
        let (mut app, dir, rx, _log) = server_rig("loading");
        app.chats.trust_server("rust-analyzer").unwrap();
        let path = dir.join("code.rs");
        fs::write(&path, "STILL_LOADING\nunknown_word\n").unwrap();
        app.open_file(&path);
        app.handle(Event::Tick);
        // A problem-free first report means the server has seen the file and reported its work.
        pump(&mut app, &rx, |app| {
            app.editor.diagnostics.contains_key(&path)
        });
        let doc = app.editor.doc().unwrap();
        doc.buf
            .set_cursor(crate::editor::Pos { line: 1, col: 0 }, false);
        press(&mut app, KeyCode::F(12), KeyModifiers::NONE);
        pump(&mut app, &rx, |app| {
            app.message
                .as_deref()
                .is_some_and(|text| text.starts_with("Nothing found yet"))
        });
        assert_eq!(
            app.message.as_deref(),
            Some(
                "Nothing found yet, but rust-analyzer is still working (Indexing). Try again soon."
            )
        );

        // The server finishes, and now "nothing" means nothing.
        let doc = app.editor.doc().unwrap();
        doc.buf.replace_all("done\nunknown_word\n");
        doc.buf
            .set_cursor(crate::editor::Pos { line: 1, col: 0 }, false);
        press(&mut app, KeyCode::F(12), KeyModifiers::NONE);
        pump(&mut app, &rx, |app| {
            app.message.as_deref() == Some("No definition found.")
        });
        drop(app);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_late_definition_answer_does_not_pull_the_user_away() {
        let (mut app, dir, rx, _log) = server_rig("late");
        app.chats.trust_server("rust-analyzer").unwrap();
        open_code(&mut app, &dir);
        app.handle(Event::Tick);
        let asked = |app: &mut App| {
            cursor_on_call(app);
            press(app, KeyCode::F(12), KeyModifiers::NONE);
            assert!(app.message.is_some());
        };

        // Moved to another pane: keystrokes must stay there.
        asked(&mut app);
        press(&mut app, KeyCode::Char('2'), KeyModifiers::ALT);
        pump(&mut app, &rx, |app| app.message.is_none());
        assert_eq!(app.focus, Focus::Terminal);
        assert_eq!(app.editor.docs[0].buf.cursor.line, 1, "no jump");
        press(&mut app, KeyCode::Char('3'), KeyModifiers::ALT);

        // Moved the cursor.
        asked(&mut app);
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        app.message = Some("waiting".to_owned());
        pump(&mut app, &rx, |app| app.message.is_none());
        assert_eq!(app.editor.docs[0].buf.cursor.line, 2, "no jump");

        // Kept typing.
        asked(&mut app);
        type_text(&mut app, "x");
        app.message = Some("waiting".to_owned());
        pump(&mut app, &rx, |app| app.message.is_none());
        assert_eq!(app.editor.docs[0].buf.cursor.line, 1, "no jump");

        // Stayed put: it jumps.
        app.editor.doc().unwrap().buf.undo();
        asked(&mut app);
        pump(&mut app, &rx, |app| app.editor.docs[0].buf.cursor.line == 0);
        drop(app);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_server_that_cannot_launch_reports_it_once() {
        let (mut app, dir, rx, _log) = server_rig("nolaunch");
        app.servers.command_override = Some(vec!["/nonexistent/server".to_owned()]);
        app.chats.trust_server("rust-analyzer").unwrap();
        open_code(&mut app, &dir);
        app.handle(Event::Tick);
        assert!(
            app.message
                .as_deref()
                .unwrap()
                .starts_with("Can't start rust-analyzer")
        );
        app.message = None;
        app.handle(Event::Tick);
        assert_eq!(app.message, None, "not repeated every second");
        assert!(app.servers.has_failed(0));
        drop(rx);
        drop(app);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_server_that_dies_explains_why_and_does_not_restart_in_a_loop() {
        let (mut app, dir, rx, _log) = server_rig("crash");
        // Like rustup's placeholder for a component that isn't installed.
        let command =
            "echo \"error: Unknown binary 'rust-analyzer' in official toolchain\" >&2; exit 1";
        app.servers.command_override =
            Some(vec!["sh".to_owned(), "-c".to_owned(), command.to_owned()]);
        app.chats.trust_server("rust-analyzer").unwrap();
        open_code(&mut app, &dir);
        app.handle(Event::Tick);
        pump(&mut app, &rx, |app| !app.servers.is_running(0));
        assert_eq!(
            app.message.as_deref(),
            Some(
                "rust-analyzer stopped: error: Unknown binary 'rust-analyzer' in official toolchain"
            )
        );

        app.handle(Event::Tick);
        assert!(!app.servers.is_running(0), "no restart loop");
        assert!(app.servers.has_failed(0));

        press(&mut app, KeyCode::F(12), KeyModifiers::NONE);
        assert!(app.modal.is_some(), "asking explicitly can try again");
        drop(app);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn language_servers_do_not_get_api_keys_and_die_with_the_app() {
        let (mut app, dir, rx, log) = server_rig("secrets");
        // SAFETY: tests only read this variable in child processes.
        unsafe { std::env::set_var("OPENROUTER_API_KEY", "sk-should-not-leak") };
        app.chats.trust_server("rust-analyzer").unwrap();
        open_code(&mut app, &dir);
        app.handle(Event::Tick);
        pump(&mut app, &rx, |_| {
            log.exists() && logged_methods(&log).len() >= 2
        });
        let first: serde_json::Value =
            serde_json::from_str(fs::read_to_string(&log).unwrap().lines().next().unwrap())
                .unwrap();
        assert_eq!(first["secret"], serde_json::Value::Null);
        let pid = first["pid"].as_i64().unwrap();
        drop(app);
        let alive = std::path::Path::new("/proc").join(pid.to_string()).exists();
        assert!(!alive, "the server must not outlive the app");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn files_with_no_server_explain_what_to_install() {
        let (mut app, dir) = app_in("noserver");
        fs::write(dir.join("notes.txt"), "text").unwrap();
        app.open_file(&dir.join("notes.txt"));
        press(&mut app, KeyCode::F(12), KeyModifiers::NONE);
        assert_eq!(
            app.message.as_deref(),
            Some("No language server is known for this kind of file.")
        );
        assert!(app.modal.is_none());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Reviewing in the editor uses the real agent loop against a local server.
    #[test]
    fn a_proposed_change_can_be_reviewed_and_answered_in_the_editor() {
        use crate::llm::testing::{mock, sse};
        use serde_json::json;
        let turn = |events: Vec<serde_json::Value>| {
            let mut all = vec![
                json!({"type": "message_start", "message": {"usage": {"input_tokens": 1, "output_tokens": 1}}}),
            ];
            all.extend(events);
            all.push(json!({"type": "message_stop"}));
            sse(&all)
        };
        let tool = |id: &str, contents: &str| {
            turn(vec![
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": id, "name": "write_file", "input": {}}}),
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": json!({"path": "f.txt", "contents": contents}).to_string()}}),
                json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}}),
            ])
        };
        let done = turn(vec![
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
        ]);
        let server = mock(vec![
            (200, tool("t1", "one\nCHANGED\n")),
            (200, tool("t2", "second try\n")),
            (200, done),
        ]);

        let (mut app, dir) = app_in("proposal");
        let (tx, rx) = mpsc::sync_channel(64);
        fs::write(dir.join("f.txt"), "one\ntwo\n").unwrap();
        app.chats = Chats::with_config(dir.clone(), tx, dir.join("config.json"));
        app.chats.endpoint_override = Some((server.url.clone(), "k".to_owned()));
        app.chats.create(0, Provider::Claude, "m".to_owned());
        let settle = |app: &mut App| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while std::time::Instant::now() < deadline {
                if !app.chats.tabs[0].is_busy() || app.chats.tabs[0].is_approving() {
                    return;
                }
                if let Ok(event) = rx.recv_timeout(std::time::Duration::from_millis(50)) {
                    app.handle(event);
                }
            }
            panic!("never settled");
        };

        press(&mut app, KeyCode::Char('4'), KeyModifiers::ALT);
        type_text(&mut app, "change it");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        settle(&mut app);
        press(&mut app, KeyCode::Char('v'), KeyModifiers::NONE);
        assert_eq!(app.focus, Focus::Editor);
        assert_eq!(app.editor.docs[0].title(), "f.txt (proposed)");

        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("2▎CHANGED"), "the proposed line is marked");
        assert!(
            text.contains("Proposed by an agent"),
            "the status line explains"
        );

        // Read-only, and the tab can't touch the real file.
        type_text(&mut app, "x");
        app.handle(Event::Input(TermEvent::Paste("pasted".to_owned())));
        press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(app.editor.docs[0].buf.text(), "one\nCHANGED\n");
        assert_eq!(
            fs::read_to_string(dir.join("f.txt")).unwrap(),
            "one\ntwo\n",
            "nothing written yet"
        );
        app.handle(Event::Tick);
        assert_eq!(
            app.editor.docs[0].buf.text(),
            "one\nCHANGED\n",
            "not reloaded from disk"
        );

        // Esc looks away without answering, and v comes back to the same tab.
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.focus, Focus::Chat);
        assert!(app.chats.tabs[0].is_approving());
        press(&mut app, KeyCode::Char('v'), KeyModifiers::NONE);
        assert_eq!(app.editor.docs.len(), 1);

        press(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
        assert_eq!(app.focus, Focus::Chat, "answering returns to the chat");
        settle(&mut app);
        assert_eq!(
            fs::read_to_string(dir.join("f.txt")).unwrap(),
            "one\nCHANGED\n"
        );

        // The next question is a new one, so the old proposal is gone.
        assert!(app.chats.tabs[0].is_approving());
        assert!(app.editor.docs.is_empty(), "the answered proposal closed");
        press(&mut app, KeyCode::Char('v'), KeyModifiers::NONE);
        assert_eq!(app.editor.docs[0].buf.text(), "second try\n");
        press(&mut app, KeyCode::Char('n'), KeyModifiers::NONE);
        settle(&mut app);
        assert_eq!(
            fs::read_to_string(dir.join("f.txt")).unwrap(),
            "one\nCHANGED\n",
            "declined in the editor"
        );
        assert!(app.editor.docs.is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fuzzy_matching_ranks_what_you_probably_meant_first() {
        let items: Vec<String> = [
            "tests/app_helpers/mod.rs",
            "src/chat.rs",
            "src/app.rs",
            "docs/a/p/p.md",
            "README.md",
        ]
        .map(String::from)
        .to_vec();
        let top = |query: &str| {
            rank(&items, query)
                .first()
                .map(|&index| items[index].as_str())
        };
        assert_eq!(
            top("app"),
            Some("src/app.rs"),
            "the file name beats a folder name"
        );
        assert_eq!(top("chat"), Some("src/chat.rs"));
        assert_eq!(top("CHAT"), Some("src/chat.rs"), "case doesn't matter");
        assert_eq!(top("schr"), Some("src/chat.rs"), "letters may have gaps");
        assert_eq!(top("zzz"), None);
        assert_eq!(
            rank(&items, ""),
            [0, 1, 2, 3, 4],
            "no filter keeps the original order"
        );
        assert_eq!(fuzzy_score("abc", "cba"), None, "order matters");
    }

    #[test]
    fn ctrl_p_finds_a_file_and_stays_out_of_the_terminals_way() {
        let (mut app, dir) = app_in("ctrlp");
        fs::create_dir_all(dir.join("src/deep")).unwrap();
        fs::write(dir.join("src/deep/needle.rs"), "found it").unwrap();
        fs::write(dir.join(".gitignore"), "skipped.txt\n").unwrap();
        fs::write(dir.join("skipped.txt"), "").unwrap();
        app.files.refresh();

        press(&mut app, KeyCode::Char('2'), KeyModifiers::ALT);
        press(&mut app, KeyCode::Char('p'), KeyModifiers::CONTROL);
        assert!(app.modal.is_none(), "the shell uses Ctrl+P");

        press(&mut app, KeyCode::Char('1'), KeyModifiers::ALT);
        press(&mut app, KeyCode::Char('p'), KeyModifiers::CONTROL);
        let Some(Modal::Pick { items, .. }) = &app.modal else {
            panic!("expected the picker")
        };
        assert!(!items.iter().any(|item| item == "skipped.txt"));
        type_text(&mut app, "ndl");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.focus, Focus::Editor);
        assert_eq!(app.editor.docs[0].buf.text(), "found it");
        assert_eq!(
            app.files.rows[app.files.selected].name, "needle.rs",
            "the tree reveals it"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn editor_context_describes_what_is_open_and_withholds_private_selections() {
        let (mut app, dir) = app_in("context");
        assert!(
            app.editor_context().is_none(),
            "nothing open, nothing to say"
        );

        fs::write(dir.join("code.rs"), "one\ntwo\nthree\n").unwrap();
        app.open_file(&dir.join("code.rs"));
        let doc = app.editor.doc().unwrap();
        doc.buf
            .set_cursor(crate::editor::Pos { line: 1, col: 0 }, false);
        let context = app.editor_context().unwrap();
        assert_eq!(context.label, "with code.rs:2");
        assert_eq!(
            context.for_model,
            "[Editor state, for context only: code.rs is open with the cursor on line 2.]"
        );

        let doc = app.editor.doc().unwrap();
        doc.buf
            .set_cursor(crate::editor::Pos { line: 2, col: 5 }, true);
        let context = app.editor_context().unwrap();
        assert_eq!(context.label, "with code.rs:3, 2 selected lines");
        assert!(
            context
                .for_model
                .contains("Selected lines 2-3:\n<selection>\ntwo\nthree\n</selection>"),
            "{}",
            context.for_model
        );

        app.editor.doc().unwrap().buf.insert("x");
        assert!(
            app.editor_context()
                .unwrap()
                .for_model
                .contains("unsaved changes")
        );

        // A selection in a private file must not reach a provider without the read approval.
        for private in [".env", "secret.pem", "ignored.txt"] {
            fs::write(dir.join(".gitignore"), "ignored.txt\n").unwrap();
            fs::write(dir.join(private), "API_KEY=hunter2\nmore").unwrap();
            app.open_file(&dir.join(private));
            let doc = app.editor.doc().unwrap();
            doc.buf.edit_key(
                KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
                1,
                "",
            );
            let context = app.editor_context().unwrap();
            assert!(
                !context.for_model.contains("hunter2"),
                "{private}: {}",
                context.for_model
            );
            assert!(
                context.label.contains("selection not sent"),
                "{private}: {}",
                context.label
            );
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_huge_selection_is_cut_on_a_character_boundary() {
        let (mut app, dir) = app_in("hugeselect");
        fs::write(dir.join("big.txt"), "日本語".repeat(MAX_CONTEXT_BYTES)).unwrap();
        app.open_file(&dir.join("big.txt"));
        app.editor.doc().unwrap().buf.edit_key(
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
            1,
            "",
        );
        let text = app.editor_context().unwrap().for_model;
        assert!(text.contains("cut off") && text.len() < MAX_CONTEXT_BYTES + 300);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn selections_read_like_text_and_cope_with_wide_characters() {
        let area = Rect::new(0, 0, 12, 4);
        let mut buffer = ScreenBuffer::empty(area);
        buffer.set_string(0, 0, "first line", Style::new());
        buffer.set_string(0, 1, "日本語 wide", Style::new());
        buffer.set_string(0, 2, "third", Style::new());
        let rect = Rect::new(0, 0, 12, 3);
        assert_eq!(
            read_selection(&mut buffer, rect, (6, 0), (4, 2)),
            "line\n日本語 wide\nthird"
        );
        assert_eq!(
            read_selection(&mut buffer, rect, (4, 2), (6, 0)),
            "line\n日本語 wide\nthird",
            "dragging backwards"
        );
        assert_eq!(read_selection(&mut buffer, rect, (0, 1), (5, 1)), "日本語");
        assert_eq!(
            read_selection(&mut buffer, rect, (0, 0), (99, 99)),
            "first line\n日本語 wide\nthird",
            "clamped to the pane"
        );
        assert_eq!(buffer[(6, 0)].style().bg, Some(SELECTION_BG));
    }

    #[test]
    fn dragging_in_the_chat_or_terminal_copies_but_a_plain_click_does_not() {
        let mouse = |kind, column, row| {
            Event::Input(TermEvent::Mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            }))
        };
        let down = MouseEventKind::Down(MouseButton::Left);
        let drag = MouseEventKind::Drag(MouseButton::Left);
        let up = MouseEventKind::Up(MouseButton::Left);

        let mut app = test_app();
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let chat = app.areas.chat;
        let (x, y) = (chat.x + 1, chat.y + 1); // "Press Enter or Alt+T to start a chat."

        app.handle(mouse(down, x, y));
        app.handle(mouse(up, x, y));
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_eq!(app.clipboard, "", "a click isn't a selection");

        app.handle(mouse(down, x, y));
        app.handle(mouse(drag, x + 4, y));
        app.handle(mouse(up, x + 4, y));
        assert_eq!(app.clipboard, "", "copied on the next draw");
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_eq!(app.clipboard, "Press");
        assert_eq!(app.message.as_deref(), Some("Copied 5 characters."));
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_eq!(
            app.message.as_deref(),
            Some("Copied 5 characters."),
            "copies once"
        );

        app.handle(Event::Input(TermEvent::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        ))));
        assert!(app.selection.is_none(), "a key press clears the highlight");
    }

    /// Every pane has content, and a popup is open, at every small terminal size.
    #[test]
    fn drawing_never_panics_at_any_terminal_size() {
        let (mut app, dir) = app_in("sizes");
        fs::write(dir.join("a.txt"), "日本語\n\tindented\n".repeat(50)).unwrap();
        app.files.refresh();
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        app.chats.create(0, Provider::Claude, "m".to_owned());
        app.chats.tabs[0].input = Buffer::from_text(&"long input line\n".repeat(20));

        let draw_all = |app: &mut App| {
            for width in 1..50 {
                for height in 1..16 {
                    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                    terminal.draw(|frame| app.draw(frame)).unwrap();
                }
            }
        };
        draw_all(&mut app);
        for pane in ['1', '2', '3', '4'] {
            press(&mut app, KeyCode::Char(pane), KeyModifiers::ALT);
            press(&mut app, KeyCode::Char('z'), KeyModifiers::ALT);
            let mut terminal = Terminal::new(TestBackend::new(3, 3)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            press(&mut app, KeyCode::Char('z'), KeyModifiers::ALT);
        }
        press(&mut app, KeyCode::Char('1'), KeyModifiers::ALT);
        type_text(&mut app, "n");
        draw_all(&mut app);
        app.modal = Some(Modal::pick(
            "Pick",
            vec!["a very long item ".repeat(20); 40],
            Then::ChatAgent(Vec::new()),
        ));
        draw_all(&mut app);
        app.modal = Some(Modal::Confirm {
            title: "A long question ".repeat(20),
            then: Then::Quit,
        });
        draw_all(&mut app);
        drop(app);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn click_focuses_the_pane_under_the_mouse() {
        let mut app = test_app();
        app.areas = Areas::compute(Rect::new(0, 0, 100, 30), None);
        let chat = app.areas.chat;
        app.handle(Event::Input(TermEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: chat.x + 1,
            row: chat.y + 1,
            modifiers: KeyModifiers::NONE,
        })));
        assert_eq!(app.focus, Focus::Chat);
    }
}
