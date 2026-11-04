//! Read-only git information for the file tree and the editor's gutter.
//!
//! This runs in the background on whatever folder is open, which may not be trusted, so
//! it must never run anything that a repository's own config points at.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Line pairs the diff may compare. Past this, a changed region is marked as a whole.
const MAX_DIFF_CELLS: usize = 4_000_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Change {
    Modified,
    Added,
}

pub struct Snapshot {
    pub head: Option<String>,
    pub changes: HashMap<PathBuf, Change>,
    /// Requested files as they are at HEAD. `None` if a file isn't in HEAD or isn't text.
    pub bases: Vec<(PathBuf, Option<String>)>,
}

fn git(dir: &Path) -> Command {
    let mut command = Command::new("git");
    // core.fsmonitor in a repository's config names a program that `git status` would run.
    command
        .current_dir(dir)
        .args(["-c", "core.fsmonitor=false"])
        .env("GIT_OPTIONAL_LOCKS", "0") // Don't take locks that could block the user's own git.
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn output(command: &mut Command) -> Option<Vec<u8>> {
    let result = command.output().ok()?;
    result.status.success().then_some(result.stdout)
}

fn text_line(bytes: Vec<u8>) -> Option<String> {
    let text = String::from_utf8(bytes).ok()?;
    Some(text.trim_end().to_owned())
}

/// The state of the repository containing `root`, or `None` if there isn't one. `known` is
/// each open file with the HEAD its base text came from. Stale or missing ones get reloaded.
pub fn snapshot(root: &Path, known: &[(PathBuf, Option<String>)]) -> Option<Snapshot> {
    let top = text_line(output(git(root).args(["rev-parse", "--show-toplevel"]))?)?;
    let top = Path::new(&top).canonicalize().ok()?;
    let head = output(git(root).args(["rev-parse", "--verify", "-q", "HEAD"])).and_then(text_line);
    let status = output(git(root).args(["status", "--porcelain=v1", "-z", "--untracked-files=all", "--ignore-submodules=all"]))?;
    let changes = parse_status(&status, &top);
    let mut bases = Vec::new();
    if let Some(head) = &head {
        for (path, loaded_at) in known {
            if loaded_at.as_ref() != Some(head)
                && let Ok(relative) = path.strip_prefix(root)
            {
                // cat-file, because it never runs a repository's text conversion filters.
                let spec = format!("HEAD:./{}", relative.display());
                let text = output(git(root).args(["cat-file", "blob", &spec])).and_then(|bytes| String::from_utf8(bytes).ok());
                bases.push((path.clone(), text));
            }
        }
    }
    Some(Snapshot { head, changes, bases })
}

fn parse_status(output: &[u8], top: &Path) -> HashMap<PathBuf, Change> {
    let mut changes = HashMap::new();
    let mut fields = output.split(|byte| *byte == 0);
    while let Some(field) = fields.next() {
        if field.len() < 4 {
            continue;
        }
        let index_state = field[0];
        if matches!(index_state, b'R' | b'C') {
            fields.next(); // The original path follows a rename or copy.
        }
        let change = if matches!(index_state, b'?' | b'A') { Change::Added } else { Change::Modified };
        changes.insert(top.join(OsStr::from_bytes(&field[3..])), change);
    }
    changes
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Mark {
    #[default]
    None,
    Added,
    Changed,
    /// Lines were removed just before this one.
    Deleted,
}

/// What each line of `new` looks like compared with `old`.
pub fn line_marks(old: &[String], new: &[String]) -> Vec<Mark> {
    let mut marks = vec![Mark::None; new.len()];
    let start = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let tail = old[start..].iter().rev().zip(new[start..].iter().rev()).take_while(|(a, b)| a == b).count();
    let a = &old[start..old.len() - tail];
    let b = &new[start..new.len() - tail];
    for (a_from, a_count, b_from, b_count) in hunks(a, b) {
        for offset in 0..b_count {
            marks[start + b_from + offset] = if offset < a_count { Mark::Changed } else { Mark::Added };
        }
        if b_count == 0 && a_count > 0 && a_from <= a.len() && !new.is_empty() {
            let line = (start + b_from).min(new.len() - 1);
            if marks[line] == Mark::None {
                marks[line] = Mark::Deleted;
            }
        }
    }
    marks
}

/// Regions that differ, as (a start, a count, b start, b count), using a longest common subsequence.
fn hunks(a: &[String], b: &[String]) -> Vec<(usize, usize, usize, usize)> {
    if a.is_empty() && b.is_empty() {
        return Vec::new();
    }
    let cells = (a.len() + 1).saturating_mul(b.len() + 1);
    if cells > MAX_DIFF_CELLS {
        return vec![(0, a.len(), 0, b.len())];
    }
    let width = b.len() + 1;
    let mut table = vec![0u32; cells];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            table[i * width + j] = if a[i] == b[j] {
                table[(i + 1) * width + j + 1] + 1
            } else {
                table[(i + 1) * width + j].max(table[i * width + j + 1])
            };
        }
    }
    let mut result = Vec::new();
    let (mut i, mut j) = (0, 0);
    let mut open: Option<(usize, usize)> = None;
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            if let Some((from_a, from_b)) = open.take() {
                result.push((from_a, i - from_a, from_b, j - from_b));
            }
            i += 1;
            j += 1;
            continue;
        }
        open.get_or_insert((i, j));
        if j == b.len() || (i < a.len() && table[(i + 1) * width + j] >= table[i * width + j + 1]) {
            i += 1;
        } else {
            j += 1;
        }
    }
    if let Some((from_a, from_b)) = open {
        result.push((from_a, i - from_a, from_b, j - from_b));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn lines(text: &str) -> Vec<String> {
        text.split('\n').map(str::to_owned).collect()
    }

    fn marks(old: &str, new: &str) -> Vec<Mark> {
        line_marks(&lines(old), &lines(new))
    }

    #[test]
    fn marks_tell_added_changed_and_deleted_lines_apart() {
        use Mark::{Added, Changed, Deleted, None};
        assert_eq!(marks("a\nb\nc", "a\nb\nc"), [None, None, None]);
        assert_eq!(marks("a\nb\nc", "a\nX\nc"), [None, Changed, None]);
        assert_eq!(marks("a\nc", "a\nb\nc"), [None, Added, None]);
        assert_eq!(marks("a\nb\nc", "a\nc"), [None, Deleted]);
        assert_eq!(marks("a\nb", "a"), [Deleted], "deleting the end marks the last line");
        assert_eq!(marks("a\nb\nc", "a\nX\nY\nc"), [None, Changed, Added, None], "a longer replacement");
        assert_eq!(marks("", "a"), [Changed]);
    }

    #[test]
    fn separate_edits_stay_separate() {
        use Mark::{Added, Changed, None};
        let old = "1\n2\n3\n4\n5\n6";
        let new = "1\nX\n3\n4\nnew\n5\n6";
        assert_eq!(marks(old, new), [None, Changed, None, None, Added, None, None]);
    }

    #[test]
    fn a_huge_changed_region_is_marked_whole_instead_of_stalling() {
        let old: Vec<String> = (0..3000).map(|i| format!("old {i}")).collect();
        let new: Vec<String> = (0..3000).map(|i| format!("new {i}")).collect();
        let started = std::time::Instant::now();
        let result = line_marks(&old, &new);
        assert!(result.iter().all(|mark| *mark == Mark::Changed));
        assert!(started.elapsed().as_secs() < 2);
    }

    fn run(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kalipso-git-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        run(&dir, &["init", "-q"]);
        dir
    }

    #[test]
    fn snapshot_reports_changes_and_base_text() {
        let dir = repo("snapshot");
        fs::create_dir(dir.join("sub")).unwrap();
        fs::write(dir.join("tracked.txt"), "one\n").unwrap();
        fs::write(dir.join("sub/old name.txt"), "x\n").unwrap();
        fs::write(dir.join("gone.txt"), "y\n").unwrap();
        fs::write(dir.join("clean.txt"), "z\n").unwrap();
        run(&dir, &["add", "."]);
        run(&dir, &["commit", "-qm", "first"]);

        fs::write(dir.join("tracked.txt"), "two\n").unwrap();
        fs::write(dir.join("new file.txt"), "n\n").unwrap();
        fs::remove_file(dir.join("gone.txt")).unwrap();
        run(&dir, &["mv", "sub/old name.txt", "sub/renamed.txt"]);

        let known = [(dir.join("tracked.txt"), None), (dir.join("new file.txt"), None)];
        let snapshot = snapshot(&dir, &known).unwrap();
        let changes = &snapshot.changes;
        assert_eq!(changes[&dir.join("tracked.txt")], Change::Modified);
        assert_eq!(changes[&dir.join("new file.txt")], Change::Added);
        assert_eq!(changes[&dir.join("gone.txt")], Change::Modified);
        assert_eq!(changes[&dir.join("sub/renamed.txt")], Change::Modified);
        assert!(!changes.contains_key(&dir.join("sub/old name.txt")), "the old name of a rename isn't an entry");
        assert!(!changes.contains_key(&dir.join("clean.txt")));
        assert!(snapshot.head.is_some());
        assert_eq!(snapshot.bases[0], (dir.join("tracked.txt"), Some("one\n".to_owned())));
        assert_eq!(snapshot.bases[1], (dir.join("new file.txt"), None));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn bases_load_only_when_head_moved() {
        let dir = repo("head");
        fs::write(dir.join("a.txt"), "one\n").unwrap();
        run(&dir, &["add", "."]);
        run(&dir, &["commit", "-qm", "first"]);
        let first = snapshot(&dir, &[]).unwrap().head;
        assert!(snapshot(&dir, &[(dir.join("a.txt"), first.clone())]).unwrap().bases.is_empty());

        fs::write(dir.join("a.txt"), "two\n").unwrap();
        run(&dir, &["commit", "-qam", "second"]);
        let moved = snapshot(&dir, &[(dir.join("a.txt"), first.clone())]).unwrap();
        assert_ne!(moved.head, first);
        assert_eq!(moved.bases[0].1.as_deref(), Some("two\n"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_subfolder_project_gets_correct_paths_and_a_repo_without_commits_works() {
        let dir = repo("subfolder");
        fs::create_dir(dir.join("pkg")).unwrap();
        fs::write(dir.join("pkg/a.txt"), "one\n").unwrap();
        let empty = snapshot(&dir, &[(dir.join("pkg/a.txt"), None)]).unwrap();
        assert_eq!(empty.head, None);
        assert!(empty.bases.is_empty());
        assert_eq!(empty.changes[&dir.join("pkg/a.txt")], Change::Added);

        run(&dir, &["add", "."]);
        run(&dir, &["commit", "-qm", "first"]);
        fs::write(dir.join("pkg/a.txt"), "two\n").unwrap();
        let root = dir.join("pkg");
        let inner = snapshot(&root, &[(root.join("a.txt"), None)]).unwrap();
        assert_eq!(inner.changes[&root.join("a.txt")], Change::Modified);
        assert_eq!(inner.bases[0].1.as_deref(), Some("one\n"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_repository_means_no_snapshot() {
        let dir = std::env::temp_dir().join(format!("kalipso-git-none-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // Keeps git from finding a repository above the temp folder.
        let mut command = git(&dir);
        command.env("GIT_CEILING_DIRECTORIES", dir.parent().unwrap());
        if output(command.args(["rev-parse", "--show-toplevel"])).is_none() {
            assert!(snapshot(&dir, &[]).is_none());
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A folder from an untrusted source can carry its own .git/config.
    #[test]
    fn a_repositorys_config_cannot_make_the_background_check_run_programs() {
        use std::os::unix::fs::PermissionsExt;
        let dir = repo("fsmonitor");
        let marker = dir.join("ran");
        let script = dir.join("evil.sh");
        fs::write(&script, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(dir.join("a.txt"), "one\n").unwrap();
        run(&dir, &["add", "a.txt"]);
        run(&dir, &["commit", "-qm", "first"]);
        run(&dir, &["config", "core.fsmonitor", script.to_str().unwrap()]);
        fs::write(dir.join(".gitattributes"), "*.txt diff=evil\n").unwrap();
        run(&dir, &["config", "diff.evil.textconv", script.to_str().unwrap()]);

        let result = snapshot(&dir, &[(dir.join("a.txt"), None)]).unwrap();
        assert_eq!(result.bases[0].1.as_deref(), Some("one\n"));
        assert!(!marker.exists(), "a program named in the repository's config ran");
        fs::remove_dir_all(&dir).unwrap();
    }
}
