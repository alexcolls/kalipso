use std::fs;
use std::io::stdout;
use std::path::PathBuf;
use std::sync::mpsc::SyncSender;

use crossterm::clipboard::CopyToClipboard;
use crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use crossterm::execute;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::chat::{AGENTS, ChatCmd, ChatEvent, Chats, ModelsFor};
use crate::editor::{self, Buffer, Editor, EditorCmd, col_to_display, render_line};
use crate::files::{self, FileTree, FilesCmd, safe_join};
use crate::llm::{ModelInfo, Provider};
use crate::term::Terminals;

/// Everything the main thread reacts to. Worker threads only ever send these.
pub enum Event {
    Input(TermEvent),
    Tick,
    PtyOutput { id: u64, bytes: Vec<u8> },
    PtyExit { id: u64 },
    Chat { chat: u64, generation: u64, event: ChatEvent },
    Models { provider: Provider, purpose: ModelsFor, result: Result<Vec<ModelInfo>, String> },
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
        let [main, status] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(area);
        let mut areas = Areas { status, ..Areas::default() };
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
            Layout::vertical([Constraint::Percentage(65), Constraint::Percentage(35)]).areas(middle);
        Areas { files, terminal, editor, chat, status }
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
        self.0.iter().find(|(_, rect)| rect.contains(pos)).map(|(index, _)| *index)
    }
}

/// A row of tab titles, scrolled so the active one is always visible.
pub fn tab_line(titles: &[String], active: usize, area: Rect) -> (Line<'static>, TabHits) {
    let width_of = |title: &String| title.width() + 2;
    let mut first = 0;
    while first < active && titles[first..=active].iter().map(width_of).sum::<usize>() > area.width as usize {
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
    ChatAgent,
    ChatProvider(usize),
    ChatModel(usize, Provider),
    SwitchModel(u64),
}

pub enum Modal {
    Input { title: String, input: Buffer, then: Then },
    Confirm { title: String, then: Then },
    Pick { title: String, items: Vec<String>, filter: String, selected: usize, then: Then },
}

impl Modal {
    /// A text prompt with `value` selected, so typing replaces it.
    fn input(title: impl Into<String>, value: &str, then: Then) -> Modal {
        let mut input = Buffer::from_text(value);
        input.edit_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL), 1, "");
        Modal::Input { title: title.into(), input, then }
    }

    fn pick(title: impl Into<String>, items: Vec<String>, then: Then) -> Modal {
        Modal::Pick { title: title.into(), items, filter: String::new(), selected: 0, then }
    }
}

/// Items containing the filter text, ignoring case.
fn filtered<'a>(items: &'a [String], filter: &str) -> Vec<&'a String> {
    let filter = filter.to_lowercase();
    items.iter().filter(|item| item.to_lowercase().contains(&filter)).collect()
}

pub struct App {
    pub root: PathBuf,
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
    pub modal: Option<Modal>,
    /// Shown in the status line until the next key press.
    pub message: Option<String>,
    pub clipboard: String,
}

impl App {
    pub fn new(root: PathBuf, tx: SyncSender<Event>) -> App {
        App {
            files: FileTree::new(root.clone()),
            editor: Editor::default(),
            terminals: Terminals::new(root.clone(), tx.clone()),
            chats: Chats::new(root.clone(), tx),
            root,
            focus: Focus::Files,
            middle: Focus::Editor,
            zoom: false,
            areas: Areas::default(),
            needs_draw: true,
            should_quit: false,
            modal: None,
            message: None,
            clipboard: String::new(),
        }
    }

    pub fn handle(&mut self, event: Event) {
        match event {
            Event::Input(TermEvent::Key(key)) if key.kind != KeyEventKind::Release => self.on_key(key),
            Event::Input(TermEvent::Mouse(mouse)) => self.on_mouse(mouse),
            Event::Input(TermEvent::Paste(text)) => self.on_paste(&text),
            Event::Input(TermEvent::Resize(..)) => self.needs_draw = true,
            Event::Input(_) => {}
            Event::Tick => {
                let tree = self.files.check_disk();
                let docs = self.editor.check_disk();
                self.needs_draw |= tree || docs;
            }
            Event::PtyOutput { id, bytes } => {
                self.terminals.on_output(id, &bytes);
                self.needs_draw = true;
            }
            Event::PtyExit { id } => {
                self.terminals.on_exit(id);
                self.needs_draw = true;
            }
            Event::Chat { chat, generation, event } => {
                if self.chats.on_event(chat, generation, event) {
                    // Show what the agent just did without waiting for the next tick.
                    self.files.refresh();
                    self.editor.check_disk();
                }
                self.needs_draw = true;
            }
            Event::Models { provider, purpose, result } => self.on_models(provider, purpose, result),
        }
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
        if self.modal.is_some() {
            self.on_modal_key(key);
        } else if !self.on_global_key(key) {
            match self.focus {
                Focus::Files => {
                    let cmd = self.files.on_key(key);
                    self.run_files_cmd(cmd);
                }
                Focus::Editor => {
                    let cmd = self.editor.on_key(key, &self.clipboard);
                    self.run_editor_cmd(cmd);
                }
                Focus::Terminal => self.terminals.on_key(key),
                Focus::Chat => {
                    let cmd = self.chats.on_key(key, &self.clipboard);
                    self.run_chat_cmd(cmd);
                }
            }
        }
    }

    fn run_chat_cmd(&mut self, cmd: ChatCmd) {
        match cmd {
            ChatCmd::None => {}
            ChatCmd::NewChat => self.new_chat(),
            ChatCmd::Copied(text) => self.copy(text),
            ChatCmd::Message(text) => self.message = Some(text),
            ChatCmd::PickModel(chat, provider) => {
                self.message = Some(format!("Loading {} models...", provider.name()));
                self.chats.load_models(provider, ModelsFor::Switch(chat));
            }
        }
    }

    fn new_chat(&mut self) {
        let items = AGENTS.iter().map(|agent| format!("{}: {}", agent.name, agent.about)).collect();
        self.modal = Some(Modal::pick("New chat: pick an agent", items, Then::ChatAgent));
    }

    fn on_models(&mut self, provider: Provider, purpose: ModelsFor, result: Result<Vec<ModelInfo>, String>) {
        self.needs_draw = true;
        let models = match result {
            Ok(models) => models,
            Err(err) => {
                // Features are only an optimisation: without them, requests use API defaults.
                if purpose != ModelsFor::Features {
                    self.message = Some(format!("Can't list {} models: {err}", provider.name()));
                }
                return;
            }
        };
        if provider == Provider::Claude {
            self.chats.remember_features(&models);
        }
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
        self.modal = Some(Modal::pick(format!("{} model", provider.name()), items, then));
    }

    fn create_chat(&mut self, agent: usize, provider: Provider, model: String) {
        if provider == Provider::Claude {
            self.chats.load_models(provider, ModelsFor::Features);
        }
        self.chats.create(agent, provider, model);
        self.set_focus(Focus::Chat);
    }

    /// Keys handled before the focused pane sees them, including the terminal pane.
    fn on_global_key(&mut self, key: KeyEvent) -> bool {
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('q') {
            let worry = match (self.editor.has_unsaved(), self.chats.any_busy()) {
                (true, true) => Some("Unsaved changes will be lost and running agents stopped. Quit?"),
                (true, false) => Some("Unsaved changes will be lost. Quit?"),
                (false, true) => Some("Running agents will be stopped. Quit?"),
                (false, false) => None,
            };
            match worry {
                Some(title) => self.modal = Some(Modal::Confirm { title: title.into(), then: Then::Quit }),
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
                    self.modal = Some(Modal::Confirm { title: "Discard unsaved changes in this tab?".into(), then: Then::CloseDoc(index) });
                } else {
                    self.editor.close(index);
                }
            }
            Focus::Terminal => self.terminals.close_tab(),
            Focus::Chat => {
                let Some(chat) = self.chats.active_chat() else { return };
                let (id, busy) = (chat.id, chat.is_busy());
                if busy {
                    self.modal = Some(Modal::Confirm { title: "This agent is still working. Stop it and close?".into(), then: Then::CloseChat(id) });
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
            Err(err) => self.message = Some(format!("Can't open {}: {err}", self.display_path(path))),
        }
    }

    fn display_path(&self, path: &std::path::Path) -> String {
        path.strip_prefix(&self.root).unwrap_or(path).display().to_string()
    }

    fn run_files_cmd(&mut self, cmd: FilesCmd) {
        let modal = match cmd {
            FilesCmd::None => return,
            FilesCmd::Open(path) => return self.open_file(&path),
            FilesCmd::NewFile(dir) => Modal::input(format!("New file in {}/", self.display_path(&dir)), "", Then::NewFile(dir)),
            FilesCmd::NewDir(dir) => Modal::input(format!("New folder in {}/", self.display_path(&dir)), "", Then::NewDir(dir)),
            FilesCmd::Rename(path) => {
                let name = path.file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned());
                Modal::input(format!("Rename {name} to"), &name, Then::Rename(path))
            }
            FilesCmd::Delete(path) => {
                let name = self.display_path(&path);
                let is_dir = fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir());
                let title = if is_dir {
                    format!("Delete folder {name} and the {} items inside?", files::count_inside(&path))
                } else {
                    format!("Delete {name}?")
                };
                Modal::Confirm { title, then: Then::Delete(path) }
            }
        };
        self.modal = Some(modal);
    }

    fn run_editor_cmd(&mut self, cmd: EditorCmd) {
        match cmd {
            EditorCmd::None => {}
            EditorCmd::Message(text) => self.message = Some(text),
            EditorCmd::Copied(text) => self.copy(text),
            EditorCmd::SaveAs => self.modal = Some(Modal::input("Save as (relative to project)", "", Then::SaveAs)),
            EditorCmd::ConfirmOverwrite => {
                self.modal = Some(Modal::Confirm { title: "File changed on disk. Overwrite it with your version?".into(), then: Then::Overwrite });
            }
            EditorCmd::GoToLine => self.modal = Some(Modal::input("Go to line", "", Then::GoToLine)),
            EditorCmd::Find => {
                let selected = self.editor.doc().and_then(|doc| doc.buf.selected_text()).filter(|text| !text.contains('\n'));
                let query = selected.unwrap_or_else(|| self.editor.find_query.clone());
                self.modal = Some(Modal::input("Find", &query, Then::Find));
            }
        }
    }

    /// Copies to the internal clipboard and, through the terminal, the system one.
    fn copy(&mut self, text: String) {
        let _ = execute!(stdout(), CopyToClipboard::to_clipboard_from(&text));
        self.clipboard = text;
    }

    fn on_modal_key(&mut self, key: KeyEvent) {
        let Some(modal) = self.modal.as_mut() else { return };
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
            Modal::Pick { items, filter, selected, .. } => {
                let shown = filtered(items, filter).len();
                match key.code {
                    KeyCode::Esc => self.modal = None,
                    KeyCode::Up => *selected = selected.saturating_sub(1),
                    KeyCode::Down => *selected = (*selected + 1).min(shown.saturating_sub(1)),
                    KeyCode::PageUp => *selected = selected.saturating_sub(10),
                    KeyCode::PageDown => *selected = (*selected + 10).min(shown.saturating_sub(1)),
                    KeyCode::Backspace => {
                        filter.pop();
                        *selected = 0;
                    }
                    KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                        filter.push(c);
                        *selected = 0;
                    }
                    KeyCode::Enter => {
                        let choice = filtered(items, filter).get(*selected).map(|item| (*item).clone());
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
            Then::CloseDoc(index) => self.editor.close(index),
            Then::CloseChat(id) => self.chats.close(id),
            Then::ChatAgent => {
                let agent = AGENTS.iter().position(|agent| value.starts_with(agent.name)).ok_or("Unknown agent")?;
                let ready: Vec<Provider> = Provider::ALL.into_iter().filter(|p| p.api_key().is_some()).collect();
                match ready.as_slice() {
                    [] => {
                        let vars: Vec<&str> = Provider::ALL.iter().map(|p| p.key_env()).collect();
                        return Err(format!("No API keys found. Set one of {} and restart.", vars.join(", ")));
                    }
                    [only] => self.chat_provider_chosen(agent, *only),
                    many => {
                        let items = many.iter().map(|p| p.name().to_owned()).collect();
                        self.modal = Some(Modal::pick("Pick a provider", items, Then::ChatProvider(agent)));
                    }
                }
            }
            Then::ChatProvider(agent) => {
                let provider = Provider::ALL.into_iter().find(|p| p.name() == value).ok_or("Unknown provider")?;
                self.chat_provider_chosen(agent, provider);
            }
            Then::ChatModel(agent, provider) => {
                self.chats.save_model(provider, value).map_err(|err| format!("Couldn't save the model choice: {err}"))?;
                self.create_chat(agent, provider, value.to_owned());
            }
            Then::SwitchModel(id) => {
                let chat = self.chats.tabs.iter_mut().find(|chat| chat.id == id).ok_or("That chat is closed")?;
                chat.model = value.to_owned();
                let provider = chat.provider;
                self.chats.save_model(provider, value).map_err(|err| format!("Couldn't save the model choice: {err}"))?;
            }
            Then::GoToLine => {
                let line = value.trim().parse().map_err(|_| format!("Not a line number: {value}"))?;
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
                self.editor.doc().ok_or("No file open")?.save_to(&path).map_err(text)?;
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
            Some(Modal::Pick { filter, selected, .. }) => {
                filter.push_str(&text.replace(['\r', '\n'], " "));
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
        frame.render_widget(Paragraph::new(self.status_line()), self.areas.status);
        if let Some(modal) = &self.modal {
            draw_modal(frame, modal);
        }
    }

    fn status_line(&self) -> Line<'static> {
        let (text, style) = match &self.message {
            Some(message) => (message.clone(), Style::new().fg(Color::Yellow)),
            None if self.focus != Focus::Chat && self.chats.any_approving() => {
                ("An agent is waiting for your approval. Alt+4 to answer.".to_owned(), Style::new().fg(Color::Black).bg(Color::Yellow))
            }
            None => {
                let hints = match self.focus {
                    Focus::Files => files::HINTS,
                    Focus::Editor => editor::HINTS,
                    Focus::Terminal => self.terminals.hints(),
                    Focus::Chat => self.chats.hints(),
                };
                (hints.to_owned(), Style::new().fg(Color::DarkGray))
            }
        };
        Line::from(vec![
            Span::styled(self.focus.title(), Style::new().fg(Color::Black).bg(Color::Cyan)),
            Span::styled(format!(" {text}"), style),
        ])
    }
}

/// The bordered frame around a pane, highlighted when focused.
pub fn pane_block(pane: Focus, focused: bool) -> Block<'static> {
    let (border, title) = if focused {
        (Style::new().fg(Color::Cyan), Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD))
    } else {
        (Style::new().fg(Color::DarkGray), Style::new().fg(Color::Gray))
    };
    Block::bordered().border_style(border).title(Span::styled(pane.title(), title))
}

fn draw_modal(frame: &mut Frame, modal: &Modal) {
    let screen = frame.area();
    let (title, list_rows) = match modal {
        Modal::Input { title, .. } | Modal::Confirm { title, .. } => (title, 0),
        Modal::Pick { title, items, filter, .. } => (title, filtered(items, filter).len().clamp(1, 14) as u16 + 1),
    };
    let wanted = match modal {
        Modal::Pick { items, .. } => items.iter().map(|item| item.width().min(200) as u16 + 4).max().unwrap_or(0),
        _ => 0,
    };
    let width = (title.width() as u16 + 6).max(wanted).clamp(44, 96).min(screen.width);
    let height = (3 + list_rows).min(screen.height);
    let area = Rect {
        x: screen.x + (screen.width - width) / 2,
        y: screen.y + (screen.height - height) / 3,
        width,
        height,
    };
    let block = Block::bordered()
        .border_style(Style::new().fg(Color::Yellow))
        .title(Span::styled(format!(" {title} "), Style::new().fg(Color::Yellow)));
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
            frame.render_widget(Paragraph::new(render_line(line, &[], selected, left, inner.width as usize)), inner);
            frame.set_cursor_position((inner.x + (display - left) as u16, inner.y));
        }
        Modal::Pick { items, filter, selected, .. } => {
            let matches = filtered(items, filter);
            let rows = inner.height.saturating_sub(1) as usize;
            let top = (*selected + 1).saturating_sub(rows);
            let mut lines = vec![Line::from(vec![
                Span::styled("filter: ", Style::new().fg(Color::DarkGray)),
                Span::raw(filter.clone()),
            ])];
            if matches.is_empty() {
                lines.push(Line::from(Span::styled("(no matches)", Style::new().fg(Color::DarkGray))));
            }
            for (index, item) in matches.iter().enumerate().skip(top).take(rows) {
                let style = if index == *selected { Style::new().fg(Color::Black).bg(Color::Yellow) } else { Style::new() };
                lines.push(Line::from(Span::styled((*item).clone(), style)));
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
        assert_eq!(app.editor.docs[0].path.as_deref(), Some(dir.join("new.txt").as_path()));

        type_text(&mut app, "d");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(dir.join("new.txt").is_file(), "Enter and Esc must not confirm a delete");
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
        assert!(app.modal.is_none() && app.editor.docs.is_empty(), "clean tab closes straight away");

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
        assert_eq!(hits.at(5, 2), Some(8), "14-column tabs: only 8 and 9 fit in 40 columns");
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

    #[test]
    fn all_panes_render_with_titles() {
        let mut app = test_app();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let text = terminal.backend().buffer().content().iter().map(|c| c.symbol()).collect::<String>();
        for title in ["1 Files", "2 Terminal", "3 Editor", "4 Chat"] {
            assert!(text.contains(title), "missing {title}");
        }
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
        app.modal = Some(Modal::pick("Pick", vec!["a very long item ".repeat(20); 40], Then::ChatAgent));
        draw_all(&mut app);
        app.modal = Some(Modal::Confirm { title: "A long question ".repeat(20), then: Then::Quit });
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
