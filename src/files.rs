use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ignore::WalkBuilder;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

pub const HINTS: &str = "Enter open  Ctrl+P find  Alt+F search  ←→ fold  n file  N folder  r rename  d delete  . hidden  R refresh";

const MAX_LISTED_FILES: usize = 100_000;

pub struct Row {
    pub path: PathBuf,
    pub name: String,
    pub depth: usize,
    pub is_dir: bool,
}

pub enum FilesCmd {
    None,
    Open(PathBuf),
    NewFile(PathBuf),
    NewDir(PathBuf),
    Rename(PathBuf),
    Delete(PathBuf),
}

pub struct FileTree {
    pub root: PathBuf,
    pub rows: Vec<Row>,
    pub selected: usize,
    top: usize,
    show_hidden: bool,
    expanded: BTreeSet<PathBuf>,
    /// Modification times of listed folders, to notice outside changes.
    seen: HashMap<PathBuf, Option<SystemTime>>,
    area: Rect,
}

fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|meta| meta.modified()).ok()
}

/// One folder's children: folders first, then case-insensitive by name.
fn list_dir(dir: &Path, show_hidden: bool) -> Vec<(PathBuf, String, bool)> {
    let mut walk = WalkBuilder::new(dir);
    walk.max_depth(Some(1)).require_git(false);
    if show_hidden {
        walk.standard_filters(false);
    }
    let mut entries: Vec<_> = walk
        .build()
        .flatten()
        .filter(|entry| entry.depth() == 1)
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Follows symlinks, so a link to a folder can be opened like a folder.
            let is_dir = entry.path().is_dir();
            (entry.into_path(), name, is_dir)
        })
        .collect();
    entries.sort_by_cached_key(|(_, name, is_dir)| (!*is_dir, name.to_lowercase()));
    entries
}

/// Every file under `root` as a relative path, respecting .gitignore. Dotfiles are included,
/// since `.github/ci.yml` and the like are worth finding.
// ponytail: walks on each Ctrl+P. Cache and watch if a huge repo makes this feel slow.
pub fn all_files(root: &Path) -> Vec<String> {
    let mut walk = WalkBuilder::new(root);
    walk.hidden(false).require_git(false).filter_entry(|entry| entry.file_name() != ".git");
    let mut files: Vec<String> = walk
        .build()
        .flatten()
        .filter(|entry| entry.path().is_file())
        .filter_map(|entry| entry.path().strip_prefix(root).ok().map(|path| path.display().to_string()))
        .take(MAX_LISTED_FILES)
        .collect();
    files.sort();
    files
}

/// Joins a typed relative path onto `base`, refusing anything that could leave it.
pub fn safe_join(base: &Path, input: &str) -> Result<PathBuf, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("name is empty".to_owned());
    }
    let relative = Path::new(input);
    if !relative.components().all(|part| matches!(part, Component::Normal(_))) {
        return Err("use a plain relative name, without .. or a leading /".to_owned());
    }
    Ok(base.join(relative))
}

/// Number of entries inside a folder, including hidden and ignored ones.
pub fn count_inside(dir: &Path) -> usize {
    WalkBuilder::new(dir).standard_filters(false).build().flatten().count().saturating_sub(1)
}

/// Deletes a file or folder. A symlink is removed itself, never what it points to.
pub fn delete(path: &Path) -> std::io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() { fs::remove_dir_all(path) } else { fs::remove_file(path) }
}

impl FileTree {
    pub fn new(root: PathBuf) -> FileTree {
        let mut tree = FileTree {
            root,
            rows: Vec::new(),
            selected: 0,
            top: 0,
            show_hidden: false,
            expanded: BTreeSet::new(),
            seen: HashMap::new(),
            area: Rect::default(),
        };
        tree.refresh();
        tree
    }

    /// Rebuilds the visible rows, keeping the selection on the same path if it still exists.
    pub fn refresh(&mut self) {
        let keep = self.rows.get(self.selected).map(|row| row.path.clone());
        self.rows.clear();
        self.seen.clear();
        let root = self.root.clone();
        self.add_children(&root, 0);
        self.selected = keep
            .and_then(|path| self.rows.iter().position(|row| row.path == path))
            .unwrap_or(self.selected)
            .min(self.rows.len().saturating_sub(1));
    }

    fn add_children(&mut self, dir: &Path, depth: usize) {
        self.seen.insert(dir.to_path_buf(), modified(dir));
        for (path, name, is_dir) in list_dir(dir, self.show_hidden) {
            let open = is_dir && self.expanded.contains(&path);
            self.rows.push(Row { path: path.clone(), name, depth, is_dir });
            if open {
                self.add_children(&path, depth + 1);
            }
        }
    }

    /// Called once a second. Returns whether the tree changed.
    pub fn check_disk(&mut self) -> bool {
        let changed = self.seen.iter().any(|(dir, seen)| modified(dir) != *seen);
        if changed {
            self.refresh();
        }
        changed
    }

    /// Selects a path, expanding the folders above it.
    pub fn reveal(&mut self, path: &Path) {
        for parent in path.ancestors().skip(1) {
            if parent == self.root || !parent.starts_with(&self.root) {
                break;
            }
            self.expanded.insert(parent.to_path_buf());
        }
        self.refresh();
        if let Some(index) = self.rows.iter().position(|row| row.path == path) {
            self.selected = index;
        }
    }

    /// Folder that new files should go into: the selected one, or the selected file's parent.
    pub fn target_dir(&self) -> PathBuf {
        match self.rows.get(self.selected) {
            Some(row) if row.is_dir => row.path.clone(),
            Some(row) => row.path.parent().map_or_else(|| self.root.clone(), Path::to_path_buf),
            None => self.root.clone(),
        }
    }

    fn toggle(&mut self, index: usize) {
        let path = self.rows[index].path.clone();
        if !self.expanded.remove(&path) {
            self.expanded.insert(path);
        }
        self.refresh();
    }

    pub fn on_key(&mut self, key: KeyEvent) -> FilesCmd {
        if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
            return FilesCmd::None;
        }
        let last = self.rows.len().saturating_sub(1);
        let page = (self.area.height as usize).max(1);
        match key.code {
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected = (self.selected + 1).min(last),
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(page),
            KeyCode::PageDown => self.selected = (self.selected + page).min(last),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = last,
            KeyCode::Char('.') => {
                self.show_hidden = !self.show_hidden;
                self.refresh();
            }
            KeyCode::Char('R') => self.refresh(),
            KeyCode::Char('n') => return FilesCmd::NewFile(self.target_dir()),
            KeyCode::Char('N') => return FilesCmd::NewDir(self.target_dir()),
            _ => {}
        }
        let Some(row) = self.rows.get(self.selected) else { return FilesCmd::None };
        let (path, is_dir, depth) = (row.path.clone(), row.is_dir, row.depth);
        match key.code {
            KeyCode::Enter if !is_dir => return FilesCmd::Open(path),
            KeyCode::Enter => self.toggle(self.selected),
            KeyCode::Right if is_dir && !self.expanded.contains(&path) => self.toggle(self.selected),
            KeyCode::Right if is_dir => self.selected = (self.selected + 1).min(last),
            KeyCode::Left if is_dir && self.expanded.contains(&path) => self.toggle(self.selected),
            KeyCode::Left if depth > 0 => {
                // Jump up to the parent folder.
                if let Some(parent) = (0..self.selected).rev().find(|&i| self.rows[i].depth < depth) {
                    self.selected = parent;
                }
            }
            KeyCode::Char('r') => return FilesCmd::Rename(path),
            KeyCode::Char('d') | KeyCode::Delete => return FilesCmd::Delete(path),
            _ => {}
        }
        FilesCmd::None
    }

    pub fn on_mouse(&mut self, mouse: MouseEvent) -> FilesCmd {
        match mouse.kind {
            MouseEventKind::ScrollUp => self.top = self.top.saturating_sub(3),
            MouseEventKind::ScrollDown => self.top = (self.top + 3).min(self.rows.len().saturating_sub(1)),
            MouseEventKind::Down(MouseButton::Left) => {
                let index = self.top + mouse.row.saturating_sub(self.area.y) as usize;
                if index < self.rows.len() && mouse.row >= self.area.y {
                    self.selected = index;
                    if self.rows[index].is_dir {
                        self.toggle(index);
                    } else {
                        return FilesCmd::Open(self.rows[index].path.clone());
                    }
                }
            }
            _ => {}
        }
        FilesCmd::None
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, focused: bool) {
        self.area = area;
        let height = area.height as usize;
        if self.selected < self.top {
            self.top = self.selected;
        } else if height > 0 && self.selected >= self.top + height {
            self.top = self.selected + 1 - height;
        }
        let selected_style = if focused {
            Style::new().bg(Color::Cyan).fg(Color::Black)
        } else {
            Style::new().add_modifier(Modifier::REVERSED)
        };
        let rows: Vec<Line> = self
            .rows
            .iter()
            .enumerate()
            .skip(self.top)
            .take(height)
            .map(|(index, row)| {
                let marker = match (row.is_dir, self.expanded.contains(&row.path)) {
                    (true, true) => "▾ ",
                    (true, false) => "▸ ",
                    _ => "  ",
                };
                let base = if row.is_dir { Style::new().fg(Color::Blue) } else { Style::new() };
                let style = if index == self.selected { selected_style } else { base };
                Line::from(Span::styled(format!("{}{marker}{}", "  ".repeat(row.depth), row.name), style))
            })
            .collect();
        frame.render_widget(Paragraph::new(rows), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kalipso-files-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn names(tree: &FileTree) -> Vec<&str> {
        tree.rows.iter().map(|row| row.name.as_str()).collect()
    }

    fn press(tree: &mut FileTree, code: KeyCode) -> FilesCmd {
        tree.on_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn tree_lists_folders_first_and_respects_gitignore_until_toggled() {
        let dir = temp_dir("list");
        fs::create_dir(dir.join("src")).unwrap();
        fs::create_dir(dir.join("target")).unwrap();
        fs::write(dir.join("src/main.rs"), "").unwrap();
        fs::write(dir.join("zeta.txt"), "").unwrap();
        fs::write(dir.join(".gitignore"), "target\n").unwrap();
        fs::write(dir.join("Alpha.txt"), "").unwrap();

        let mut tree = FileTree::new(dir.clone());
        assert_eq!(names(&tree), ["src", "Alpha.txt", "zeta.txt"]);

        press(&mut tree, KeyCode::Right);
        assert_eq!(names(&tree), ["src", "main.rs", "Alpha.txt", "zeta.txt"]);
        press(&mut tree, KeyCode::Down);
        assert!(matches!(press(&mut tree, KeyCode::Enter), FilesCmd::Open(p) if p.ends_with("src/main.rs")));
        press(&mut tree, KeyCode::Left);
        assert_eq!(tree.selected, 0, "Left jumps to the parent folder");
        press(&mut tree, KeyCode::Left);
        assert_eq!(names(&tree), ["src", "Alpha.txt", "zeta.txt"]);

        press(&mut tree, KeyCode::Char('.'));
        assert_eq!(names(&tree), ["src", "target", ".gitignore", "Alpha.txt", "zeta.txt"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn check_disk_notices_new_files_in_expanded_folders_and_keeps_selection() {
        let dir = temp_dir("watch");
        fs::create_dir(dir.join("sub")).unwrap();
        fs::write(dir.join("b.txt"), "").unwrap();
        let mut tree = FileTree::new(dir.clone());
        press(&mut tree, KeyCode::Down);
        assert_eq!(tree.rows[tree.selected].name, "b.txt");
        assert!(!tree.check_disk());

        // Directory mtimes can have coarse resolution, so age the recorded time first.
        tree.seen.insert(dir.clone(), Some(SystemTime::UNIX_EPOCH));
        fs::write(dir.join("a.txt"), "").unwrap();
        assert!(tree.check_disk());
        assert_eq!(names(&tree), ["sub", "a.txt", "b.txt"]);
        assert_eq!(tree.rows[tree.selected].name, "b.txt", "selection follows the path");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn all_files_finds_nested_and_dot_files_but_not_ignored_ones() {
        let dir = temp_dir("all");
        fs::create_dir_all(dir.join("src/deep")).unwrap();
        fs::create_dir_all(dir.join(".github")).unwrap();
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::create_dir_all(dir.join("target")).unwrap();
        fs::write(dir.join(".gitignore"), "target\n").unwrap();
        fs::write(dir.join("src/deep/a.rs"), "").unwrap();
        fs::write(dir.join(".github/ci.yml"), "").unwrap();
        fs::write(dir.join(".git/config"), "").unwrap();
        fs::write(dir.join("target/out"), "").unwrap();
        assert_eq!(all_files(&dir), [".github/ci.yml", ".gitignore", "src/deep/a.rs"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn safe_join_refuses_paths_that_leave_the_base() {
        let base = Path::new("/project");
        assert_eq!(safe_join(base, "a/b.txt").unwrap(), Path::new("/project/a/b.txt"));
        for bad in ["", "  ", "/etc/passwd", "../x", "a/../../x", "a/./b/.."] {
            assert!(safe_join(base, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn deleting_a_symlink_keeps_its_target() {
        let dir = temp_dir("delete");
        let target = dir.join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("keep.txt"), "").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        delete(&link).unwrap();
        assert!(!link.exists());
        assert!(target.join("keep.txt").exists());
        assert_eq!(count_inside(&target), 1);
        delete(&target).unwrap();
        assert!(!target.exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
