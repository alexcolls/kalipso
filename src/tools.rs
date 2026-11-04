//! What agents can do to the project, and which of those things need approval.

use std::collections::{HashSet, VecDeque};
use std::fs;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ignore::WalkBuilder;
use ignore::gitignore::Gitignore;
use serde_json::{Value, json};

use crate::editor::{load_text, write_atomic};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
/// Output is kept from both ends, because errors tend to be at the end.
const CAPTURE_END_BYTES: usize = 16 * 1024;
const READ_MAX_LINES: usize = 2000;
const READ_MAX_LINE_CHARS: usize = 2000;
const READ_MAX_BYTES: usize = 256 * 1024;
const LIST_MAX: usize = 2000;
const SEARCH_MAX_MATCHES: usize = 200;
const SEARCH_MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const SEARCH_MAX_LINE_CHARS: usize = 300;

/// Removed from commands agents run, so a prompt-injected command can't simply read them.
const SECRET_VARS: [&str; 5] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "OPENAI_API_KEY",
    "XAI_API_KEY",
    "OPENROUTER_API_KEY",
];

pub const READ_TOOLS: [&str; 3] = ["read_file", "list_dir", "search"];

pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: Value,
}

/// The definitions for the named tools, in the order given.
pub fn definitions(names: &[&str]) -> Vec<ToolDef> {
    names.iter().filter_map(|name| definition(name)).collect()
}

fn definition(name: &str) -> Option<ToolDef> {
    let (name, description, schema) = match name {
        "read_file" => (
            "read_file",
            "Read a text file in the project. Returns numbered lines. Use offset and limit for large files.",
            json!({"type": "object", "properties": {
                "path": {"type": "string", "description": "Path relative to the project root"},
                "offset": {"type": "integer", "description": "First line to return, counting from 1"},
                "limit": {"type": "integer", "description": "Maximum number of lines to return"}
            }, "required": ["path"]}),
        ),
        "list_dir" => (
            "list_dir",
            "List a folder in the project. Folders end with /. Gitignored entries are left out.",
            json!({"type": "object", "properties": {
                "path": {"type": "string", "description": "Folder relative to the project root. Defaults to the root."}
            }}),
        ),
        "search" => (
            "search",
            "Search the project's text files for a regular expression. Returns path:line: text. Gitignored and secret files are skipped.",
            json!({"type": "object", "properties": {
                "pattern": {"type": "string", "description": "Rust regex syntax"},
                "path": {"type": "string", "description": "Folder or file to search. Defaults to the whole project."}
            }, "required": ["pattern"]}),
        ),
        "write_file" => (
            "write_file",
            "Create a file or replace its whole contents. Creates missing folders. Needs the user's approval.",
            json!({"type": "object", "properties": {
                "path": {"type": "string", "description": "Path relative to the project root"},
                "contents": {"type": "string", "description": "The complete new contents of the file"}
            }, "required": ["path", "contents"]}),
        ),
        "edit_file" => (
            "edit_file",
            "Replace exact text in a file. old_string must match exactly once unless replace_all is true. Needs the user's approval.",
            json!({"type": "object", "properties": {
                "path": {"type": "string", "description": "Path relative to the project root"},
                "old_string": {"type": "string", "description": "The exact text to replace"},
                "new_string": {"type": "string", "description": "The text to put in its place"},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence"}
            }, "required": ["path", "old_string", "new_string"]}),
        ),
        "run_command" => (
            "run_command",
            "Run a shell command in the project root and return its output. Times out after 120 seconds. No input is available. Needs the user's approval.",
            json!({"type": "object", "properties": {
                "command": {"type": "string", "description": "Passed to sh -c"}
            }, "required": ["command"]}),
        ),
        _ => return None,
    };
    Some(ToolDef { name, description, schema })
}

/// Checks a model's tool input against the schema. Streamed input isn't validated by
/// every provider, and a truncated call can still parse as valid JSON.
pub fn validate(name: &str, input: &Value) -> Result<(), String> {
    let Some(def) = definition(name) else { return Err(format!("Unknown tool: {name}")) };
    let Some(object) = input.as_object() else { return Err("Input must be a JSON object".to_owned()) };
    for field in def.schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        if !object.contains_key(field) {
            return Err(format!("Missing required field: {field}"));
        }
    }
    for (field, value) in object {
        let wanted = def.schema["properties"][field]["type"].as_str();
        let ok = match wanted {
            Some("string") => value.is_string(),
            Some("integer") => value.is_u64() || value.is_i64(),
            Some("boolean") => value.is_boolean(),
            _ => true,
        };
        if !ok {
            return Err(format!("Field {field} must be a {}", wanted.unwrap_or("value")));
        }
    }
    Ok(())
}

fn text_field<'a>(input: &'a Value, field: &str) -> &'a str {
    input[field].as_str().unwrap_or("")
}

/// Resolves a model-supplied path to somewhere inside `root`, following symlinks.
/// The target doesn't have to exist yet, so new files can be written.
pub fn resolve(root: &Path, input: &str) -> Result<PathBuf, String> {
    let outside = || format!("{input} is outside the project");
    let path = Path::new(if input.is_empty() { "." } else { input });
    let path = if path.is_absolute() { path.strip_prefix(root).map_err(|_| outside())? } else { path };
    if path.components().any(|part| matches!(part, Component::ParentDir | Component::Prefix(_) | Component::RootDir)) {
        return Err(outside());
    }
    // The deepest part that exists decides where this really points.
    let full = root.join(path);
    let mut existing = full.as_path();
    let mut missing = Vec::new();
    loop {
        match fs::canonicalize(existing) {
            Ok(real) => {
                if !real.starts_with(root) {
                    return Err(outside());
                }
                let mut out = real;
                out.extend(missing.iter().rev());
                return Ok(out);
            }
            Err(_) if fs::symlink_metadata(existing).is_ok() => {
                // A dangling symlink: writing through it could land anywhere.
                return Err(outside());
            }
            Err(_) => {
                missing.push(existing.file_name().ok_or_else(outside)?);
                existing = existing.parent().ok_or_else(outside)?;
            }
        }
    }
}

fn is_secret_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    if matches!(lower.as_str(), ".env.example" | ".env.sample" | ".env.template") {
        return false;
    }
    lower.starts_with(".env")
        || lower.starts_with("id_rsa")
        || lower.starts_with("id_ed25519")
        || lower.starts_with("id_ecdsa")
        || [".pem", ".key", ".p12", ".pfx"].iter().any(|ext| lower.ends_with(ext))
        || matches!(lower.as_str(), ".netrc" | ".npmrc" | ".pypirc" | "credentials")
}

fn is_gitignored(root: &Path, path: &Path) -> bool {
    let is_dir = path.is_dir();
    for dir in path.ancestors().skip(1) {
        if !dir.starts_with(root) {
            break;
        }
        let (rules, _) = Gitignore::new(dir.join(".gitignore"));
        let matched = rules.matched_path_or_any_parents(path, is_dir);
        if matched.is_ignore() {
            return true;
        }
        if matched.is_whitelist() {
            return false;
        }
    }
    false
}

/// Files that may hold credentials or that git is told to keep out. Touching these always asks.
pub fn is_sensitive(root: &Path, path: &Path) -> bool {
    let inside = path.strip_prefix(root).unwrap_or(path);
    let secret = inside.components().any(|part| match part {
        Component::Normal(name) => {
            let name = name.to_string_lossy();
            name == ".git" || is_secret_name(&name)
        }
        _ => false,
    });
    secret || is_gitignored(root, path)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Gate {
    Run,
    /// Show this to the user and wait for an answer.
    Ask(String),
    Deny(String),
}

/// Decides whether a tool call may run. `tools` is what the agent is allowed to use and
/// `allowed` is what the user said "always" to in this chat tab.
pub fn gate(tools: &[&str], name: &str, input: &Value, root: &Path, allowed: &HashSet<String>) -> Gate {
    if !tools.contains(&name) {
        return Gate::Deny(format!("This agent can't use {name}."));
    }
    let path_of = |field: &str| resolve(root, text_field(input, field));
    match name {
        "read_file" => match path_of("path") {
            Err(err) => Gate::Deny(err),
            Ok(path) if is_sensitive(root, &path) => {
                Gate::Ask(format!("Read a file that may be private: {}", text_field(input, "path")))
            }
            Ok(_) => Gate::Run,
        },
        "list_dir" | "search" => match path_of("path") {
            Err(err) => Gate::Deny(err),
            Ok(_) => Gate::Run,
        },
        "write_file" | "edit_file" => match path_of("path") {
            Err(err) => Gate::Deny(err),
            Ok(path) => {
                if allowed.contains(name) && !is_sensitive(root, &path) {
                    Gate::Run
                } else {
                    match change_preview(name, input, &path) {
                        Ok(preview) => Gate::Ask(preview),
                        Err(err) => Gate::Deny(err),
                    }
                }
            }
        },
        "run_command" => {
            if allowed.contains(name) {
                Gate::Run
            } else {
                Gate::Ask(format!("$ {}", text_field(input, "command")))
            }
        }
        _ => Gate::Deny(format!("Unknown tool: {name}")),
    }
}

/// A line diff of the changed region: common lines at both ends are trimmed.
// ponytail: one changed region, no LCS. Scattered edits show more context than needed, which is safe.
pub fn diff_preview(old: &str, new: &str) -> String {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let start = old_lines.iter().zip(&new_lines).take_while(|(a, b)| a == b).count();
    let tail = old_lines[start..].iter().rev().zip(new_lines[start..].iter().rev()).take_while(|(a, b)| a == b).count();
    let mut out = String::new();
    for line in &old_lines[start..old_lines.len() - tail] {
        out.push_str(&format!("-{line}\n"));
    }
    for line in &new_lines[start..new_lines.len() - tail] {
        out.push_str(&format!("+{line}\n"));
    }
    if out.is_empty() {
        out.push_str(" (no change)\n");
    }
    out
}

/// The result of an edit, or why it can't be applied.
fn apply_edit(text: &str, input: &Value) -> Result<String, String> {
    let old = text_field(input, "old_string");
    let new = text_field(input, "new_string");
    if old.is_empty() {
        return Err("old_string is empty. Use write_file to create a file.".to_owned());
    }
    let count = text.matches(old).count();
    let replace_all = input["replace_all"].as_bool().unwrap_or(false);
    match (count, replace_all) {
        (0, _) => Err("old_string was not found in the file. Read the file again and copy the text exactly.".to_owned()),
        (1, _) | (_, true) => Ok(text.replace(old, new)),
        (n, false) => Err(format!("old_string matches {n} places. Include more surrounding text, or set replace_all.")),
    }
}

/// A file change's current and proposed contents.
fn contents_of_change(name: &str, input: &Value, path: &Path) -> Result<(String, String), String> {
    if name == "write_file" {
        let old = if path.exists() { load_text(path).map(|(text, _)| text).unwrap_or_default() } else { String::new() };
        return Ok((old, text_field(input, "contents").to_owned()));
    }
    let (old, _) = load_text(path).map_err(|err| format!("{}: {err}", text_field(input, "path")))?;
    let new = apply_edit(&old, input)?;
    Ok((old, new))
}

/// Where a file change would land, with the file's current and proposed contents.
pub fn proposal(name: &str, input: &Value, root: &Path) -> Result<(PathBuf, String, String), String> {
    let path = resolve(root, text_field(input, "path"))?;
    let (old, new) = contents_of_change(name, input, &path)?;
    Ok((path, old, new))
}

fn change_preview(name: &str, input: &Value, path: &Path) -> Result<String, String> {
    let shown = text_field(input, "path");
    let (old, new) = contents_of_change(name, input, path)?;
    let verb = if path.exists() { "Change" } else { "Create" };
    Ok(format!("{verb} {shown}\n{}", diff_preview(&old, &new)))
}

pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
}

fn ok(text: String) -> ToolOutput {
    ToolOutput { text, is_error: false }
}

fn error(text: impl Into<String>) -> ToolOutput {
    ToolOutput { text: text.into(), is_error: true }
}

/// Runs a tool that has already passed [`validate`] and [`gate`].
pub fn run(name: &str, input: &Value, root: &Path, cancel: &AtomicBool) -> ToolOutput {
    let result = match name {
        "read_file" => read_file(input, root),
        "list_dir" => list_dir(input, root),
        "search" => search(input, root),
        "write_file" => write_file(input, root),
        "edit_file" => edit_file(input, root),
        "run_command" => return run_command(text_field(input, "command"), root, cancel),
        _ => Err(format!("Unknown tool: {name}")),
    };
    result.map_or_else(error, ok)
}

fn read_file(input: &Value, root: &Path) -> Result<String, String> {
    let shown = text_field(input, "path");
    let path = resolve(root, shown)?;
    let (text, _) = load_text(&path).map_err(|err| format!("{shown}: {err}"))?;
    let total = text.lines().count();
    let offset = input["offset"].as_u64().unwrap_or(1).max(1) as usize;
    let limit = input["limit"].as_u64().map_or(READ_MAX_LINES, |n| (n as usize).min(READ_MAX_LINES));
    let mut out = String::new();
    let mut shown_lines = 0;
    for (index, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
        if out.len() > READ_MAX_BYTES {
            break;
        }
        let clipped: String = line.chars().take(READ_MAX_LINE_CHARS).collect();
        out.push_str(&format!("{}\t{clipped}\n", index + 1));
        shown_lines += 1;
    }
    let last = offset - 1 + shown_lines;
    if last < total {
        out.push_str(&format!("[Showing lines {offset}-{last} of {total}. Use offset to read more.]\n"));
    }
    if out.is_empty() {
        out.push_str("(empty file)\n");
    }
    Ok(out)
}

fn walker(path: &Path) -> WalkBuilder {
    let mut walk = WalkBuilder::new(path);
    walk.hidden(false).require_git(false);
    walk.filter_entry(|entry| entry.file_name() != ".git");
    walk
}

fn list_dir(input: &Value, root: &Path) -> Result<String, String> {
    let path = resolve(root, text_field(input, "path"))?;
    if !path.is_dir() {
        return Err(format!("{} is not a folder", text_field(input, "path")));
    }
    let mut names: Vec<String> = walker(&path)
        .max_depth(Some(1))
        .build()
        .flatten()
        .filter(|entry| entry.depth() == 1)
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.path().is_dir() { format!("{name}/") } else { name }
        })
        .collect();
    names.sort();
    let total = names.len();
    names.truncate(LIST_MAX);
    let mut out = names.join("\n");
    if total > LIST_MAX {
        out.push_str(&format!("\n[Showing {LIST_MAX} of {total} entries.]"));
    }
    Ok(if out.is_empty() { "(empty folder)".to_owned() } else { out })
}

pub struct SearchHit {
    pub path: String,
    pub line: usize,
    pub text: String,
}

/// Matches in text files under `base`, up to `limit`. Gitignored and private files are
/// skipped. The bool says whether there were more.
pub fn search_files(root: &Path, base: &Path, pattern: &regex::Regex, limit: usize) -> (Vec<SearchHit>, bool) {
    let mut hits = Vec::new();
    for entry in walker(base).build().flatten() {
        let path = entry.path();
        if !entry.file_type().is_some_and(|kind| kind.is_file()) || is_sensitive(root, path) {
            continue;
        }
        if entry.metadata().map_or(true, |meta| meta.len() > SEARCH_MAX_FILE_BYTES) {
            continue;
        }
        let Ok(text) = fs::read_to_string(path) else { continue };
        let shown = path.strip_prefix(root).unwrap_or(path).display().to_string();
        for (index, line) in text.lines().enumerate() {
            if pattern.is_match(line) {
                if hits.len() == limit {
                    return (hits, true);
                }
                let text = line.chars().take(SEARCH_MAX_LINE_CHARS).collect();
                hits.push(SearchHit { path: shown.clone(), line: index + 1, text });
            }
        }
    }
    (hits, false)
}

fn search(input: &Value, root: &Path) -> Result<String, String> {
    let pattern = regex::Regex::new(text_field(input, "pattern")).map_err(|err| format!("Invalid pattern: {err}"))?;
    let base = resolve(root, text_field(input, "path"))?;
    let (hits, more) = search_files(root, &base, &pattern, SEARCH_MAX_MATCHES);
    if hits.is_empty() {
        return Ok("No matches.".to_owned());
    }
    let mut out: String = hits.iter().map(|hit| format!("{}:{}: {}\n", hit.path, hit.line, hit.text)).collect();
    if more {
        out.push_str(&format!("[Stopped at {SEARCH_MAX_MATCHES} matches. Narrow the search.]\n"));
    }
    Ok(out)
}

fn write_file(input: &Value, root: &Path) -> Result<String, String> {
    let shown = text_field(input, "path");
    let path = resolve(root, shown)?;
    if path.is_dir() {
        return Err(format!("{shown} is a folder"));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let contents = text_field(input, "contents");
    write_atomic(&path, contents.as_bytes()).map_err(|err| format!("{shown}: {err}"))?;
    Ok(format!("Wrote {shown} ({} bytes).", contents.len()))
}

fn edit_file(input: &Value, root: &Path) -> Result<String, String> {
    let shown = text_field(input, "path");
    let path = resolve(root, shown)?;
    let (text, crlf) = load_text(&path).map_err(|err| format!("{shown}: {err}"))?;
    let new = apply_edit(&text, input)?;
    let bytes = if crlf { new.replace('\n', "\r\n") } else { new };
    write_atomic(&path, bytes.as_bytes()).map_err(|err| format!("{shown}: {err}"))?;
    Ok(format!("Edited {shown}."))
}

#[derive(Default)]
struct Capture {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    dropped: usize,
}

impl Capture {
    fn push(&mut self, bytes: &[u8]) {
        let room = CAPTURE_END_BYTES.saturating_sub(self.head.len()).min(bytes.len());
        self.head.extend_from_slice(&bytes[..room]);
        self.tail.extend(&bytes[room..]);
        while self.tail.len() > CAPTURE_END_BYTES {
            self.tail.pop_front();
            self.dropped += 1;
        }
    }

    fn text(&self) -> String {
        let mut out = String::from_utf8_lossy(&self.head).into_owned();
        if self.dropped > 0 {
            out.push_str(&format!("\n[... {} bytes of output omitted ...]\n", self.dropped));
        }
        let (a, b) = self.tail.as_slices();
        out.push_str(&String::from_utf8_lossy(&[a, b].concat()));
        out
    }
}

fn kill_group(pid: u32) {
    // SAFETY: kill has no memory effects. The child leads its own group (process_group(0)),
    // so the negative pid only reaches that command and anything it started.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

fn run_command(command: &str, root: &Path, cancel: &AtomicBool) -> ToolOutput {
    let mut builder = Command::new("sh");
    builder
        .arg("-c")
        .arg(command)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    for var in SECRET_VARS {
        builder.env_remove(var);
    }
    let mut child = match builder.spawn() {
        Ok(child) => child,
        Err(err) => return error(format!("Can't run the command: {err}")),
    };
    let capture = Arc::new(Mutex::new(Capture::default()));
    let mut readers = Vec::new();
    let pipes: [Option<Box<dyn Read + Send>>; 2] = [
        child.stdout.take().map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
        child.stderr.take().map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
    ];
    for pipe in pipes.into_iter().flatten() {
        let capture = capture.clone();
        let mut pipe = pipe;
        readers.push(thread::spawn(move || {
            let mut chunk = [0; 8192];
            while let Ok(n) = pipe.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                capture.lock().unwrap().push(&chunk[..n]);
            }
        }));
    }

    let started = Instant::now();
    let mut stopped_because = None;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(err) => return error(format!("Lost track of the command: {err}")),
        }
        if stopped_because.is_none() {
            if cancel.load(Ordering::Relaxed) {
                stopped_because = Some("Stopped by the user.".to_owned());
            } else if started.elapsed() > COMMAND_TIMEOUT {
                stopped_because = Some(format!("Timed out after {} seconds.", COMMAND_TIMEOUT.as_secs()));
            }
            if stopped_because.is_some() {
                kill_group(child.id());
            }
        }
        thread::sleep(Duration::from_millis(20));
    };

    // A background process can hold the pipes open forever, so don't wait long for them.
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline && !readers.iter().all(|reader| reader.is_finished()) {
        thread::sleep(Duration::from_millis(10));
    }
    let mut text = capture.lock().unwrap().text();
    if text.is_empty() {
        text.push_str("(no output)");
    }
    if let Some(reason) = stopped_because {
        text.push_str(&format!("\n{reason}"));
        return error(text);
    }
    match status.and_then(|status| status.code()) {
        Some(0) => ok(text),
        Some(code) => error(format!("{text}\nExit code {code}.")),
        None => error(format!("{text}\nKilled by a signal.")),
    }
}

/// A short description of a tool call for the chat transcript.
pub fn label(name: &str, input: &Value) -> String {
    let detail = match name {
        "run_command" => text_field(input, "command"),
        "search" => text_field(input, "pattern"),
        _ => text_field(input, "path"),
    };
    let first_line = detail.lines().next().unwrap_or("");
    format!("{name} {first_line}").trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn project(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kalipso-tools-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    const ALL: [&str; 6] = ["read_file", "list_dir", "search", "write_file", "edit_file", "run_command"];

    fn no_allowed() -> HashSet<String> {
        HashSet::new()
    }

    fn go(name: &str, input: Value, root: &Path) -> ToolOutput {
        validate(name, &input).unwrap();
        run(name, &input, root, &AtomicBool::new(false))
    }

    #[test]
    fn paths_cannot_leave_the_project() {
        let root = project("confine");
        let outside = project("confine-outside");
        fs::write(outside.join("secret.txt"), "x").unwrap();
        fs::create_dir(root.join("sub")).unwrap();
        symlink(&outside, root.join("escape")).unwrap();
        symlink(outside.join("nothing-yet"), root.join("dangling")).unwrap();

        assert_eq!(resolve(&root, "a/b/new.txt").unwrap(), root.join("a/b/new.txt"));
        assert_eq!(resolve(&root, "").unwrap(), root);
        assert_eq!(resolve(&root, "sub").unwrap(), root.join("sub"));
        assert_eq!(resolve(&root, root.join("sub").to_str().unwrap()).unwrap(), root.join("sub"));

        for bad in ["../x", "sub/../../x", "/etc/passwd", "escape/secret.txt", "escape/new.txt", "dangling"] {
            assert!(resolve(&root, bad).is_err(), "{bad} should be refused");
        }

        // The refusal reaches the tool level too, for both reading and writing.
        let read = gate(&ALL, "read_file", &json!({"path": "escape/secret.txt"}), &root, &no_allowed());
        assert!(matches!(read, Gate::Deny(_)));
        let mut allowed = no_allowed();
        allowed.insert("write_file".to_owned());
        let write = gate(&ALL, "write_file", &json!({"path": "escape/x", "contents": ""}), &root, &allowed);
        assert!(matches!(write, Gate::Deny(_)), "an 'always' answer must not widen the sandbox");
        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&outside).unwrap();
    }

    #[test]
    fn agents_only_get_their_own_tools() {
        let root = project("agents");
        let review = ["read_file", "list_dir", "search", "run_command"];
        let design = ["read_file", "list_dir", "search", "write_file"];
        let write = json!({"path": "a.txt", "contents": "x"});
        let edit = json!({"path": "a.txt", "old_string": "a", "new_string": "b"});
        let command = json!({"command": "true"});
        assert!(matches!(gate(&review, "write_file", &write, &root, &no_allowed()), Gate::Deny(_)));
        assert!(matches!(gate(&review, "edit_file", &edit, &root, &no_allowed()), Gate::Deny(_)));
        assert!(matches!(gate(&review, "run_command", &command, &root, &no_allowed()), Gate::Ask(_)));
        assert!(matches!(gate(&design, "run_command", &command, &root, &no_allowed()), Gate::Deny(_)));
        assert!(matches!(gate(&design, "write_file", &write, &root, &no_allowed()), Gate::Ask(_)));
        // Even if the user said "always" somewhere, a missing tool stays missing.
        let mut allowed = no_allowed();
        allowed.insert("run_command".to_owned());
        assert!(matches!(gate(&design, "run_command", &command, &root, &allowed), Gate::Deny(_)));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn writes_and_commands_ask_until_always_and_sensitive_files_always_ask() {
        let root = project("approve");
        fs::write(root.join(".gitignore"), "ignored.txt\nbuild/\n").unwrap();
        let mut allowed = no_allowed();
        let write = json!({"path": "a.txt", "contents": "x"});
        let read = |path: &str| json!({"path": path});

        assert!(matches!(gate(&ALL, "write_file", &write, &root, &allowed), Gate::Ask(_)));
        allowed.insert("write_file".to_owned());
        assert_eq!(gate(&ALL, "write_file", &write, &root, &allowed), Gate::Run);

        for secret in [".env", ".env.local", "config/.env", "server.pem", "id_rsa", ".git/config", "ignored.txt", "build/out.bin"] {
            let write = json!({"path": secret, "contents": "x"});
            assert!(matches!(gate(&ALL, "write_file", &write, &root, &allowed), Gate::Ask(_)), "write {secret}");
            assert!(matches!(gate(&ALL, "read_file", &read(secret), &root, &allowed), Gate::Ask(_)), "read {secret}");
        }
        for plain in ["a.txt", ".env.example", "src/environment.rs", ".github/ci.yml"] {
            assert_eq!(gate(&ALL, "read_file", &read(plain), &root, &allowed), Gate::Run, "read {plain}");
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn validate_rejects_truncated_and_mistyped_input() {
        assert!(validate("write_file", &json!({"path": "a"})).unwrap_err().contains("contents"));
        assert!(validate("write_file", &json!({"path": 5, "contents": ""})).is_err());
        assert!(validate("read_file", &json!({"path": "a", "offset": "1"})).is_err());
        assert!(validate("read_file", &json!("a")).is_err());
        assert!(validate("nope", &json!({})).is_err());
        assert!(validate("list_dir", &json!({})).is_ok());
        assert!(validate("read_file", &json!({"path": "a", "offset": 2})).is_ok());
    }

    #[test]
    fn file_tools_read_write_and_edit_precisely() {
        let root = project("files");
        let done = go("write_file", json!({"path": "src/a.txt", "contents": "one\ntwo\ntwo\n"}), &root);
        assert!(!done.is_error, "{}", done.text);
        assert_eq!(fs::read_to_string(root.join("src/a.txt")).unwrap(), "one\ntwo\ntwo\n");

        let read = go("read_file", json!({"path": "src/a.txt"}), &root);
        assert_eq!(read.text, "1\tone\n2\ttwo\n3\ttwo\n");
        let part = go("read_file", json!({"path": "src/a.txt", "offset": 2, "limit": 1}), &root);
        assert!(part.text.starts_with("2\ttwo\n") && part.text.contains("Showing lines 2-2 of 3"));

        let ambiguous = go("edit_file", json!({"path": "src/a.txt", "old_string": "two", "new_string": "x"}), &root);
        assert!(ambiguous.is_error && ambiguous.text.contains("2 places"));
        assert_eq!(fs::read_to_string(root.join("src/a.txt")).unwrap(), "one\ntwo\ntwo\n", "failed edit changes nothing");

        let all = go("edit_file", json!({"path": "src/a.txt", "old_string": "two", "new_string": "x", "replace_all": true}), &root);
        assert!(!all.is_error);
        assert_eq!(fs::read_to_string(root.join("src/a.txt")).unwrap(), "one\nx\nx\n");

        let missing = go("edit_file", json!({"path": "src/a.txt", "old_string": "zzz", "new_string": "x"}), &root);
        assert!(missing.is_error && missing.text.contains("not found"));

        fs::write(root.join("crlf.txt"), "a\r\nb\r\n").unwrap();
        go("edit_file", json!({"path": "crlf.txt", "old_string": "a\nb", "new_string": "c\nd"}), &root);
        assert_eq!(fs::read(root.join("crlf.txt")).unwrap(), b"c\r\nd\r\n", "CRLF survives an edit");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn listing_and_search_skip_ignored_and_secret_files() {
        let root = project("search");
        fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join(".git/config"), "needle").unwrap();
        fs::write(root.join("ignored.txt"), "needle").unwrap();
        fs::write(root.join(".env"), "needle=secret").unwrap();
        fs::write(root.join("plain.txt"), "first\nthe needle here\n").unwrap();
        fs::write(root.join("binary.bin"), b"needle\xff\xfe").unwrap();
        fs::create_dir(root.join("sub")).unwrap();

        let found = go("search", json!({"pattern": "need.e"}), &root);
        assert_eq!(found.text, "plain.txt:2: the needle here\n");
        assert_eq!(go("search", json!({"pattern": "absent"}), &root).text, "No matches.");
        assert!(go("search", json!({"pattern": "("}), &root).is_error);

        let listed = go("list_dir", json!({}), &root).text;
        assert!(listed.contains("plain.txt") && listed.contains("sub/"));
        assert!(!listed.contains(".git\n") && !listed.contains("ignored.txt"), "{listed}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn commands_report_output_and_failures_without_api_keys() {
        let root = project("commands");
        // SAFETY: tests read this variable only through the child process of this test.
        unsafe { std::env::set_var("OPENROUTER_API_KEY", "sk-should-not-leak") };
        let out = go("run_command", json!({"command": "echo hi; echo oops >&2; pwd; echo key=$OPENROUTER_API_KEY"}), &root);
        assert!(!out.is_error);
        assert!(out.text.contains("hi") && out.text.contains("oops") && out.text.contains(root.to_str().unwrap()));
        assert!(out.text.contains("key=\n") || out.text.ends_with("key="), "{}", out.text);
        assert!(!out.text.contains("sk-should-not-leak"));

        let failed = go("run_command", json!({"command": "echo bad; exit 3"}), &root);
        assert!(failed.is_error && failed.text.contains("bad") && failed.text.contains("Exit code 3"));
        assert_eq!(go("run_command", json!({"command": "true"}), &root).text, "(no output)");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn cancelling_a_command_kills_everything_it_started() {
        let root = project("cancel");
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let marker = root.join("survived");
        let command = format!("(sleep 2; touch {}) & sleep 60", marker.display());
        let dir = root.clone();
        let worker = thread::spawn(move || run_command(&command, &dir, &flag));
        thread::sleep(Duration::from_millis(200));
        let started = Instant::now();
        cancel.store(true, Ordering::Relaxed);
        let out = worker.join().unwrap();
        assert!(out.is_error && out.text.contains("Stopped by the user"));
        assert!(started.elapsed() < Duration::from_secs(5), "should stop promptly");
        thread::sleep(Duration::from_millis(2300));
        assert!(!marker.exists(), "background children must die with the command");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_background_process_holding_the_pipes_does_not_hang_the_tool() {
        let root = project("background");
        let started = Instant::now();
        let out = go("run_command", json!({"command": "sleep 5 & echo started"}), &root);
        assert!(!out.is_error && out.text.contains("started"));
        assert!(started.elapsed() < Duration::from_secs(3), "took {:?}", started.elapsed());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn huge_command_output_keeps_both_ends() {
        let mut capture = Capture::default();
        capture.push(b"START");
        for _ in 0..1000 {
            capture.push(&[b'x'; 1024]);
        }
        capture.push(b"END");
        let text = capture.text();
        assert!(text.starts_with("START") && text.ends_with("END") && text.contains("omitted"));
        assert!(text.len() < 2 * CAPTURE_END_BYTES + 100);
    }

    #[test]
    fn previews_show_only_what_changes() {
        assert_eq!(diff_preview("a\nb\nc\n", "a\nX\nc\n"), "-b\n+X\n");
        assert_eq!(diff_preview("", "new\n"), "+new\n");
        assert_eq!(diff_preview("a\n", "a\n"), " (no change)\n");
        assert_eq!(diff_preview("a\na\n", "a\na\na\n"), "+a\n", "repeated lines don't overlap");
        let root = project("preview");
        fs::write(root.join("f.txt"), "one\ntwo\n").unwrap();
        let input = json!({"path": "f.txt", "old_string": "two", "new_string": "2"});
        let Gate::Ask(preview) = gate(&ALL, "edit_file", &input, &root, &no_allowed()) else { panic!() };
        assert_eq!(preview, "Change f.txt\n-two\n+2\n");
        let missing = json!({"path": "f.txt", "old_string": "zzz", "new_string": "2"});
        assert!(matches!(gate(&ALL, "edit_file", &missing, &root, &no_allowed()), Gate::Deny(_)), "a doomed edit needs no approval");
        fs::remove_dir_all(&root).unwrap();
    }
}
