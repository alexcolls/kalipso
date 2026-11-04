use std::collections::{BTreeMap, HashMap};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use syntect::highlighting::{
    FontStyle, HighlightIterator, HighlightState, Highlighter, Theme, ThemeSet,
};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::app::{TabHits, tab_line};
use crate::git::{Mark, line_marks};
use crate::lsp::Diagnostic;

const TAB_WIDTH: usize = 4;
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Parser state is cached this many lines apart.
const CHECKPOINT_EVERY: usize = 256;
/// Longer lines (minified bundles) render plain: syntect gets very slow on them.
const MAX_HIGHLIGHT_LINE: usize = 4096;
pub const SELECTION_BG: Color = Color::Rgb(60, 70, 95);

const HINTS: &str = "Ctrl+S save  Ctrl+P open  Alt+F search  F12 definition  Shift+F12 back  F8 problems  Ctrl+Z/Y undo  Ctrl+F find  Ctrl+G line";
const PROPOSAL_HINTS: &str =
    "Proposed by an agent, read-only.  y allow  n decline  a always  Esc back to chat";

/// A position in a buffer. `col` is a byte offset, always on a char boundary.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct Pos {
    pub line: usize,
    pub col: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EditKind {
    Typing,
    Backspace,
    Other,
}

struct Edit {
    start: Pos,
    removed: String,
    inserted: String,
    before: Pos,
    after: Pos,
    kind: EditKind,
}

#[derive(PartialEq, Eq, Debug)]
pub enum Outcome {
    Ignored,
    Moved,
    Edited,
    Copied(String),
}

/// Editable text with a cursor, selection and undo. Used by the editor and all text inputs.
// ponytail: Vec<String> is fine to about 1M lines; move to ropey if very large files matter.
pub struct Buffer {
    lines: Vec<String>,
    pub cursor: Pos,
    pub anchor: Option<Pos>,
    /// Display column that vertical moves try to keep.
    goal: Option<usize>,
    undo: Vec<Edit>,
    redo: Vec<Edit>,
    /// Undo depth that matches the file on disk. `None` when that state is unreachable.
    saved: Option<usize>,
    /// Stops the next edit merging into the previous undo step.
    sealed: bool,
    changed_from: Option<usize>,
    revision: u64,
}

fn end_of(start: Pos, text: &str) -> Pos {
    match text.rfind('\n') {
        Some(i) => Pos {
            line: start.line + text.matches('\n').count(),
            col: text.len() - i - 1,
        },
        None => Pos {
            line: start.line,
            col: start.col + text.len(),
        },
    }
}

fn prev_boundary(line: &str, col: usize) -> usize {
    line[..col]
        .grapheme_indices(true)
        .next_back()
        .map_or(0, |(i, _)| i)
}

fn next_boundary(line: &str, col: usize) -> usize {
    line[col..]
        .graphemes(true)
        .next()
        .map_or(col, |g| col + g.len())
}

fn grapheme_width(grapheme: &str, display: usize) -> usize {
    match grapheme {
        "\t" => TAB_WIDTH - display % TAB_WIDTH,
        g if g.chars().next().is_some_and(char::is_control) => 1,
        g => g.width(),
    }
}

/// Display column of byte offset `col`, with tabs expanded.
pub fn col_to_display(line: &str, col: usize) -> usize {
    line[..col]
        .graphemes(true)
        .fold(0, |display, g| display + grapheme_width(g, display))
}

/// Byte offset of the grapheme at display column `target`.
pub fn display_to_col(line: &str, target: usize) -> usize {
    let mut display = 0;
    for (i, g) in line.grapheme_indices(true) {
        let width = grapheme_width(g, display);
        if display + width > target {
            return i;
        }
        display += width;
    }
    line.len()
}

#[derive(PartialEq)]
enum CharClass {
    Space,
    Word,
    Other,
}

fn class_of(c: char) -> CharClass {
    if c.is_whitespace() {
        CharClass::Space
    } else if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else {
        CharClass::Other
    }
}

impl Buffer {
    pub fn from_text(text: &str) -> Buffer {
        Buffer {
            lines: text.split('\n').map(str::to_owned).collect(),
            cursor: Pos::default(),
            anchor: None,
            goal: None,
            undo: Vec::new(),
            redo: Vec::new(),
            saved: Some(0),
            sealed: true,
            changed_from: None,
            revision: 0,
        }
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn is_dirty(&self) -> bool {
        self.saved != Some(self.undo.len())
    }

    pub fn mark_saved(&mut self) {
        self.saved = Some(self.undo.len());
        self.sealed = true;
    }

    /// Goes up with every change to the text.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Lowest line changed since the last call.
    fn take_changed(&mut self) -> Option<usize> {
        self.changed_from.take()
    }

    pub fn selection(&self) -> Option<(Pos, Pos)> {
        let anchor = self.anchor?;
        match anchor.cmp(&self.cursor) {
            std::cmp::Ordering::Less => Some((anchor, self.cursor)),
            std::cmp::Ordering::Greater => Some((self.cursor, anchor)),
            std::cmp::Ordering::Equal => None,
        }
    }

    pub fn selected_text(&self) -> Option<String> {
        self.selection().map(|(start, end)| self.slice(start, end))
    }

    fn slice(&self, start: Pos, end: Pos) -> String {
        if start.line == end.line {
            return self.lines[start.line][start.col..end.col].to_owned();
        }
        let mut out = self.lines[start.line][start.col..].to_owned();
        for line in &self.lines[start.line + 1..end.line] {
            out.push('\n');
            out.push_str(line);
        }
        out.push('\n');
        out.push_str(&self.lines[end.line][..end.col]);
        out
    }

    /// Replaces a range without touching undo history. Returns the end of the new text.
    fn splice(&mut self, start: Pos, end: Pos, text: &str) -> Pos {
        let head = self.lines[start.line][..start.col].to_owned();
        let tail = self.lines[end.line][end.col..].to_owned();
        let mut new: Vec<String> = text.split('\n').map(str::to_owned).collect();
        let last = new.len() - 1;
        let col = new[last].len() + if last == 0 { head.len() } else { 0 };
        new[0].insert_str(0, &head);
        new[last].push_str(&tail);
        self.lines.splice(start.line..=end.line, new);
        self.changed_from = Some(self.changed_from.map_or(start.line, |c| c.min(start.line)));
        self.revision += 1;
        Pos {
            line: start.line + last,
            col,
        }
    }

    fn apply(&mut self, start: Pos, end: Pos, text: &str, kind: EditKind) {
        let removed = self.slice(start, end);
        let before = self.cursor;
        let after = self.splice(start, end, text);
        self.cursor = after;
        self.anchor = None;
        self.goal = None;
        if self.saved > Some(self.undo.len()) {
            self.saved = None; // Editing after undoing past the save point orphans it.
        }
        self.redo.clear();
        if !self.sealed
            && let Some(last) = self.undo.last_mut()
            && last.kind == kind
        {
            match kind {
                EditKind::Typing if last.after == start => {
                    last.inserted.push_str(text);
                    last.after = after;
                    return;
                }
                EditKind::Backspace if end == last.start => {
                    last.start = start;
                    last.removed.insert_str(0, &removed);
                    last.after = after;
                    return;
                }
                _ => {}
            }
        }
        self.sealed = false;
        self.undo.push(Edit {
            start,
            removed,
            inserted: text.to_owned(),
            before,
            after,
            kind,
        });
    }

    pub fn undo(&mut self) -> bool {
        let Some(edit) = self.undo.pop() else {
            return false;
        };
        self.splice(
            edit.start,
            end_of(edit.start, &edit.inserted),
            &edit.removed,
        );
        self.cursor = edit.before;
        self.anchor = None;
        self.goal = None;
        self.sealed = true;
        self.redo.push(edit);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(edit) = self.redo.pop() else {
            return false;
        };
        self.splice(
            edit.start,
            end_of(edit.start, &edit.removed),
            &edit.inserted,
        );
        self.cursor = edit.after;
        self.anchor = None;
        self.goal = None;
        self.sealed = true;
        self.undo.push(edit);
        true
    }

    /// Replaces the selection, or inserts at the cursor.
    pub fn insert(&mut self, text: &str) {
        let (start, end) = self.selection().unwrap_or((self.cursor, self.cursor));
        self.sealed = true;
        self.apply(start, end, text, EditKind::Other);
        self.sealed = true;
    }

    fn insert_typed(&mut self, c: char) {
        let (start, end) = self.selection().unwrap_or((self.cursor, self.cursor));
        if start != end || c.is_whitespace() {
            self.sealed = true; // Selections and word gaps start a new undo step.
        }
        let mut utf8 = [0; 4];
        self.apply(start, end, c.encode_utf8(&mut utf8), EditKind::Typing);
    }

    /// Replaces the whole text as one undo step, keeping the cursor where it still fits.
    pub fn replace_all(&mut self, text: &str) {
        let cursor = self.cursor;
        let last = self.lines.len() - 1;
        let end = Pos {
            line: last,
            col: self.lines[last].len(),
        };
        self.sealed = true;
        self.apply(Pos::default(), end, text, EditKind::Other);
        self.sealed = true;
        self.cursor = self.clamp(cursor);
    }

    fn clamp(&self, pos: Pos) -> Pos {
        let line = pos.line.min(self.lines.len() - 1);
        let text = &self.lines[line];
        let mut col = pos.col.min(text.len());
        while !text.is_char_boundary(col) {
            col -= 1;
        }
        Pos { line, col }
    }

    pub fn set_cursor(&mut self, pos: Pos, extend: bool) {
        match (extend, self.anchor) {
            (true, None) => self.anchor = Some(self.cursor),
            (false, _) => self.anchor = None,
            _ => {}
        }
        self.cursor = self.clamp(pos);
        self.goal = None;
        self.sealed = true;
    }

    fn left(&self, pos: Pos) -> Pos {
        if pos.col > 0 {
            Pos {
                line: pos.line,
                col: prev_boundary(&self.lines[pos.line], pos.col),
            }
        } else if pos.line > 0 {
            Pos {
                line: pos.line - 1,
                col: self.lines[pos.line - 1].len(),
            }
        } else {
            pos
        }
    }

    fn right(&self, pos: Pos) -> Pos {
        let line = &self.lines[pos.line];
        if pos.col < line.len() {
            Pos {
                line: pos.line,
                col: next_boundary(line, pos.col),
            }
        } else if pos.line + 1 < self.lines.len() {
            Pos {
                line: pos.line + 1,
                col: 0,
            }
        } else {
            pos
        }
    }

    fn word_left(&self, pos: Pos) -> Pos {
        if pos.col == 0 {
            return self.left(pos);
        }
        let mut chars = self.lines[pos.line][..pos.col]
            .char_indices()
            .rev()
            .peekable();
        let mut col = pos.col;
        while let Some((i, _)) = chars.next_if(|(_, c)| c.is_whitespace()) {
            col = i;
        }
        if let Some(&(_, first)) = chars.peek() {
            let class = class_of(first);
            while let Some((i, _)) = chars.next_if(|(_, c)| class_of(*c) == class) {
                col = i;
            }
        }
        Pos {
            line: pos.line,
            col,
        }
    }

    fn word_right(&self, pos: Pos) -> Pos {
        let line = &self.lines[pos.line];
        if pos.col == line.len() {
            return self.right(pos);
        }
        let mut chars = line[pos.col..].char_indices().peekable();
        let mut col = line.len() - pos.col;
        if let Some(&(_, first)) = chars.peek() {
            let class = class_of(first);
            while chars.next_if(|(_, c)| class_of(*c) == class).is_some() {}
        }
        while chars.next_if(|(_, c)| c.is_whitespace()).is_some() {}
        if let Some(&(i, _)) = chars.peek() {
            col = i;
        }
        Pos {
            line: pos.line,
            col: pos.col + col,
        }
    }

    fn vertical(&mut self, delta: isize, extend: bool) {
        let display = col_to_display(&self.lines[self.cursor.line], self.cursor.col);
        let goal = self.goal.unwrap_or(display);
        let last = self.lines.len() as isize - 1;
        let target = self.cursor.line as isize + delta;
        let pos = if target < 0 {
            Pos::default()
        } else if target > last {
            Pos {
                line: last as usize,
                col: self.lines[last as usize].len(),
            }
        } else {
            let line = target as usize;
            Pos {
                line,
                col: display_to_col(&self.lines[line], goal),
            }
        };
        self.set_cursor(pos, extend);
        self.goal = Some(goal);
    }

    fn indent_unit(&self) -> &'static str {
        if self
            .lines
            .iter()
            .take(200)
            .any(|line| line.starts_with('\t'))
        {
            "\t"
        } else {
            "    "
        }
    }

    /// Selected whole lines, as a first..=last range.
    fn selected_lines(&self) -> (usize, usize) {
        match self.selection() {
            Some((start, end)) if end.col == 0 && end.line > start.line => {
                (start.line, end.line - 1)
            }
            Some((start, end)) => (start.line, end.line),
            None => (self.cursor.line, self.cursor.line),
        }
    }

    fn map_lines(&mut self, change: impl Fn(&str, &str) -> String) {
        let (first, last) = self.selected_lines();
        let unit = self.indent_unit();
        let old = &self.lines[first..=last];
        let new: Vec<String> = old.iter().map(|line| change(line, unit)).collect();
        if new == old {
            return;
        }
        let keep_selection = self.anchor.is_some();
        let end = Pos {
            line: last,
            col: self.lines[last].len(),
        };
        self.sealed = true;
        self.apply(
            Pos {
                line: first,
                col: 0,
            },
            end,
            &new.join("\n"),
            EditKind::Other,
        );
        self.sealed = true;
        if keep_selection {
            self.anchor = Some(Pos {
                line: first,
                col: 0,
            });
        }
    }

    fn delete_range_or(&mut self, kind: EditKind, range: impl Fn(&Buffer) -> (Pos, Pos)) {
        if let Some((start, end)) = self.selection() {
            self.sealed = true;
            self.apply(start, end, "", EditKind::Other);
        } else {
            let (start, end) = range(self);
            if start != end {
                self.apply(start, end, "", kind);
            }
        }
    }

    /// Text keys shared by the editor and text inputs.
    pub fn edit_key(&mut self, key: KeyEvent, page: usize, clipboard: &str) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        if key.modifiers.contains(KeyModifiers::ALT) {
            return Outcome::Ignored;
        }
        let cursor = self.cursor;
        let page = page.max(1) as isize;
        match key.code {
            KeyCode::Left if !shift && !ctrl && self.selection().is_some() => {
                let (start, _) = self.selection().unwrap();
                self.set_cursor(start, false);
            }
            KeyCode::Right if !shift && !ctrl && self.selection().is_some() => {
                let (_, end) = self.selection().unwrap();
                self.set_cursor(end, false);
            }
            KeyCode::Left if ctrl => self.set_cursor(self.word_left(cursor), shift),
            KeyCode::Right if ctrl => self.set_cursor(self.word_right(cursor), shift),
            KeyCode::Left => self.set_cursor(self.left(cursor), shift),
            KeyCode::Right => self.set_cursor(self.right(cursor), shift),
            KeyCode::Up => self.vertical(-1, shift),
            KeyCode::Down => self.vertical(1, shift),
            KeyCode::PageUp => self.vertical(-page, shift),
            KeyCode::PageDown => self.vertical(page, shift),
            KeyCode::Home if ctrl => self.set_cursor(Pos::default(), shift),
            KeyCode::End if ctrl => {
                let line = self.lines.len() - 1;
                self.set_cursor(
                    Pos {
                        line,
                        col: self.lines[line].len(),
                    },
                    shift,
                );
            }
            KeyCode::Home => {
                let text = &self.lines[cursor.line];
                let indent = text.len() - text.trim_start().len();
                let col = if cursor.col == indent { 0 } else { indent };
                self.set_cursor(
                    Pos {
                        line: cursor.line,
                        col,
                    },
                    shift,
                );
            }
            KeyCode::End => self.set_cursor(
                Pos {
                    line: cursor.line,
                    col: self.lines[cursor.line].len(),
                },
                shift,
            ),
            // Many terminals send Ctrl+Backspace as Ctrl+H.
            KeyCode::Backspace if ctrl => {
                self.delete_range_or(EditKind::Other, |b| (b.word_left(b.cursor), b.cursor));
                return Outcome::Edited;
            }
            KeyCode::Char('h') if ctrl => {
                self.delete_range_or(EditKind::Other, |b| (b.word_left(b.cursor), b.cursor));
                return Outcome::Edited;
            }
            KeyCode::Backspace => {
                self.delete_range_or(EditKind::Backspace, |b| (b.left(b.cursor), b.cursor));
                return Outcome::Edited;
            }
            KeyCode::Delete if ctrl => {
                self.delete_range_or(EditKind::Other, |b| (b.cursor, b.word_right(b.cursor)));
                return Outcome::Edited;
            }
            KeyCode::Delete => {
                self.delete_range_or(EditKind::Other, |b| (b.cursor, b.right(b.cursor)));
                return Outcome::Edited;
            }
            KeyCode::Enter => {
                let text = &self.lines[cursor.line];
                let indent = &text[..text.len() - text.trim_start().len()];
                let indent = &indent[..indent.len().min(cursor.col)];
                self.insert(&format!("\n{indent}"));
                return Outcome::Edited;
            }
            KeyCode::Tab if self.selection().is_some() => {
                self.map_lines(|line, unit| {
                    if line.is_empty() {
                        String::new()
                    } else {
                        format!("{unit}{line}")
                    }
                });
                return Outcome::Edited;
            }
            KeyCode::Tab => {
                let unit = self.indent_unit();
                if unit == "\t" {
                    self.insert("\t");
                } else {
                    let display = col_to_display(&self.lines[cursor.line], cursor.col);
                    self.insert(&" ".repeat(TAB_WIDTH - display % TAB_WIDTH));
                }
                return Outcome::Edited;
            }
            KeyCode::BackTab => {
                self.map_lines(|line, unit| {
                    line.strip_prefix(unit)
                        .or_else(|| line.strip_prefix('\t'))
                        .unwrap_or_else(|| line.trim_start_matches(' '))
                        .to_owned()
                });
                return Outcome::Edited;
            }
            KeyCode::Char('z') if ctrl && shift => {
                self.redo();
                return Outcome::Edited;
            }
            KeyCode::Char('z') if ctrl => {
                self.undo();
                return Outcome::Edited;
            }
            KeyCode::Char('y') if ctrl => {
                self.redo();
                return Outcome::Edited;
            }
            KeyCode::Char('a') if ctrl => {
                let line = self.lines.len() - 1;
                self.anchor = Some(Pos::default());
                self.cursor = Pos {
                    line,
                    col: self.lines[line].len(),
                };
            }
            KeyCode::Char('c') if ctrl => return Outcome::Copied(self.selected_or_line()),
            KeyCode::Char('x') if ctrl => {
                let text = self.selected_or_line();
                if self.selection().is_some() {
                    self.insert("");
                } else {
                    self.delete_line();
                }
                return Outcome::Copied(text);
            }
            KeyCode::Char('v') if ctrl => {
                if !clipboard.is_empty() {
                    self.insert(clipboard);
                }
                return Outcome::Edited;
            }
            KeyCode::Char(c) if !ctrl => {
                self.insert_typed(c);
                return Outcome::Edited;
            }
            _ => return Outcome::Ignored,
        }
        Outcome::Moved
    }

    fn selected_or_line(&self) -> String {
        self.selected_text()
            .unwrap_or_else(|| format!("{}\n", self.lines[self.cursor.line]))
    }

    fn delete_line(&mut self) {
        let line = self.cursor.line;
        let (start, end) = if line + 1 < self.lines.len() {
            (
                Pos { line, col: 0 },
                Pos {
                    line: line + 1,
                    col: 0,
                },
            )
        } else if line > 0 {
            (
                Pos {
                    line: line - 1,
                    col: self.lines[line - 1].len(),
                },
                Pos {
                    line,
                    col: self.lines[line].len(),
                },
            )
        } else {
            (
                Pos::default(),
                Pos {
                    line: 0,
                    col: self.lines[0].len(),
                },
            )
        };
        self.sealed = true;
        self.apply(start, end, "", EditKind::Other);
        self.sealed = true;
    }

    /// Finds `query` after (or before) the cursor, wrapping once, and selects it.
    /// All-lowercase queries match case-insensitively.
    pub fn find(&mut self, query: &str, forward: bool) -> bool {
        let Some(re) = literal_regex(query) else {
            return false;
        };
        let from = self.selection().map_or(
            self.cursor,
            |(start, end)| if forward { end } else { start },
        );
        let total = self.lines.len();
        let found = if forward {
            (0..=total).find_map(|i| {
                let line = (from.line + i) % total;
                let min = if i == 0 { from.col } else { 0 };
                re.find_iter(&self.lines[line])
                    .find(|m| m.start() >= min)
                    .map(|m| (line, m.range()))
            })
        } else {
            (0..=total).find_map(|i| {
                let line = (from.line + total - i % total) % total;
                let max = if i == 0 { from.col } else { usize::MAX };
                re.find_iter(&self.lines[line])
                    .filter(|m| m.end() <= max)
                    .last()
                    .map(|m| (line, m.range()))
            })
        };
        let Some((line, range)) = found else {
            return false;
        };
        self.set_cursor(
            Pos {
                line,
                col: range.start,
            },
            false,
        );
        self.set_cursor(
            Pos {
                line,
                col: range.end,
            },
            true,
        );
        true
    }
}

/// Matches `query` as plain text. All-lowercase queries ignore case.
pub fn literal_regex(query: &str) -> Option<regex::Regex> {
    let insensitive = !query.chars().any(char::is_uppercase);
    regex::RegexBuilder::new(&regex::escape(query))
        .case_insensitive(insensitive)
        .build()
        .ok()
}

/// Reads a file as text, refusing anything that couldn't be saved back unchanged.
pub fn load_text(path: &Path) -> Result<(String, bool), String> {
    let meta = fs::metadata(path).map_err(|e| e.to_string())?;
    // A pipe blocks forever and a device like /dev/zero never ends, yet both report size 0.
    // A project can contain either, or a symlink to one.
    if !meta.is_file() {
        return Err("not a regular file".to_owned());
    }
    let size = meta.len();
    if size > MAX_FILE_BYTES {
        return Err(format!("too large to edit ({} MB)", size / (1024 * 1024)));
    }
    let bytes = fs::read(path).map_err(|e| e.to_string())?;
    if bytes.contains(&0) {
        return Err("binary file".to_owned());
    }
    let text = String::from_utf8(bytes).map_err(|_| "not valid UTF-8".to_owned())?;
    // Only all-CRLF files use CRLF mode. In mixed files the stray \r stays as line content,
    // so saving still reproduces the original bytes.
    let newlines = text.matches('\n').count();
    if newlines > 0 && text.matches("\r\n").count() == newlines {
        return Ok((text.replace("\r\n", "\n"), true));
    }
    Ok((text, false))
}

/// Creates the temp file for a save. It holds what the real file holds, so it must never be
/// readable by anyone the real file isn't, not even for the moment before it's renamed.
fn open_temp(tmp: &Path, existing: Option<&fs::Metadata>) -> io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    if let Some(meta) = existing {
        options.mode(meta.permissions().mode() & 0o7777);
    }
    options.open(tmp)
}

/// Writes through a temp file and rename, so a crash can't leave a half-written file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::other("not a file path"))?;
    let dir = target
        .parent()
        .ok_or_else(|| io::Error::other("no parent directory"))?;
    let tmp = dir.join(format!(
        ".{}.kalipso-{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    let _ = fs::remove_file(&tmp);
    let existing = fs::metadata(&target).ok();
    let result = (|| {
        let mut file = open_temp(&tmp, existing.as_ref())?;
        file.write_all(bytes)?;
        // The umask may have narrowed what open_temp asked for.
        if let Some(meta) = &existing {
            file.set_permissions(meta.permissions())?;
        }
        file.sync_all()?;
        fs::rename(&tmp, &target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// What the file looked like when we last read or wrote it.
type Stamp = (SystemTime, u64);

fn stamp_of(path: &Path) -> Option<Stamp> {
    let meta = fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Conflict {
    Changed,
    Deleted,
}

pub struct Syntax {
    pub set: SyntaxSet,
    pub theme: Theme,
}

/// Loaded once. `main` starts this on a background thread so opening a file stays snappy.
pub fn syntax() -> &'static Syntax {
    static SYNTAX: OnceLock<Syntax> = OnceLock::new();
    SYNTAX.get_or_init(|| {
        let mut themes = ThemeSet::load_defaults();
        Syntax {
            set: SyntaxSet::load_defaults_newlines(),
            theme: themes
                .themes
                .remove("base16-ocean.dark")
                .expect("bundled theme"),
        }
    })
}

/// The syntax for a language name or file extension, or plain text.
pub fn syntax_for_token(token: &str) -> &'static SyntaxReference {
    let set = &syntax().set;
    // ponytail: syntect's bundled grammars lack TypeScript and TOML. Add two-face if that matters.
    let token = match token {
        "ts" | "tsx" | "mts" | "cts" | "jsx" | "typescript" => "js",
        other => other,
    };
    set.find_syntax_by_token(token)
        .unwrap_or_else(|| set.find_syntax_plain_text())
}

fn syntax_for(path: Option<&Path>, first_line: &str) -> &'static SyntaxReference {
    let set = &syntax().set;
    let by_path = path
        .and_then(|p| p.extension())
        .and_then(|e| e.to_str())
        .map(syntax_for_token);
    match by_path {
        Some(found) if found.name != "Plain Text" => found,
        _ => set
            .find_syntax_by_first_line(first_line)
            .unwrap_or_else(|| set.find_syntax_plain_text()),
    }
}

pub fn to_style(style: syntect::highlighting::Style) -> Style {
    let fg = style.foreground;
    let mut out = Style::new().fg(Color::Rgb(fg.r, fg.g, fg.b));
    if style.font_style.contains(FontStyle::BOLD) {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    out
}

/// Colours for one line, as (end byte, style) runs.
type Runs = Vec<(usize, Style)>;

/// Incremental highlighting. Visible lines are parsed from the nearest cached parser
/// state, and an edit only invalidates from the edited line down.
struct Highlight {
    syntax: &'static SyntaxReference,
    checkpoints: Vec<(ParseState, HighlightState)>,
    cache: BTreeMap<usize, Runs>,
}

impl Highlight {
    fn new(syntax: &'static SyntaxReference) -> Highlight {
        Highlight {
            syntax,
            checkpoints: Vec::new(),
            cache: BTreeMap::new(),
        }
    }

    fn invalidate(&mut self, line: usize) {
        self.cache.split_off(&line);
        self.checkpoints.truncate(line / CHECKPOINT_EVERY + 1);
    }

    /// Makes sure `first..last` is in the cache.
    fn prepare(&mut self, lines: &[String], first: usize, last: usize) {
        let last = last.min(lines.len());
        let Some(missing) = (first..last).find(|line| !self.cache.contains_key(line)) else {
            return;
        };
        let Syntax { set, theme } = syntax();
        let highlighter = Highlighter::new(theme);
        if self.checkpoints.is_empty() {
            let state = HighlightState::new(&highlighter, ScopeStack::new());
            self.checkpoints.push((ParseState::new(self.syntax), state));
        }
        let start = (missing / CHECKPOINT_EVERY).min(self.checkpoints.len() - 1) * CHECKPOINT_EVERY;
        let (mut parse, mut high) = self.checkpoints[start / CHECKPOINT_EVERY].clone();
        for (index, line) in lines.iter().enumerate().take(last).skip(start) {
            if index % CHECKPOINT_EVERY == 0 && index / CHECKPOINT_EVERY == self.checkpoints.len() {
                self.checkpoints.push((parse.clone(), high.clone()));
            }
            let long = line.len() > MAX_HIGHLIGHT_LINE;
            let text = if long {
                "\n".to_owned()
            } else {
                format!("{line}\n")
            };
            let mut runs = Vec::new();
            if let Ok(ops) = parse.parse_line(&text, set) {
                let mut end = 0;
                for (style, piece) in HighlightIterator::new(&mut high, &ops, &text, &highlighter) {
                    end += piece.len();
                    runs.push((end.min(line.len()), to_style(style)));
                }
            }
            self.cache
                .insert(index, if long { Vec::new() } else { runs });
        }
    }
}

/// One line as styled spans, clipped to the viewport. Tabs and control characters are
/// drawn as spaces and dots, because they would otherwise corrupt the screen.
pub fn render_line(
    line: &str,
    runs: &[(usize, Style)],
    selected: Option<(usize, usize)>,
    x: usize,
    width: usize,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut push = |text: &str, style: Style| match spans.last_mut() {
        Some(last) if last.style == style => last.content.to_mut().push_str(text),
        _ => spans.push(Span::styled(text.to_owned(), style)),
    };
    let mut display = 0;
    let mut run = 0;
    for (i, g) in line.grapheme_indices(true) {
        let w = grapheme_width(g, display);
        if display >= x + width {
            break;
        }
        while run < runs.len() && runs[run].0 <= i {
            run += 1;
        }
        let mut style = runs.get(run).map_or(Style::new(), |r| r.1);
        if selected.is_some_and(|(start, end)| i >= start && i < end) {
            style = style.bg(SELECTION_BG);
        }
        let visible_from = display.max(x);
        let visible_to = (display + w).min(x + width);
        if visible_to > visible_from {
            let is_control = g.chars().next().is_some_and(char::is_control);
            if g == "\t" || visible_to - visible_from < w {
                push(&" ".repeat(visible_to - visible_from), style);
            } else if is_control {
                push("·", style.fg(Color::DarkGray));
            } else {
                push(g, style);
            }
        }
        display += w;
    }
    // Show that a selection continues past the end of this line.
    if let Some((_, end)) = selected
        && end > line.len()
        && display >= x
        && display < x + width
    {
        push(" ", Style::new().bg(SELECTION_BG));
    }
    Line::from(spans)
}

pub struct Doc {
    pub path: Option<PathBuf>,
    pub buf: Buffer,
    crlf: bool,
    top: usize,
    left: usize,
    follow_cursor: bool,
    disk: Option<Stamp>,
    pub conflict: Option<Conflict>,
    highlight: Highlight,
    /// This file's lines at git HEAD, if it has any, and the HEAD they came from.
    base: Option<Vec<String>>,
    base_head: Option<String>,
    marks: Vec<Mark>,
    marks_stale: bool,
    /// Set when this read-only tab shows an agent's proposed change: which chat and tool call.
    proposal: Option<(u64, String)>,
    /// How many times this has been saved, so language servers can be told.
    pub saves: u32,
}

impl Doc {
    fn new(path: Option<PathBuf>, text: &str, crlf: bool) -> Doc {
        let first_line = text.lines().next().unwrap_or("");
        let disk = path.as_deref().and_then(stamp_of);
        Doc {
            highlight: Highlight::new(syntax_for(path.as_deref(), first_line)),
            path,
            buf: Buffer::from_text(text),
            crlf,
            top: 0,
            left: 0,
            follow_cursor: true,
            disk,
            conflict: None,
            base: None,
            base_head: None,
            marks: Vec::new(),
            marks_stale: false,
            proposal: None,
            saves: 0,
        }
    }

    pub fn is_proposal(&self) -> bool {
        self.proposal.is_some()
    }

    /// Sets the text the gutter compares against.
    fn set_base(&mut self, text: Option<&str>, head: Option<String>) {
        self.base = text.map(|text| {
            let text = if self.crlf {
                text.replace("\r\n", "\n")
            } else {
                text.to_owned()
            };
            text.split('\n').map(str::to_owned).collect()
        });
        self.base_head = head;
        self.marks_stale = true;
    }

    pub fn title(&self) -> String {
        let name = self
            .path
            .as_deref()
            .and_then(Path::file_name)
            .map_or("untitled".into(), |n| n.to_string_lossy());
        if self.proposal.is_some() {
            format!("{name} (proposed)")
        } else if self.buf.is_dirty() {
            format!("{name} ●")
        } else {
            name.into_owned()
        }
    }

    pub fn save_to(&mut self, path: &Path) -> io::Result<()> {
        let text = self.buf.text();
        let bytes = if self.crlf {
            text.replace('\n', "\r\n")
        } else {
            text
        };
        write_atomic(path, bytes.as_bytes())?;
        if self.path.as_deref() != Some(path) {
            self.highlight = Highlight::new(syntax_for(
                Some(path),
                self.buf.lines().first().map_or("", String::as_str),
            ));
            self.path = Some(path.to_path_buf());
        }
        self.disk = stamp_of(path);
        self.conflict = None;
        self.buf.mark_saved();
        self.saves += 1;
        Ok(())
    }

    /// Replaces the buffer with the file on disk. The reload itself can be undone.
    pub fn reload(&mut self) -> Result<(), String> {
        let Some(path) = self.path.clone() else {
            return Ok(());
        };
        let (text, crlf) = load_text(&path)?;
        if text != self.buf.text() {
            self.buf.replace_all(&text);
        }
        self.crlf = crlf;
        self.disk = stamp_of(&path);
        self.conflict = None;
        self.buf.mark_saved();
        Ok(())
    }

    /// Reacts to changes made outside this buffer. Returns whether anything changed.
    fn check_disk(&mut self) -> bool {
        // A proposal differs from the file on purpose.
        if self.proposal.is_some() {
            return false;
        }
        let Some(path) = self.path.clone() else {
            return false;
        };
        let now = stamp_of(&path);
        if now == self.disk {
            return false;
        }
        match now {
            None => {
                self.disk = None;
                self.conflict = Some(Conflict::Deleted);
            }
            Some(_) if !self.buf.is_dirty() && self.conflict != Some(Conflict::Deleted) => {
                if self.reload().is_err() {
                    self.disk = now;
                    self.conflict = Some(Conflict::Changed);
                }
            }
            Some(_) => {
                self.disk = now;
                self.conflict = Some(Conflict::Changed);
            }
        }
        true
    }
}

pub enum EditorCmd {
    None,
    SaveAs,
    ConfirmOverwrite,
    Find,
    GoToLine,
    Copied(String),
    Message(String),
    Definition,
    NextProblem,
}

#[derive(Default)]
pub struct Editor {
    pub docs: Vec<Doc>,
    pub active: usize,
    pub find_query: String,
    view: Rect,
    gutter: u16,
    tabs: TabHits,
    selecting: bool,
    /// The tab to return to when a proposal closes.
    before_proposal: Option<usize>,
    pub diagnostics: HashMap<PathBuf, Vec<Diagnostic>>,
    /// Where go-to-definition came from, for going back.
    jumps: Vec<(PathBuf, Pos)>,
}

impl Editor {
    pub fn doc(&mut self) -> Option<&mut Doc> {
        self.docs.get_mut(self.active)
    }

    pub fn has_unsaved(&self) -> bool {
        self.docs.iter().any(|doc| doc.buf.is_dirty())
    }

    /// Opens a file, or switches to it if it is already open.
    pub fn open(&mut self, path: &Path) -> Result<(), String> {
        if let Some(i) = self
            .docs
            .iter()
            .position(|doc| doc.proposal.is_none() && doc.path.as_deref() == Some(path))
        {
            self.active = i;
            return Ok(());
        }
        let (text, crlf) = load_text(path)?;
        self.docs
            .push(Doc::new(Some(path.to_path_buf()), &text, crlf));
        self.active = self.docs.len() - 1;
        Ok(())
    }

    /// Shows an agent's proposed change in a read-only tab, marked against the current file.
    pub fn open_proposal(
        &mut self,
        chat: u64,
        call: String,
        path: &Path,
        current: &str,
        proposed: &str,
    ) {
        let id = (chat, call);
        if let Some(index) = self
            .docs
            .iter()
            .position(|doc| doc.proposal.as_ref() == Some(&id))
        {
            self.active = index;
            return;
        }
        let mut doc = Doc::new(Some(path.to_path_buf()), proposed, false);
        doc.set_base(Some(current), None);
        doc.proposal = Some(id);
        self.before_proposal.get_or_insert(self.active);
        self.docs.push(doc);
        self.active = self.docs.len() - 1;
    }

    /// The chat waiting on the proposal in the active tab, if it shows one.
    pub fn active_proposal(&self) -> Option<u64> {
        self.docs
            .get(self.active)?
            .proposal
            .as_ref()
            .map(|(chat, _)| *chat)
    }

    /// Closes proposal tabs for questions that have been answered.
    pub fn retain_proposals(&mut self, still_open: impl Fn(u64, &str) -> bool) {
        let count = self.docs.len();
        self.docs.retain(|doc| {
            doc.proposal
                .as_ref()
                .is_none_or(|(chat, call)| still_open(*chat, call))
        });
        if self.docs.len() == count {
            return;
        }
        // Proposals open after every real tab, so its index is still right.
        if !self.docs.iter().any(|doc| doc.proposal.is_some())
            && let Some(previous) = self.before_proposal.take()
        {
            self.active = previous;
        }
        self.active = self.active.min(self.docs.len().saturating_sub(1));
    }

    pub fn hints(&self) -> &'static str {
        if self.active_proposal().is_some() {
            PROPOSAL_HINTS
        } else {
            HINTS
        }
    }

    pub fn new_untitled(&mut self) {
        self.docs.push(Doc::new(None, "", false));
        self.active = self.docs.len() - 1;
    }

    pub fn close(&mut self, index: usize) {
        if index < self.docs.len() {
            self.docs.remove(index);
            self.active = self.active.min(self.docs.len().saturating_sub(1));
        }
    }

    pub fn cycle(&mut self, delta: isize) {
        if !self.docs.is_empty() {
            let count = self.docs.len() as isize;
            self.active = (self.active as isize + delta).rem_euclid(count) as usize;
        }
    }

    pub fn check_disk(&mut self) -> bool {
        let mut changed = false;
        for doc in &mut self.docs {
            changed |= doc.check_disk(); // Every doc must be checked, so no short-circuiting.
        }
        changed
    }

    /// Each open file with the HEAD its gutter is based on.
    pub fn known_bases(&self) -> Vec<(PathBuf, Option<String>)> {
        self.docs
            .iter()
            .filter(|doc| doc.proposal.is_none())
            .filter_map(|doc| Some((doc.path.clone()?, doc.base_head.clone())))
            .collect()
    }

    pub fn apply_bases(&mut self, head: &Option<String>, bases: &[(PathBuf, Option<String>)]) {
        for (path, text) in bases {
            for doc in self
                .docs
                .iter_mut()
                .filter(|doc| doc.proposal.is_none() && doc.path.as_ref() == Some(path))
            {
                doc.set_base(text.as_deref(), head.clone());
            }
        }
    }

    /// Keeps open buffers pointing at the right file after a rename in the file tree.
    pub fn path_renamed(&mut self, old: &Path, new: &Path) {
        for doc in &mut self.docs {
            if let Some(rest) = doc.path.as_deref().and_then(|p| p.strip_prefix(old).ok()) {
                let moved = new.join(rest);
                doc.disk = stamp_of(&moved);
                doc.path = Some(moved);
            }
        }
    }

    pub fn save(&mut self) -> EditorCmd {
        let Some(doc) = self.doc() else {
            return EditorCmd::None;
        };
        let Some(path) = doc.path.clone() else {
            return EditorCmd::SaveAs;
        };
        // Re-check now rather than trusting the last tick, so the file can't be clobbered.
        doc.check_disk();
        if doc.conflict == Some(Conflict::Changed) {
            return EditorCmd::ConfirmOverwrite;
        }
        match doc.save_to(&path) {
            Ok(()) => EditorCmd::None,
            Err(err) => EditorCmd::Message(format!("Save failed: {err}")),
        }
    }

    pub fn find_next(&mut self, forward: bool) -> EditorCmd {
        let query = self.find_query.clone();
        let Some(doc) = self.doc() else {
            return EditorCmd::None;
        };
        doc.follow_cursor = true;
        if query.is_empty() {
            return EditorCmd::Find;
        }
        if doc.buf.find(&query, forward) {
            EditorCmd::None
        } else {
            EditorCmd::Message(format!("Not found: {query}"))
        }
    }

    pub fn go_to_line(&mut self, line: usize) {
        if let Some(doc) = self.doc() {
            doc.buf.set_cursor(
                Pos {
                    line: line.saturating_sub(1),
                    col: 0,
                },
                false,
            );
            doc.follow_cursor = true;
        }
    }

    /// Moves the cursor after a jump. `column` is whatever the closure makes of this line.
    pub fn go_to(&mut self, line: usize, column: impl Fn(&str) -> usize) {
        if let Some(doc) = self.doc() {
            let line = line.min(doc.buf.lines().len() - 1);
            let col = column(&doc.buf.lines()[line]);
            doc.buf.set_cursor(Pos { line, col }, false);
            doc.follow_cursor = true;
        }
    }

    /// Opens a file at a position, remembering where this came from so Shift+F12 can go back.
    /// If the file can't be opened, nothing moves.
    pub fn jump_to(
        &mut self,
        path: &Path,
        line: usize,
        column: impl Fn(&str) -> usize,
    ) -> Result<(), String> {
        let from = self
            .docs
            .get(self.active)
            .and_then(|doc| Some((doc.path.clone()?, doc.buf.cursor)));
        self.open(path)?;
        self.jumps.extend(from);
        self.go_to(line, column);
        Ok(())
    }

    fn jump_back(&mut self) -> EditorCmd {
        let Some((path, position)) = self.jumps.pop() else {
            return EditorCmd::Message("Nowhere to go back to.".to_owned());
        };
        if let Err(err) = self.open(&path) {
            return EditorCmd::Message(format!("Can't go back: {err}"));
        }
        if let Some(doc) = self.doc() {
            doc.buf.set_cursor(position, false);
            doc.follow_cursor = true;
        }
        EditorCmd::None
    }

    /// Moves to the next problem in this file, wrapping around, and describes it.
    pub fn next_problem(&mut self) -> String {
        let Some(doc) = self.docs.get_mut(self.active) else {
            return "No file open.".to_owned();
        };
        let problems = doc
            .path
            .as_ref()
            .and_then(|path| self.diagnostics.get(path))
            .map_or(&[][..], Vec::as_slice);
        let Some(first) = problems.first() else {
            return "No problems reported in this file.".to_owned();
        };
        let cursor = doc.buf.cursor.line;
        let target = problems
            .iter()
            .find(|problem| problem.line > cursor)
            .unwrap_or(first);
        let count = problems.len();
        let text = format!(
            "{count} problem{}: {}",
            if count == 1 { "" } else { "s" },
            target.message
        );
        doc.buf.set_cursor(
            Pos {
                line: target.line,
                col: 0,
            },
            false,
        );
        doc.follow_cursor = true;
        text
    }

    pub fn paste(&mut self, text: &str) {
        if let Some(doc) = self.doc().filter(|doc| doc.proposal.is_none()) {
            doc.buf
                .insert(&text.replace("\r\n", "\n").replace('\r', "\n"));
            doc.follow_cursor = true;
        }
    }

    pub fn on_key(&mut self, key: KeyEvent, clipboard: &str) -> EditorCmd {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.view.height as usize;
        if self.active_proposal().is_some() {
            let moves = matches!(
                key.code,
                KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::PageUp
                    | KeyCode::PageDown
                    | KeyCode::Home
                    | KeyCode::End
            );
            let copies = ctrl && matches!(key.code, KeyCode::Char('c' | 'a'));
            let finds = (ctrl && key.code == KeyCode::Char('f')) || key.code == KeyCode::F(3);
            if !moves && !copies && !finds {
                return EditorCmd::None;
            }
        }
        match key.code {
            KeyCode::Char('s') if ctrl => return self.save(),
            KeyCode::Char('f') if ctrl => return EditorCmd::Find,
            KeyCode::Char('g') if ctrl => return EditorCmd::GoToLine,
            KeyCode::F(3) => return self.find_next(!key.modifiers.contains(KeyModifiers::SHIFT)),
            KeyCode::F(12) if key.modifiers.contains(KeyModifiers::SHIFT) => {
                return self.jump_back();
            }
            KeyCode::F(12) => return EditorCmd::Definition,
            KeyCode::F(8) => return EditorCmd::NextProblem,
            KeyCode::Char('r') if ctrl => {
                let Some(doc) = self.doc() else {
                    return EditorCmd::None;
                };
                return match doc.reload() {
                    Ok(()) => EditorCmd::None,
                    Err(err) => EditorCmd::Message(format!("Reload failed: {err}")),
                };
            }
            _ => {}
        }
        let Some(doc) = self.doc() else {
            return EditorCmd::None;
        };
        doc.follow_cursor = true;
        match doc.buf.edit_key(key, page, clipboard) {
            Outcome::Copied(text) => EditorCmd::Copied(text),
            _ => EditorCmd::None,
        }
    }

    pub fn on_mouse(&mut self, mouse: MouseEvent) {
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
            && let Some(index) = self.tabs.at(mouse.column, mouse.row)
        {
            self.active = index;
            return;
        }
        let (view, gutter) = (self.view, self.gutter);
        let Some(doc) = self.docs.get_mut(self.active) else {
            return;
        };
        let last = doc.buf.lines().len().saturating_sub(1);
        let position = |doc: &Doc| {
            let line = (doc.top + mouse.row.saturating_sub(view.y) as usize).min(last);
            let display = doc.left + mouse.column.saturating_sub(view.x + gutter) as usize;
            Pos {
                line,
                col: display_to_col(&doc.buf.lines()[line], display),
            }
        };
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                doc.top = doc.top.saturating_sub(3);
                doc.follow_cursor = false;
            }
            MouseEventKind::ScrollDown => {
                doc.top = (doc.top + 3).min(last);
                doc.follow_cursor = false;
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let pos = position(doc);
                doc.buf.set_cursor(pos, false);
                doc.follow_cursor = true;
                self.selecting = true;
            }
            MouseEventKind::Drag(MouseButton::Left) if self.selecting => {
                let pos = position(doc);
                doc.buf.set_cursor(pos, true);
                doc.follow_cursor = true;
            }
            MouseEventKind::Up(MouseButton::Left) => self.selecting = false,
            _ => {}
        }
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, focused: bool) {
        if self.docs.is_empty() {
            let hint = Paragraph::new("Ctrl+P finds a file by name. Alt+T starts a new one.")
                .style(Style::new().fg(Color::DarkGray));
            frame.render_widget(hint, area);
            return;
        }
        let titles: Vec<String> = self.docs.iter().map(Doc::title).collect();
        let (tabs, hits) = tab_line(&titles, self.active, area);
        self.tabs = hits;
        frame.render_widget(Paragraph::new(tabs), Rect { height: 1, ..area });
        let mut body = Rect {
            y: area.y + 1,
            height: area.height.saturating_sub(1),
            ..area
        };

        let doc = &mut self.docs[self.active];
        if let Some(conflict) = doc.conflict {
            let text = match conflict {
                Conflict::Changed => {
                    " Changed on disk. Ctrl+R reload  Ctrl+S overwrite (asks first)"
                }
                Conflict::Deleted => " Deleted on disk. Ctrl+S saves it again.",
            };
            let banner =
                Paragraph::new(text).style(Style::new().fg(Color::Black).bg(Color::Yellow));
            frame.render_widget(
                banner,
                Rect {
                    height: 1.min(body.height),
                    ..body
                },
            );
            body.y += 1;
            body.height = body.height.saturating_sub(1);
        }
        if body.is_empty() {
            return;
        }
        let count = doc.buf.lines().len();
        let gutter = count.to_string().len() as u16 + 2;
        self.view = body;
        self.gutter = gutter;
        let width = body.width.saturating_sub(gutter) as usize;
        let height = body.height as usize;

        let cursor = doc.buf.cursor;
        let cursor_display = col_to_display(&doc.buf.lines()[cursor.line], cursor.col);
        if doc.follow_cursor {
            doc.top = doc
                .top
                .min(cursor.line)
                .max((cursor.line + 1).saturating_sub(height));
            doc.left = doc
                .left
                .min(cursor_display)
                .max((cursor_display + 1).saturating_sub(width));
        }
        doc.top = doc.top.min(count - 1);

        if let Some(line) = doc.buf.take_changed() {
            doc.highlight.invalidate(line);
            doc.marks_stale = true;
        }
        if std::mem::take(&mut doc.marks_stale) {
            doc.marks = doc
                .base
                .as_ref()
                .map_or_else(Vec::new, |base| line_marks(base, doc.buf.lines()));
        }
        let last = (doc.top + height).min(count);
        doc.highlight.prepare(doc.buf.lines(), doc.top, last);

        let selection = doc.buf.selection();
        let mut rows = Vec::with_capacity(height);
        for index in doc.top..last {
            let text = &doc.buf.lines()[index];
            let selected = selection.and_then(|(start, end)| {
                if index < start.line || index > end.line {
                    return None;
                }
                let from = if index == start.line { start.col } else { 0 };
                let to = if index == end.line {
                    end.col
                } else {
                    usize::MAX
                };
                Some((from, to))
            });
            let runs = doc
                .highlight
                .cache
                .get(&index)
                .map_or(&[][..], Vec::as_slice);
            let mut row = render_line(text, runs, selected, doc.left, width);
            let problem = doc
                .path
                .as_ref()
                .and_then(|path| self.diagnostics.get(path))
                .and_then(|list| list.iter().find(|problem| problem.line == index));
            let problem_color = problem.map(|problem| match problem.severity {
                1 => Color::Red,
                2 => Color::Yellow,
                _ => Color::Blue,
            });
            if let (Some(problem), Some(color)) = (problem, problem_color) {
                row.spans.push(Span::styled(
                    format!("   {}", problem.message),
                    Style::new().fg(color).add_modifier(Modifier::ITALIC),
                ));
            }
            let number_style = match problem_color {
                Some(color) => Style::new().fg(color),
                None if index == cursor.line => Style::new().fg(Color::Gray),
                None => Style::new().fg(Color::DarkGray),
            };
            let (sign, sign_color) = match doc.marks.get(index).copied().unwrap_or_default() {
                Mark::None => (" ", Color::Reset),
                Mark::Added => ("▎", Color::Green),
                Mark::Changed => ("▎", Color::Yellow),
                Mark::Deleted => ("▁", Color::Red),
            };
            row.spans
                .insert(0, Span::styled(sign, Style::new().fg(sign_color)));
            row.spans.insert(
                0,
                Span::styled(
                    format!("{:>w$}", index + 1, w = gutter as usize - 1),
                    number_style,
                ),
            );
            rows.push(row);
        }
        frame.render_widget(Paragraph::new(rows), body);

        if focused
            && cursor.line >= doc.top
            && cursor.line < last
            && cursor_display >= doc.left
            && cursor_display < doc.left + width
        {
            frame.set_cursor_position((
                body.x + gutter + (cursor_display - doc.left) as u16,
                body.y + (cursor.line - doc.top) as u16,
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn with(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn typed(buf: &mut Buffer, text: &str) {
        for c in text.chars() {
            buf.edit_key(plain(KeyCode::Char(c)), 10, "");
        }
    }

    #[test]
    fn typing_undoes_by_word_and_redo_restores_it() {
        let mut buf = Buffer::from_text("");
        typed(&mut buf, "hello world");
        assert_eq!(buf.text(), "hello world");
        buf.undo();
        assert_eq!(buf.text(), "hello");
        buf.undo();
        assert_eq!(buf.text(), "");
        assert!(!buf.undo());
        buf.redo();
        buf.redo();
        assert_eq!(buf.text(), "hello world");
        assert_eq!(buf.cursor, Pos { line: 0, col: 11 });
    }

    #[test]
    fn multiline_edits_round_trip_through_undo() {
        let mut buf = Buffer::from_text("one\ntwo\nthree");
        buf.set_cursor(Pos { line: 0, col: 1 }, false);
        buf.set_cursor(Pos { line: 2, col: 2 }, true);
        assert_eq!(buf.selected_text().unwrap(), "ne\ntwo\nth");
        buf.insert("X\nY");
        assert_eq!(buf.text(), "oX\nYree");
        assert_eq!(buf.cursor, Pos { line: 1, col: 1 });
        buf.undo();
        assert_eq!(buf.text(), "one\ntwo\nthree");
        buf.redo();
        assert_eq!(buf.text(), "oX\nYree");
    }

    #[test]
    fn backspace_joins_lines_and_handles_graphemes() {
        let mut buf = Buffer::from_text("a\ne\u{301}");
        buf.set_cursor(
            Pos {
                line: 1,
                col: "e\u{301}".len(),
            },
            false,
        );
        buf.edit_key(plain(KeyCode::Backspace), 10, "");
        assert_eq!(buf.text(), "a\n");
        buf.edit_key(plain(KeyCode::Backspace), 10, "");
        assert_eq!(buf.text(), "a");
        buf.undo();
        assert_eq!(buf.text(), "a\ne\u{301}");
    }

    #[test]
    fn dirty_tracks_the_save_point_through_undo() {
        let mut buf = Buffer::from_text("x");
        assert!(!buf.is_dirty());
        buf.insert("a");
        assert!(buf.is_dirty());
        buf.mark_saved();
        assert!(!buf.is_dirty());
        buf.insert("b");
        assert!(buf.is_dirty());
        buf.undo();
        assert!(!buf.is_dirty(), "back at the save point");
        buf.undo();
        assert!(buf.is_dirty(), "before the save point");
        buf.insert("c");
        buf.undo();
        assert!(buf.is_dirty(), "the old save point is unreachable now");
    }

    #[test]
    fn typing_after_save_does_not_merge_into_saved_state() {
        let mut buf = Buffer::from_text("");
        typed(&mut buf, "ab");
        buf.mark_saved();
        typed(&mut buf, "cd");
        buf.undo();
        assert_eq!(buf.text(), "ab");
        assert!(!buf.is_dirty());
    }

    #[test]
    fn word_motion_stops_at_class_changes() {
        let mut buf = Buffer::from_text("foo.bar  baz");
        let ctrl = KeyModifiers::CONTROL;
        buf.edit_key(with(KeyCode::Right, ctrl), 10, "");
        assert_eq!(buf.cursor.col, 3);
        buf.edit_key(with(KeyCode::Right, ctrl), 10, "");
        assert_eq!(buf.cursor.col, 4);
        buf.edit_key(with(KeyCode::Right, ctrl), 10, "");
        assert_eq!(buf.cursor.col, 9);
        buf.edit_key(with(KeyCode::Right, ctrl), 10, "");
        assert_eq!(buf.cursor.col, 12);
        buf.edit_key(with(KeyCode::Left, ctrl), 10, "");
        assert_eq!(buf.cursor.col, 9);
        buf.edit_key(with(KeyCode::Left, ctrl), 10, "");
        assert_eq!(buf.cursor.col, 4);
    }

    #[test]
    fn vertical_moves_keep_the_display_column_across_tabs_and_wide_chars() {
        let mut buf = Buffer::from_text("\tabc\n日本語x\nabcdefgh");
        buf.set_cursor(Pos { line: 0, col: 3 }, false); // display col 6
        buf.edit_key(plain(KeyCode::Down), 10, "");
        assert_eq!(
            buf.cursor,
            Pos {
                line: 1,
                col: "日本語".len()
            }
        );
        buf.edit_key(plain(KeyCode::Down), 10, "");
        assert_eq!(buf.cursor, Pos { line: 2, col: 6 });
    }

    #[test]
    fn enter_keeps_indent_and_tab_indents_selection() {
        let mut buf = Buffer::from_text("    code");
        buf.set_cursor(Pos { line: 0, col: 8 }, false);
        buf.edit_key(plain(KeyCode::Enter), 10, "");
        assert_eq!(buf.text(), "    code\n    ");

        let mut buf = Buffer::from_text("a\nb");
        buf.edit_key(with(KeyCode::Char('a'), KeyModifiers::CONTROL), 10, "");
        buf.edit_key(plain(KeyCode::Tab), 10, "");
        assert_eq!(buf.text(), "    a\n    b");
        buf.edit_key(plain(KeyCode::BackTab), 10, "");
        assert_eq!(buf.text(), "a\nb");
    }

    #[test]
    fn cut_without_selection_takes_the_line() {
        let mut buf = Buffer::from_text("one\ntwo\nthree");
        buf.set_cursor(Pos { line: 1, col: 1 }, false);
        let out = buf.edit_key(with(KeyCode::Char('x'), KeyModifiers::CONTROL), 10, "");
        assert_eq!(out, Outcome::Copied("two\n".to_owned()));
        assert_eq!(buf.text(), "one\nthree");
        buf.edit_key(
            with(KeyCode::Char('v'), KeyModifiers::CONTROL),
            10,
            "pasted",
        );
        assert_eq!(buf.text(), "one\npastedthree");
    }

    #[test]
    fn find_wraps_and_uses_smart_case() {
        let mut buf = Buffer::from_text("Alpha beta\ngamma alpha");
        assert!(buf.find("alpha", true));
        assert_eq!(buf.selection().unwrap().0, Pos { line: 0, col: 0 });
        assert!(buf.find("alpha", true));
        assert_eq!(buf.selection().unwrap().0, Pos { line: 1, col: 6 });
        assert!(buf.find("alpha", true), "wraps to the top");
        assert_eq!(buf.selection().unwrap().0, Pos { line: 0, col: 0 });
        assert!(buf.find("alpha", false), "backwards wraps to the bottom");
        assert_eq!(buf.selection().unwrap().0, Pos { line: 1, col: 6 });
        buf.set_cursor(Pos { line: 1, col: 0 }, false);
        assert!(buf.find("Alpha", true), "uppercase query is case-sensitive");
        assert_eq!(buf.selection().unwrap().0, Pos { line: 0, col: 0 });
        assert!(!buf.find("zzz", true));
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kalipso-editor-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn save_round_trip(dir: &Path, original: &[u8]) -> Vec<u8> {
        let path = dir.join("file.txt");
        fs::write(&path, original).unwrap();
        let (text, crlf) = load_text(&path).unwrap();
        let mut doc = Doc::new(Some(path.clone()), &text, crlf);
        doc.save_to(&path).unwrap();
        fs::read(&path).unwrap()
    }

    #[test]
    fn saving_reproduces_line_endings_exactly() {
        let dir = temp_dir("endings");
        for original in [
            &b"a\nb\n"[..],
            b"a\nb",
            b"a\r\nb\r\n",
            b"a\r\nb",
            b"mixed\r\nendings\nhere\r\n",
            b"",
            b"\n\n",
        ] {
            assert_eq!(
                save_round_trip(&dir, original),
                original,
                "{:?}",
                String::from_utf8_lossy(original)
            );
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn save_keeps_permissions_and_follows_symlinks_and_leaves_no_temp_file() {
        use std::os::unix::fs::symlink;
        let dir = temp_dir("save");
        let real = dir.join("real.sh");
        fs::write(&real, "old").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o750)).unwrap();
        let link = dir.join("link.sh");
        symlink(&real, &link).unwrap();
        write_atomic(&link, b"new").unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&real).unwrap(), "new");
        assert_eq!(
            fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o750
        );
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn binary_and_invalid_utf8_files_are_refused() {
        let dir = temp_dir("binary");
        let binary = dir.join("a.bin");
        fs::write(&binary, b"abc\0def").unwrap();
        assert!(load_text(&binary).unwrap_err().contains("binary"));
        let latin = dir.join("b.txt");
        fs::write(&latin, b"caf\xe9").unwrap();
        assert!(load_text(&latin).unwrap_err().contains("UTF-8"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn disk_changes_reload_clean_buffers_and_never_touch_dirty_ones() {
        let dir = temp_dir("conflict");
        let path = dir.join("f.txt");
        fs::write(&path, "one").unwrap();
        let mut editor = Editor::default();
        editor.open(&path).unwrap();

        fs::write(&path, "two!").unwrap();
        assert!(editor.check_disk());
        assert_eq!(editor.docs[0].buf.text(), "two!");
        assert!(!editor.docs[0].buf.is_dirty());
        assert_eq!(editor.docs[0].conflict, None);
        editor.docs[0].buf.undo();
        assert_eq!(editor.docs[0].buf.text(), "one", "a reload can be undone");
        editor.docs[0].buf.redo();

        editor.docs[0].buf.insert("mine ");
        fs::write(&path, "three!!").unwrap();
        assert!(editor.check_disk());
        assert_eq!(editor.docs[0].conflict, Some(Conflict::Changed));
        assert!(
            editor.docs[0].buf.text().contains("mine"),
            "dirty buffer kept"
        );
        assert!(matches!(editor.save(), EditorCmd::ConfirmOverwrite));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "three!!",
            "not overwritten silently"
        );

        fs::remove_file(&path).unwrap();
        assert!(editor.check_disk());
        assert_eq!(editor.docs[0].conflict, Some(Conflict::Deleted));
        assert!(matches!(editor.save(), EditorCmd::None));
        assert!(path.exists(), "saving a deleted file recreates it");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pipes_and_devices_are_refused_instead_of_hanging() {
        let dir = temp_dir("special");
        let pipe = dir.join("pipe");
        let made = std::process::Command::new("mkfifo")
            .arg(&pipe)
            .status()
            .unwrap();
        assert!(made.success());
        let zero = dir.join("zero");
        std::os::unix::fs::symlink("/dev/zero", &zero).unwrap();
        for path in [pipe, zero, dir.clone()] {
            // On a thread, so getting this wrong fails the test instead of hanging the suite.
            let (done, result) = std::sync::mpsc::channel();
            let opened = path.clone();
            std::thread::spawn(move || done.send(load_text(&opened)));
            let answer = result.recv_timeout(std::time::Duration::from_secs(5));
            let error = answer
                .unwrap_or_else(|_| panic!("{path:?} hung"))
                .unwrap_err();
            assert_eq!(error, "not a regular file", "{path:?}");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_temp_file_for_a_private_file_is_private_from_the_start() {
        let dir = temp_dir("tempmode");
        let secret = dir.join(".env");
        fs::write(&secret, "KEY=1").unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
        let meta = fs::metadata(&secret).unwrap();
        let file = open_temp(&dir.join("tmp"), Some(&meta)).unwrap();
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        write_atomic(&secret, b"KEY=2").unwrap();
        assert_eq!(
            fs::metadata(&secret).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_jump_to_a_file_that_cannot_open_moves_nothing() {
        let dir = temp_dir("jump");
        let good = dir.join("good.txt");
        fs::write(&good, "one\ntwo\n").unwrap();
        fs::write(dir.join("binary.bin"), b"a\0b").unwrap();
        let mut editor = Editor::default();
        editor.open(&good).unwrap();
        editor.go_to(1, |_| 0);

        assert!(editor.jump_to(&dir.join("binary.bin"), 0, |_| 0).is_err());
        assert_eq!(editor.docs.len(), 1);
        assert_eq!(editor.docs[0].buf.cursor.line, 1, "the cursor stays put");
        assert!(
            matches!(editor.jump_back(), EditorCmd::Message(_)),
            "and it isn't a place to go back to"
        );

        // Past the end of the file stops at the last line rather than panicking.
        editor.jump_to(&good, 99, |_| 0).unwrap();
        assert_eq!(editor.docs[0].buf.cursor.line, 2);
        assert!(matches!(editor.jump_back(), EditorCmd::None));
        assert_eq!(editor.docs[0].buf.cursor.line, 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn closing_a_proposal_returns_to_the_tab_you_were_on() {
        let dir = temp_dir("proposal-tab");
        for name in ["a.txt", "b.txt", "c.txt"] {
            fs::write(dir.join(name), name).unwrap();
        }
        let mut editor = Editor::default();
        for name in ["a.txt", "b.txt", "c.txt"] {
            editor.open(&dir.join(name)).unwrap();
        }
        editor.open(&dir.join("a.txt")).unwrap();
        assert_eq!(editor.active, 0);

        editor.open_proposal(1, "t1".to_owned(), &dir.join("b.txt"), "b.txt", "proposed");
        assert_eq!(editor.active, 3);
        assert!(editor.active_proposal().is_some());
        editor.open(&dir.join("b.txt")).unwrap();
        assert_eq!(
            editor.active, 1,
            "opening the real file finds the real tab, not the proposal"
        );
        editor.open_proposal(1, "t1".to_owned(), &dir.join("b.txt"), "b.txt", "proposed");
        assert_eq!(editor.active, 3, "the same proposal isn't opened twice");
        assert_eq!(editor.docs.len(), 4);
        assert_eq!(
            editor.known_bases().len(),
            3,
            "proposals have no git base of their own"
        );

        editor.retain_proposals(|_, _| true);
        assert_eq!(editor.active, 3, "nothing closed, nothing moves");
        editor.retain_proposals(|_, _| false);
        assert_eq!(editor.docs.len(), 3);
        assert_eq!(editor.active, 0, "back to a.txt, not the last tab");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn highlighting_recovers_after_edits_past_a_checkpoint() {
        let mut lines: Vec<String> = (0..600).map(|i| format!("let x{i} = {i};")).collect();
        let syntax = syntax_for_token("rs");
        let mut hl = Highlight::new(syntax);
        hl.prepare(&lines, 550, 560);
        assert!(hl.cache.contains_key(&555));
        assert!(hl.checkpoints.len() >= 3, "states saved on the way down");
        // Open a block comment above the viewport: everything below must be recoloured.
        let before = hl.cache[&555].clone();
        lines[300] = "/*".to_owned();
        hl.invalidate(300);
        assert!(!hl.cache.contains_key(&555));
        hl.prepare(&lines, 550, 560);
        assert_ne!(hl.cache[&555], before);
    }

    #[test]
    fn render_line_expands_tabs_and_hides_control_characters() {
        let line = render_line("a\tb\u{1b}[31m", &[], None, 0, 40);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "a   b·[31m");
        assert!(!text.contains('\u{1b}'));
        // Clipped on both sides without splitting a wide character.
        let line = render_line("日本語", &[], None, 1, 3);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, " 本");
    }
}
