//! A small Language Server Protocol client: diagnostics and go-to-definition.
//!
//! Language servers run code from the project they're pointed at (build scripts, macros,
//! plugins), so nothing here starts one on its own. `App` asks first, once per project.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{Value, json};

use crate::app::Event;
use crate::editor::Doc;
use crate::tools;

const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
const STDERR_TAIL_BYTES: usize = 2000;

pub struct Spec {
    pub name: &'static str,
    command: &'static [&'static str],
    /// File extension and language id.
    languages: &'static [(&'static str, &'static str)],
}

const RUST: &[(&str, &str)] = &[("rs", "rust")];
const PYTHON: &[(&str, &str)] = &[("py", "python")];
const WEB: &[(&str, &str)] = &[
    ("ts", "typescript"),
    ("tsx", "typescriptreact"),
    ("js", "javascript"),
    ("jsx", "javascriptreact"),
];
const GO: &[(&str, &str)] = &[("go", "go")];
const C: &[(&str, &str)] = &[
    ("c", "c"),
    ("h", "c"),
    ("cpp", "cpp"),
    ("cc", "cpp"),
    ("cxx", "cpp"),
    ("hpp", "cpp"),
];

/// Tried in order. The first one installed for a file's language is used.
pub const SERVERS: [Spec; 6] = [
    Spec {
        name: "rust-analyzer",
        command: &["rust-analyzer"],
        languages: RUST,
    },
    Spec {
        name: "pyright",
        command: &["pyright-langserver", "--stdio"],
        languages: PYTHON,
    },
    Spec {
        name: "pylsp",
        command: &["pylsp"],
        languages: PYTHON,
    },
    Spec {
        name: "typescript-language-server",
        command: &["typescript-language-server", "--stdio"],
        languages: WEB,
    },
    Spec {
        name: "gopls",
        command: &["gopls"],
        languages: GO,
    },
    Spec {
        name: "clangd",
        command: &["clangd"],
        languages: C,
    },
];

impl Spec {
    fn language_of(&self, path: &Path) -> Option<&'static str> {
        let extension = path.extension()?.to_str()?;
        self.languages
            .iter()
            .find(|(known, _)| *known == extension)
            .map(|(_, language)| *language)
    }

    fn is_installed(&self) -> bool {
        std::env::var_os("PATH").is_some_and(|paths| {
            std::env::split_paths(&paths).any(|dir| dir.join(self.command[0]).is_file())
        })
    }
}

/// Which servers could handle a file, installed or not.
pub fn server_names_for(path: &Path) -> Vec<&'static str> {
    SERVERS
        .iter()
        .filter(|spec| spec.language_of(path).is_some())
        .map(|spec| spec.name)
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub line: usize,
    /// 1 error, 2 warning, 3 information, 4 hint.
    pub severity: u8,
    pub message: String,
}

pub enum LspEvent {
    None,
    Diagnostics {
        path: PathBuf,
        diagnostics: Vec<Diagnostic>,
    },
    Definition {
        path: PathBuf,
        line: usize,
        character: usize,
        utf8: bool,
    },
    /// Nothing found. The text says what the server is still doing, if anything: while a
    /// server loads a project it answers "nothing" even for symbols that exist.
    NoDefinition(Option<String>),
    Problem(String),
}

pub fn path_to_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for byte in path.as_os_str().as_encoded_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                uri.push(*byte as char)
            }
            other => uri.push_str(&format!("%{other:02X}")),
        }
    }
    uri
}

pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let encoded = uri.strip_prefix("file://")?.as_bytes();
    let mut bytes = Vec::with_capacity(encoded.len());
    let mut index = 0;
    while index < encoded.len() {
        let escaped = (encoded[index] == b'%')
            .then(|| encoded.get(index + 1..index + 3))
            .flatten();
        let value =
            escaped.and_then(|hex| u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok());
        match value {
            Some(value) => {
                bytes.push(value);
                index += 3;
            }
            None => {
                bytes.push(encoded[index]);
                index += 1;
            }
        }
    }
    let path = PathBuf::from(std::ffi::OsString::from_vec(bytes));
    // Paths from a server should match the ones the editor has open.
    Some(path.canonicalize().unwrap_or(path))
}

/// A byte offset as the column a server expects. The protocol counts UTF-16 units by default.
pub fn to_server_column(line: &str, byte: usize, utf8: bool) -> usize {
    if utf8 {
        byte
    } else {
        line[..byte].encode_utf16().count()
    }
}

/// A server's column as a byte offset, stopping at the end of the line.
pub fn to_byte_column(line: &str, column: usize, utf8: bool) -> usize {
    if utf8 {
        let mut byte = column.min(line.len());
        while !line.is_char_boundary(byte) {
            byte -= 1;
        }
        return byte;
    }
    let mut units = 0;
    for (byte, c) in line.char_indices() {
        if units >= column {
            return byte;
        }
        units += c.len_utf16();
    }
    line.len()
}

/// The first target in any of the shapes a definition response can take.
fn first_location(result: &Value) -> Option<(PathBuf, usize, usize)> {
    let first = match result {
        Value::Array(items) => items.first()?,
        Value::Null => return None,
        single => single,
    };
    let uri = first["uri"].as_str().or(first["targetUri"].as_str())?;
    let range = if first["targetSelectionRange"].is_object() {
        &first["targetSelectionRange"]
    } else {
        &first["range"]
    };
    let start = &range["start"];
    Some((
        uri_to_path(uri)?,
        start["line"].as_u64()? as usize,
        start["character"].as_u64()? as usize,
    ))
}

fn parse_diagnostics(params: &Value) -> Option<LspEvent> {
    let path = uri_to_path(params["uri"].as_str()?)?;
    let mut diagnostics: Vec<Diagnostic> = params["diagnostics"]
        .as_array()?
        .iter()
        .filter_map(|item| {
            Some(Diagnostic {
                line: item["range"]["start"]["line"].as_u64()? as usize,
                severity: item["severity"].as_u64().unwrap_or(1) as u8,
                message: item["message"]
                    .as_str()?
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_owned(),
            })
        })
        .collect();
    diagnostics.sort_by_key(|diagnostic| (diagnostic.line, diagnostic.severity));
    Some(LspEvent::Diagnostics { path, diagnostics })
}

fn read_frames(mut reader: impl BufRead, mut on_message: impl FnMut(Value) -> bool) {
    loop {
        let mut length = None;
        loop {
            let mut header = String::new();
            match reader.read_line(&mut header) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse::<usize>().ok();
            }
        }
        // A missing or absurd length means the stream can't be trusted any further.
        let Some(length) = length.filter(|length| *length <= MAX_MESSAGE_BYTES) else {
            return;
        };
        let mut body = vec![0; length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        if let Ok(message) = serde_json::from_slice(&body)
            && !on_message(message)
        {
            return;
        }
    }
}

struct Opened {
    version: i64,
    revision: u64,
    saves: u32,
}

enum Pending {
    Initialize,
    Definition,
}

struct Client {
    input: Sender<Vec<u8>>,
    child: Child,
    next_id: i64,
    pending: HashMap<i64, Pending>,
    ready: bool,
    queued: Vec<Value>,
    utf8: bool,
    open: HashMap<PathBuf, Opened>,
    /// Long-running work the server has reported and not finished, by its token.
    working_on: HashMap<String, String>,
}

impl Drop for Client {
    fn drop(&mut self) {
        tools::kill_group(self.child.id()); // Servers start helpers, like cargo check.
        let _ = self.child.wait();
    }
}

impl Client {
    fn write(&self, message: &Value) {
        let body = message.to_string();
        let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        frame.extend_from_slice(body.as_bytes());
        let _ = self.input.send(frame);
    }

    /// Everything but the handshake waits until the server says it's ready.
    fn send(&mut self, message: Value) {
        if self.ready {
            self.write(&message);
        } else {
            self.queued.push(message);
        }
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn sync_doc(&mut self, language: &str, doc: &Doc, path: &Path) {
        let uri = path_to_uri(path);
        let (revision, saves) = (doc.buf.revision(), doc.saves);
        let Some(opened) = self.open.get_mut(path) else {
            self.open.insert(
                path.to_path_buf(),
                Opened {
                    version: 1,
                    revision,
                    saves,
                },
            );
            self.notify("textDocument/didOpen", json!({"textDocument": {"uri": uri, "languageId": language, "version": 1, "text": doc.buf.text()}}));
            return;
        };
        let mut changed_version = None;
        if opened.revision != revision {
            opened.revision = revision;
            opened.version += 1;
            changed_version = Some(opened.version);
        }
        let saved = opened.saves != saves;
        opened.saves = saves;
        if let Some(version) = changed_version {
            // ponytail: sends the whole file. Incremental sync if huge files feel slow.
            self.notify("textDocument/didChange", json!({"textDocument": {"uri": uri, "version": version}, "contentChanges": [{"text": doc.buf.text()}]}));
        }
        if saved {
            self.notify(
                "textDocument/didSave",
                json!({"textDocument": {"uri": uri}}),
            );
        }
    }

    fn on_message(&mut self, message: Value) -> LspEvent {
        let method = message["method"].as_str();
        match (method, message.get("id")) {
            // A request from the server. Some hang until they get any answer at all.
            (Some(method), Some(id)) => {
                let result = if method == "workspace/configuration" {
                    let items = message["params"]["items"].as_array().map_or(0, Vec::len);
                    Value::Array(vec![Value::Null; items])
                } else {
                    Value::Null
                };
                self.write(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
                LspEvent::None
            }
            (Some("textDocument/publishDiagnostics"), None) => {
                parse_diagnostics(&message["params"]).unwrap_or(LspEvent::None)
            }
            (Some("$/progress"), None) => {
                let params = &message["params"];
                let token = params["token"].to_string();
                match params["value"]["kind"].as_str() {
                    Some("begin") => {
                        let title = params["value"]["title"].as_str().unwrap_or("working");
                        self.working_on.insert(token, title.to_owned());
                    }
                    Some("end") => {
                        self.working_on.remove(&token);
                    }
                    _ => {}
                }
                LspEvent::None
            }
            (Some(_), None) => LspEvent::None,
            (None, Some(id)) => self.on_response(id.as_i64().unwrap_or(-1), &message),
            (None, None) => LspEvent::None,
        }
    }

    fn on_response(&mut self, id: i64, message: &Value) -> LspEvent {
        let error = message["error"]["message"].as_str();
        match self.pending.remove(&id) {
            None => LspEvent::None, // Includes a definition request that a newer one replaced.
            Some(Pending::Initialize) => {
                if let Some(error) = error {
                    return LspEvent::Problem(format!("it refused to start: {error}"));
                }
                self.utf8 = message["result"]["capabilities"]["positionEncoding"] == "utf-8";
                self.ready = true;
                self.write(&json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}));
                for queued in std::mem::take(&mut self.queued) {
                    self.write(&queued);
                }
                LspEvent::None
            }
            Some(Pending::Definition) => {
                if let Some(error) = error {
                    return LspEvent::Problem(error.to_owned());
                }
                match first_location(&message["result"]) {
                    Some((path, line, character)) => LspEvent::Definition {
                        path,
                        line,
                        character,
                        utf8: self.utf8,
                    },
                    None => LspEvent::NoDefinition(self.working_on.values().next().cloned()),
                }
            }
        }
    }
}

pub struct Servers {
    root: PathBuf,
    tx: SyncSender<Event>,
    clients: HashMap<usize, Client>,
    /// Servers that stopped, so they don't restart in a loop.
    failed: HashSet<usize>,
    /// Replaces every server's command. Tests point this at a fake server.
    pub command_override: Option<Vec<String>>,
}

impl Servers {
    pub fn new(root: PathBuf, tx: SyncSender<Event>) -> Servers {
        Servers {
            root,
            tx,
            clients: HashMap::new(),
            failed: HashSet::new(),
            command_override: None,
        }
    }

    /// The server to use for a file, if one is installed.
    pub fn index_for(&self, path: &Path) -> Option<usize> {
        SERVERS.iter().position(|spec| {
            spec.language_of(path).is_some()
                && (self.command_override.is_some() || spec.is_installed())
        })
    }

    pub fn is_running(&self, index: usize) -> bool {
        self.clients.contains_key(&index)
    }

    pub fn has_failed(&self, index: usize) -> bool {
        self.failed.contains(&index)
    }

    pub fn start(&mut self, index: usize) -> Result<(), String> {
        let command: Vec<String> = match &self.command_override {
            Some(command) => command.clone(),
            None => SERVERS[index]
                .command
                .iter()
                .map(|part| (*part).to_owned())
                .collect(),
        };
        let mut builder = Command::new(&command[0]);
        builder
            .args(&command[1..])
            .current_dir(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        for var in tools::SECRET_VARS {
            builder.env_remove(var);
        }
        let mut child = builder.spawn().map_err(|err| {
            self.failed.insert(index);
            err.to_string()
        })?;
        let (Some(mut stdin), Some(stdout), Some(mut stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err("couldn't connect to it".to_owned());
        };

        // Writes go through their own thread, so a stuck server can't block the UI.
        let (input, queue) = mpsc::channel::<Vec<u8>>();
        thread::spawn(move || {
            while let Ok(frame) = queue.recv() {
                if stdin
                    .write_all(&frame)
                    .and_then(|()| stdin.flush())
                    .is_err()
                {
                    break;
                }
            }
        });
        // Kept so a server that dies can explain why.
        let tail = Arc::new(Mutex::new(Vec::new()));
        let stderr_tail = tail.clone();
        let stderr_thread = thread::spawn(move || {
            let mut chunk = [0; 4096];
            while let Ok(count) = stderr.read(&mut chunk) {
                if count == 0 {
                    break;
                }
                let mut kept = stderr_tail.lock().unwrap();
                kept.extend_from_slice(&chunk[..count]);
                let extra = kept.len().saturating_sub(STDERR_TAIL_BYTES);
                kept.drain(..extra);
            }
        });
        let tx = self.tx.clone();
        thread::spawn(move || {
            read_frames(BufReader::new(stdout), |message| {
                tx.send(Event::Lsp {
                    server: index,
                    message,
                })
                .is_ok()
            });
            let _ = stderr_thread.join();
            let bytes = tail.lock().unwrap().clone();
            let reason = String::from_utf8_lossy(&bytes)
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("")
                .to_owned();
            let _ = tx.send(Event::LspExit {
                server: index,
                reason,
            });
        });

        let mut client = Client {
            input,
            child,
            next_id: 1,
            pending: HashMap::new(),
            ready: false,
            queued: Vec::new(),
            utf8: false,
            open: HashMap::new(),
            working_on: HashMap::new(),
        };
        client.pending.insert(0, Pending::Initialize);
        client.write(&json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
            "processId": std::process::id(),
            "rootUri": path_to_uri(&self.root),
            "workspaceFolders": [{"uri": path_to_uri(&self.root), "name": "project"}],
            "capabilities": {
                "general": {"positionEncodings": ["utf-8", "utf-16"]},
                "textDocument": {"publishDiagnostics": {}, "definition": {"linkSupport": true}, "synchronization": {"didSave": true}},
                "workspace": {"configuration": true},
                "window": {"workDoneProgress": true},
            },
        }}));
        self.failed.remove(&index);
        self.clients.insert(index, client);
        Ok(())
    }

    /// Brings each server up to date with the open files.
    pub fn sync(&mut self, docs: &[Doc]) {
        for (index, client) in &mut self.clients {
            let spec = &SERVERS[*index];
            let mut wanted = HashSet::new();
            for doc in docs.iter().filter(|doc| !doc.is_proposal()) {
                let Some(path) = doc.path.as_deref() else {
                    continue;
                };
                if let Some(language) = spec.language_of(path) {
                    wanted.insert(path.to_path_buf());
                    client.sync_doc(language, doc, path);
                }
            }
            let closed: Vec<PathBuf> = client
                .open
                .keys()
                .filter(|path| !wanted.contains(*path))
                .cloned()
                .collect();
            for path in closed {
                client.open.remove(&path);
                client.notify(
                    "textDocument/didClose",
                    json!({"textDocument": {"uri": path_to_uri(&path)}}),
                );
            }
        }
    }

    /// Asks where the symbol under the cursor is defined. The answer arrives as an event.
    pub fn definition(&mut self, index: usize, doc: &Doc) {
        let Some(path) = doc.path.as_deref() else {
            return;
        };
        let Some(client) = self.clients.get_mut(&index) else {
            return;
        };
        let position = doc.buf.cursor;
        let character =
            to_server_column(&doc.buf.lines()[position.line], position.col, client.utf8);
        let id = client.next_id;
        client.next_id += 1;
        // Only the newest answer matters: jumping for an old request would be a surprise.
        client
            .pending
            .retain(|_, pending| !matches!(pending, Pending::Definition));
        client.pending.insert(id, Pending::Definition);
        client.send(
            json!({"jsonrpc": "2.0", "id": id, "method": "textDocument/definition", "params": {
                "textDocument": {"uri": path_to_uri(path)},
                "position": {"line": position.line, "character": character},
            }}),
        );
    }

    pub fn on_message(&mut self, server: usize, message: Value) -> LspEvent {
        match self.clients.get_mut(&server) {
            Some(client) => client.on_message(message),
            None => LspEvent::None,
        }
    }

    /// Returns whether this was a running server, as opposed to one already closed.
    pub fn on_exit(&mut self, server: usize) -> bool {
        self.failed.insert(server);
        self.clients.remove(&server).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_round_trip_with_spaces_and_unicode() {
        let path = Path::new("/tmp/no such dir/kalipso é 日本/a b.rs");
        let uri = path_to_uri(path);
        assert_eq!(
            uri,
            "file:///tmp/no%20such%20dir/kalipso%20%C3%A9%20%E6%97%A5%E6%9C%AC/a%20b.rs"
        );
        assert_eq!(uri_to_path(&uri).unwrap(), path);
        assert_eq!(
            uri_to_path("file:///tmp/nope/a%2eb.rs").unwrap(),
            Path::new("/tmp/nope/a.b.rs"),
            "lowercase and needless escapes"
        );
        assert_eq!(
            uri_to_path("file:///tmp/nope/100%").unwrap(),
            Path::new("/tmp/nope/100%"),
            "a stray % is left alone"
        );
        assert_eq!(uri_to_path("https://example.com/a.rs"), None);
    }

    #[test]
    fn columns_convert_between_bytes_and_each_encoding() {
        let line = "é😀x"; // Bytes: 2 + 4 + 1. UTF-16 units: 1 + 2 + 1.
        assert_eq!(to_server_column(line, 2, false), 1);
        assert_eq!(to_server_column(line, 6, false), 3);
        assert_eq!(to_server_column(line, 6, true), 6);
        assert_eq!(to_byte_column(line, 1, false), 2);
        assert_eq!(to_byte_column(line, 3, false), 6);
        assert_eq!(
            to_byte_column(line, 2, false),
            6,
            "inside a surrogate pair moves past it, not into it"
        );
        assert_eq!(
            to_byte_column(line, 99, false),
            line.len(),
            "past the end stops at the end"
        );
        assert_eq!(to_byte_column(line, 6, true), 6);
        assert_eq!(
            to_byte_column(line, 3, true),
            2,
            "inside a character moves back to its start"
        );
        assert_eq!(to_byte_column(line, 99, true), line.len());
    }

    #[test]
    fn definition_responses_are_understood_in_every_shape() {
        let location = json!({"uri": "file:///nope/a.rs", "range": {"start": {"line": 3, "character": 7}, "end": {"line": 3, "character": 9}}});
        let expected = Some((PathBuf::from("/nope/a.rs"), 3, 7));
        assert_eq!(first_location(&location), expected);
        assert_eq!(
            first_location(&json!([location, {"uri": "file:///other"}])),
            expected
        );
        let link = json!([{
            "targetUri": "file:///nope/a.rs",
            "targetRange": {"start": {"line": 0, "character": 0}},
            "targetSelectionRange": {"start": {"line": 3, "character": 7}},
        }]);
        assert_eq!(
            first_location(&link),
            expected,
            "the name's position, not the whole item"
        );
        assert_eq!(first_location(&json!(null)), None);
        assert_eq!(first_location(&json!([])), None);
        assert_eq!(
            first_location(&json!({"uri": 5})),
            None,
            "malformed answers are ignored"
        );
    }

    #[test]
    fn diagnostics_are_sorted_and_keep_only_the_first_line_of_a_message() {
        let params = json!({"uri": "file:///nope/a.rs", "diagnostics": [
            {"range": {"start": {"line": 9, "character": 0}}, "severity": 2, "message": "later"},
            {"range": {"start": {"line": 1, "character": 0}}, "message": "first\nmore detail"},
            {"range": {}, "message": "no position"},
        ]});
        let Some(LspEvent::Diagnostics { path, diagnostics }) = parse_diagnostics(&params) else {
            panic!()
        };
        assert_eq!(path, Path::new("/nope/a.rs"));
        assert_eq!(
            diagnostics,
            [
                Diagnostic {
                    line: 1,
                    severity: 1,
                    message: "first".to_owned()
                },
                Diagnostic {
                    line: 9,
                    severity: 2,
                    message: "later".to_owned()
                },
            ]
        );
    }

    fn frames(input: &[u8]) -> Vec<Value> {
        let mut seen = Vec::new();
        read_frames(input, |message| {
            seen.push(message);
            true
        });
        seen
    }

    #[test]
    fn frames_are_parsed_and_bad_streams_stop_cleanly() {
        let body = r#"{"a":1}"#;
        let good = format!(
            "Content-Length: {}\r\nContent-Type: x\r\n\r\n{body}content-length:{}\r\n\r\n{body}",
            body.len(),
            body.len()
        );
        assert_eq!(
            frames(good.as_bytes()).len(),
            2,
            "two messages, either header case"
        );
        assert!(
            frames(b"Content-Length: 99\r\n\r\nshort").is_empty(),
            "a truncated body yields nothing"
        );
        assert!(
            frames(b"X-Other: 1\r\n\r\n{}").is_empty(),
            "no length means stop"
        );
        let huge = format!("Content-Length: {}\r\n\r\n", MAX_MESSAGE_BYTES + 1);
        assert!(
            frames(huge.as_bytes()).is_empty(),
            "an absurd length isn't allocated"
        );
        let bad_json = "Content-Length: 3\r\n\r\nnopContent-Length: 2\r\n\r\n{}";
        assert_eq!(
            frames(bad_json.as_bytes()).len(),
            1,
            "one unparseable message doesn't end the stream"
        );
    }

    #[test]
    fn server_lookup_goes_by_extension() {
        assert_eq!(server_names_for(Path::new("a.rs")), ["rust-analyzer"]);
        assert_eq!(server_names_for(Path::new("a.py")), ["pyright", "pylsp"]);
        assert!(server_names_for(Path::new("a.txt")).is_empty());
        assert!(server_names_for(Path::new("Makefile")).is_empty());
    }
}
