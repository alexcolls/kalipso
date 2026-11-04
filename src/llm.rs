//! Streaming clients for the wire formats the four providers need.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};
use ureq::Agent;

use crate::tools::{ToolDef, ToolOutput, definitions};

pub const DEFAULT_CLAUDE_MODEL: &str = "claude-opus-5-5";
const ANTHROPIC_VERSION: &str = "2026-10-21";
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
/// Models that accept the server-side `fallbacks` parameter.
const FALLBACK_MODELS: [&str; 4] = ["claude-fable-5-1", "claude-opus-5-5", "claude-opus-5", "claude-sonnet-5-5"];
const MAX_TOKENS: u32 = 64000;
const MAX_RETRIES: u32 = 2;
/// Applies to each read, so long streams are fine as long as data keeps arriving.
const READ_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wire {
    Messages,
    Responses,
    ChatCompletions,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Provider {
    Claude,
    OpenAi,
    Grok,
    OpenRouter,
}

impl Provider {
    pub const ALL: [Provider; 4] = [Provider::Claude, Provider::OpenAi, Provider::Grok, Provider::OpenRouter];

    pub fn name(self) -> &'static str {
        match self {
            Provider::Claude => "Claude",
            Provider::OpenAi => "OpenAI",
            Provider::Grok => "Grok",
            Provider::OpenRouter => "OpenRouter",
        }
    }

    /// Name used in the config file.
    pub fn id(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::OpenAi => "openai",
            Provider::Grok => "grok",
            Provider::OpenRouter => "openrouter",
        }
    }

    pub fn key_env(self) -> &'static str {
        match self {
            Provider::Claude => "ANTHROPIC_API_KEY",
            Provider::OpenAi => "OPENAI_API_KEY",
            Provider::Grok => "XAI_API_KEY",
            Provider::OpenRouter => "OPENROUTER_API_KEY",
        }
    }

    pub fn wire(self) -> Wire {
        match self {
            Provider::Claude => Wire::Messages,
            Provider::OpenAi | Provider::Grok => Wire::Responses,
            Provider::OpenRouter => Wire::ChatCompletions,
        }
    }

    pub fn api_key(self) -> Option<String> {
        std::env::var(self.key_env()).ok().filter(|key| !key.is_empty())
    }

    /// The API root. `<PROVIDER>_BASE_URL` overrides it, for proxies and gateways.
    pub fn base_url(self) -> String {
        let (var, default) = match self {
            Provider::Claude => ("ANTHROPIC_BASE_URL", "https://api.anthropic.com/v1"),
            Provider::OpenAi => ("OPENAI_BASE_URL", "https://api.openai.com/v1"),
            Provider::Grok => ("XAI_BASE_URL", "https://api.x.ai/v1"),
            Provider::OpenRouter => ("OPENROUTER_BASE_URL", "https://openrouter.ai/api/v1"),
        };
        std::env::var(var).ok().filter(|url| !url.is_empty()).unwrap_or_else(|| default.to_owned())
    }
}

/// What the /v1/models endpoint says a Claude model accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClaudeFeatures {
    pub adaptive_thinking: bool,
    pub effort: bool,
}

pub struct Request {
    pub wire: Wire,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub system: String,
    pub tool_names: Vec<&'static str>,
    pub history: Vec<Value>,
    pub claude: ClaudeFeatures,
    /// Set once a provider refuses `store: false`, so later requests skip it.
    pub responses_stored: Arc<AtomicBool>,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Delta {
    Text(String),
    Thinking(String),
    ToolStart(String),
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// The parsed input, or the raw text if it wasn't valid JSON.
    pub input: Result<Value, String>,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Stop {
    EndTurn,
    ToolUse,
    MaxTokens,
    Refusal(Option<String>),
    Other(String),
}

#[derive(Debug)]
pub struct Turn {
    /// Provider-native items to append to the history unchanged.
    pub items: Vec<Value>,
    pub calls: Vec<ToolCall>,
    pub stop: Stop,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

pub fn user_message(text: &str) -> Value {
    json!({"role": "user", "content": text})
}

/// History items carrying tool results. Claude needs them together in one user message.
pub fn tool_results(wire: Wire, results: &[(String, ToolOutput)]) -> Vec<Value> {
    match wire {
        Wire::Messages => {
            let content: Vec<Value> = results
                .iter()
                .map(|(id, output)| {
                    let mut block = json!({"type": "tool_result", "tool_use_id": id, "content": output.text});
                    if output.is_error {
                        block["is_error"] = json!(true);
                    }
                    block
                })
                .collect();
            vec![json!({"role": "user", "content": content})]
        }
        Wire::Responses => results
            .iter()
            .map(|(id, output)| json!({"type": "function_call_output", "call_id": id, "output": marked(output)}))
            .collect(),
        Wire::ChatCompletions => results
            .iter()
            .map(|(id, output)| json!({"role": "tool", "tool_call_id": id, "content": marked(output)}))
            .collect(),
    }
}

/// These formats have no error flag for tool results, so errors are marked in the text.
fn marked(output: &ToolOutput) -> String {
    if output.is_error { format!("ERROR: {}", output.text) } else { output.text.clone() }
}

fn function_tools(defs: &[ToolDef], nested: bool) -> Vec<Value> {
    defs.iter()
        .map(|def| {
            if nested {
                json!({"type": "function", "function": {"name": def.name, "description": def.description, "parameters": def.schema}})
            } else {
                json!({"type": "function", "name": def.name, "description": def.description, "parameters": def.schema})
            }
        })
        .collect()
}

fn uses_fallbacks(request: &Request) -> bool {
    request.wire == Wire::Messages && FALLBACK_MODELS.contains(&request.model.as_str())
}

/// The endpoint path and JSON body for a request.
pub fn build_body(request: &Request) -> (&'static str, Value) {
    let defs = definitions(&request.tool_names);
    match request.wire {
        Wire::Messages => {
            let mut body = json!({
                "model": request.model,
                "max_tokens": MAX_TOKENS,
                "stream": true,
                "system": request.system,
                "messages": request.history,
                "cache_control": {"type": "ephemeral"},
            });
            if !defs.is_empty() {
                let tools: Vec<Value> = defs
                    .iter()
                    .map(|def| json!({"name": def.name, "description": def.description, "input_schema": def.schema, "eager_input_streaming": true}))
                    .collect();
                body["tools"] = json!(tools);
            }
            if request.claude.adaptive_thinking {
                body["thinking"] = json!({"type": "adaptive", "display": "summarized"});
            }
            if request.claude.effort {
                body["output_config"] = json!({"effort": "high"});
            }
            if uses_fallbacks(request) {
                body["fallbacks"] = json!("default");
            }
            ("/messages", body)
        }
        Wire::Responses => {
            let mut body = json!({
                "model": request.model,
                "instructions": request.system,
                "input": request.history,
                "stream": true,
            });
            if !defs.is_empty() {
                body["tools"] = json!(function_tools(&defs, false));
            }
            if !request.responses_stored.load(Ordering::Relaxed) {
                // Keeps code out of the provider's stored responses where that's supported.
                body["store"] = json!(false);
                body["include"] = json!(["reasoning.encrypted_content"]);
            }
            ("/responses", body)
        }
        Wire::ChatCompletions => {
            let mut messages = vec![json!({"role": "system", "content": request.system})];
            messages.extend(request.history.iter().cloned());
            let mut body = json!({
                "model": request.model,
                "stream": true,
                "stream_options": {"include_usage": true},
                "messages": messages,
            });
            if !defs.is_empty() {
                body["tools"] = json!(function_tools(&defs, true));
            }
            ("/chat/completions", body)
        }
    }
}

fn agent() -> Agent {
    Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(READ_TIMEOUT))
        .timeout_per_read(Some(READ_TIMEOUT))
        .build()
        .into()
}

/// Pulls the human-readable message out of an API error body. All three formats use error.message.
fn error_text(status: u16, body: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let message = parsed.as_ref().and_then(|v| v["error"]["message"].as_str().or(v["message"].as_str()));
    match message {
        Some(message) => format!("HTTP {status}: {message}"),
        None => {
            let short: String = body.chars().take(300).collect();
            format!("HTTP {status}: {short}")
        }
    }
}

fn is_retryable(status: u16) -> bool {
    matches!(status, 408 | 409 | 429) || status >= 500
}

/// Sleeps in short steps so Esc still works. Returns false if cancelled.
fn wait(duration: Duration, cancel: &AtomicBool) -> bool {
    let step = Duration::from_millis(50);
    let mut left = duration;
    while !left.is_zero() {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        thread::sleep(step.min(left));
        left = left.saturating_sub(step);
    }
    !cancel.load(Ordering::Relaxed)
}

/// Runs one model turn, calling `emit` as text arrives. `Ok(None)` means it was cancelled.
pub fn stream(request: &Request, cancel: &AtomicBool, mut emit: impl FnMut(Delta)) -> Result<Option<Turn>, String> {
    let agent = agent();
    let mut attempt = 0;
    let response = loop {
        let (path, body) = build_body(request);
        let url = format!("{}{path}", request.base_url);
        let mut call = agent.post(&url).header("content-type", "application/json");
        call = match request.wire {
            Wire::Messages => {
                let call = call.header("x-api-key", &request.api_key).header("anthropic-version", ANTHROPIC_VERSION);
                if uses_fallbacks(request) { call.header("anthropic-beta", FALLBACK_BETA) } else { call }
            }
            _ => call.header("authorization", &format!("Bearer {}", request.api_key)),
        };
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let delay = match call.send(body.to_string()) {
            Ok(response) if response.status().is_success() => break response,
            Ok(mut response) => {
                let status = response.status().as_u16();
                let retry_after = response
                    .headers()
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok());
                let text = response.body_mut().read_to_string().unwrap_or_default();
                let lowered = text.to_lowercase();
                if status == 400
                    && request.wire == Wire::Responses
                    && !request.responses_stored.load(Ordering::Relaxed)
                    && ["store", "include", "encrypted"].iter().any(|word| lowered.contains(word))
                {
                    request.responses_stored.store(true, Ordering::Relaxed);
                    continue;
                }
                if !is_retryable(status) || attempt >= MAX_RETRIES {
                    return Err(error_text(status, &text));
                }
                Duration::from_secs(retry_after.unwrap_or(2 + 4 * attempt as u64).min(30))
            }
            Err(err) => {
                if attempt >= MAX_RETRIES {
                    return Err(format!("Network error: {err}"));
                }
                Duration::from_secs(2 + 4 * attempt as u64)
            }
        };
        attempt += 1;
        if !wait(delay, cancel) {
            return Ok(None);
        }
    };

    let reader = BufReader::new(response.into_body().into_reader());
    let mut parser: Box<dyn Parser> = match request.wire {
        Wire::Messages => Box::<MessagesParser>::default(),
        Wire::Responses => Box::<ResponsesParser>::default(),
        Wire::ChatCompletions => Box::<ChatParser>::default(),
    };
    let finished = read_sse(reader, cancel, |data| {
        let value: Value = serde_json::from_str(data).map_err(|err| format!("Bad data from the provider: {err}"))?;
        parser.event(&value, &mut emit)
    })?;
    if !finished {
        return Ok(None);
    }
    parser.finish().map(Some)
}

/// Feeds each server-sent event's data to `on_data`. Returns false if cancelled.
fn read_sse(reader: impl BufRead, cancel: &AtomicBool, mut on_data: impl FnMut(&str) -> Result<(), String>) -> Result<bool, String> {
    let mut data = String::new();
    for line in reader.lines() {
        if cancel.load(Ordering::Relaxed) {
            return Ok(false);
        }
        let line = line.map_err(|err| format!("The connection dropped: {err}"))?;
        if line.is_empty() {
            if data == "[DONE]" {
                return Ok(true);
            }
            if !data.is_empty() {
                on_data(&data)?;
                data.clear();
            }
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        }
        // Other lines are `event:` names, which repeat the payload's type, and `:` comments.
    }
    if !data.is_empty() && data != "[DONE]" {
        on_data(&data)?;
    }
    Ok(true)
}

/// Assembles one wire format's stream into a [`Turn`].
trait Parser {
    fn event(&mut self, data: &Value, emit: &mut dyn FnMut(Delta)) -> Result<(), String>;
    fn finish(self: Box<Self>) -> Result<Turn, String>;
}

const CUT_OFF: &str = "The connection closed before the response finished.";

fn text_of(value: &Value) -> String {
    value.as_str().unwrap_or("").to_owned()
}

fn parse_arguments(raw: &str) -> Result<Value, String> {
    let text = if raw.trim().is_empty() { "{}" } else { raw };
    serde_json::from_str(text).map_err(|_| raw.to_owned())
}

#[derive(Default)]
struct MessagesParser {
    blocks: Vec<Value>,
    partial_input: BTreeMap<usize, String>,
    stop: Option<String>,
    refusal_category: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
    done: bool,
}

impl Parser for MessagesParser {
    fn event(&mut self, data: &Value, emit: &mut dyn FnMut(Delta)) -> Result<(), String> {
        let index = data["index"].as_u64().unwrap_or(0) as usize;
        match data["type"].as_str().unwrap_or("") {
            "message_start" => {
                let usage = &data["message"]["usage"];
                self.input_tokens = ["input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"]
                    .iter()
                    .filter_map(|field| usage[field].as_u64())
                    .sum();
                self.output_tokens = usage["output_tokens"].as_u64().unwrap_or(0);
            }
            "content_block_start" => {
                let block = data["content_block"].clone();
                if block["type"] == "tool_use" {
                    emit(Delta::ToolStart(text_of(&block["name"])));
                }
                if self.blocks.len() <= index {
                    self.blocks.resize(index + 1, Value::Null);
                }
                self.blocks[index] = block;
            }
            "content_block_delta" => {
                let delta = &data["delta"];
                let Some(block) = self.blocks.get_mut(index) else { return Ok(()) };
                let mut append = |field: &str, text: &str| {
                    let joined = format!("{}{text}", block[field].as_str().unwrap_or(""));
                    block[field] = json!(joined);
                };
                match delta["type"].as_str().unwrap_or("") {
                    "text_delta" => {
                        let text = delta["text"].as_str().unwrap_or("");
                        append("text", text);
                        emit(Delta::Text(text.to_owned()));
                    }
                    "thinking_delta" => {
                        let text = delta["thinking"].as_str().unwrap_or("");
                        append("thinking", text);
                        emit(Delta::Thinking(text.to_owned()));
                    }
                    "signature_delta" => append("signature", delta["signature"].as_str().unwrap_or("")),
                    "input_json_delta" => {
                        self.partial_input.entry(index).or_default().push_str(delta["partial_json"].as_str().unwrap_or(""));
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(stop) = data["delta"]["stop_reason"].as_str() {
                    self.stop = Some(stop.to_owned());
                }
                self.refusal_category = data["delta"]["stop_details"]["category"].as_str().map(str::to_owned);
                if let Some(tokens) = data["usage"]["output_tokens"].as_u64() {
                    self.output_tokens = tokens;
                }
            }
            "message_stop" => self.done = true,
            "error" => return Err(text_of(&data["error"]["message"])),
            _ => {}
        }
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<Turn, String> {
        if !self.done {
            return Err(CUT_OFF.to_owned());
        }
        let mut raw = BTreeMap::new();
        for (index, block) in self.blocks.iter_mut().enumerate() {
            if block["type"] == "tool_use"
                && let Some(partial) = self.partial_input.remove(&index)
            {
                match parse_arguments(&partial) {
                    Ok(input) => block["input"] = input,
                    Err(text) => {
                        block["input"] = json!({});
                        raw.insert(text_of(&block["id"]), text);
                    }
                }
            }
        }
        let blocks = replayable_blocks(std::mem::take(&mut self.blocks));
        let calls = blocks
            .iter()
            .filter(|block| block["type"] == "tool_use")
            .map(|block| {
                let id = text_of(&block["id"]);
                let input = raw.remove(&id).map_or_else(|| Ok(block["input"].clone()), Err);
                ToolCall { name: text_of(&block["name"]), id, input }
            })
            .collect();
        let stop = match self.stop.as_deref() {
            Some("end_turn" | "stop_sequence") => Stop::EndTurn,
            Some("tool_use") => Stop::ToolUse,
            Some("max_tokens") => Stop::MaxTokens,
            Some("refusal") => Stop::Refusal(self.refusal_category.take()),
            other => Stop::Other(other.unwrap_or("unknown").to_owned()),
        };
        let items = if blocks.is_empty() { Vec::new() } else { vec![json!({"role": "assistant", "content": blocks})] };
        Ok(Turn { items, calls, stop, input_tokens: self.input_tokens, output_tokens: self.output_tokens })
    }
}

/// Prepares a turn's blocks for the next request. After a mid-output fallback, only text
/// may be replayed from before the last `fallback` block. Empty text blocks are rejected
/// by the API, so those go too. Everything else goes back unchanged, thinking included.
pub fn replayable_blocks(blocks: Vec<Value>) -> Vec<Value> {
    let last_fallback = blocks.iter().rposition(|block| block["type"] == "fallback");
    blocks
        .into_iter()
        .enumerate()
        .filter(|(index, block)| {
            if block.is_null() || (block["type"] == "text" && block["text"].as_str().unwrap_or("").is_empty()) {
                return false;
            }
            match last_fallback {
                Some(last) if *index < last => block["type"] == "text",
                _ => true,
            }
        })
        .map(|(_, block)| block)
        .collect()
}

#[derive(Default)]
struct ResponsesParser {
    output: Vec<Value>,
    final_output: Option<Vec<Value>>,
    status: Option<String>,
    incomplete_reason: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
}

impl Parser for ResponsesParser {
    fn event(&mut self, data: &Value, emit: &mut dyn FnMut(Delta)) -> Result<(), String> {
        match data["type"].as_str().unwrap_or("") {
            "response.output_text.delta" | "response.refusal.delta" => emit(Delta::Text(text_of(&data["delta"]))),
            "response.reasoning_summary_text.delta" => emit(Delta::Thinking(text_of(&data["delta"]))),
            "response.output_item.added" if data["item"]["type"] == "function_call" => {
                emit(Delta::ToolStart(text_of(&data["item"]["name"])));
            }
            "response.output_item.done" => self.output.push(data["item"].clone()),
            "response.completed" | "response.incomplete" => {
                let response = &data["response"];
                self.status = response["status"].as_str().map(str::to_owned).or(Some("completed".to_owned()));
                self.incomplete_reason = response["incomplete_details"]["reason"].as_str().map(str::to_owned);
                self.input_tokens = response["usage"]["input_tokens"].as_u64().unwrap_or(0);
                self.output_tokens = response["usage"]["output_tokens"].as_u64().unwrap_or(0);
                self.final_output = response["output"].as_array().filter(|items| !items.is_empty()).cloned();
            }
            "response.failed" => return Err(text_of(&data["response"]["error"]["message"])),
            "error" => {
                let message = data["message"].as_str().or(data["error"]["message"].as_str()).unwrap_or("Unknown error");
                return Err(message.to_owned());
            }
            _ => {}
        }
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<Turn, String> {
        let Some(status) = self.status else { return Err(CUT_OFF.to_owned()) };
        let items = self.final_output.unwrap_or(self.output);
        let calls: Vec<ToolCall> = items
            .iter()
            .filter(|item| item["type"] == "function_call")
            .map(|item| ToolCall {
                id: text_of(&item["call_id"]),
                name: text_of(&item["name"]),
                input: parse_arguments(item["arguments"].as_str().unwrap_or("")),
            })
            .collect();
        let stop = match (status.as_str(), self.incomplete_reason.as_deref()) {
            ("incomplete", Some("max_output_tokens")) => Stop::MaxTokens,
            ("incomplete", Some("content_filter")) => Stop::Refusal(Some("content_filter".to_owned())),
            ("incomplete", reason) => Stop::Other(reason.unwrap_or("incomplete").to_owned()),
            _ if !calls.is_empty() => Stop::ToolUse,
            _ => Stop::EndTurn,
        };
        Ok(Turn { items, calls, stop, input_tokens: self.input_tokens, output_tokens: self.output_tokens })
    }
}

#[derive(Default)]
struct ChatParser {
    text: String,
    calls: BTreeMap<u64, (String, String, String)>,
    finish_reason: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
}

impl Parser for ChatParser {
    fn event(&mut self, data: &Value, emit: &mut dyn FnMut(Delta)) -> Result<(), String> {
        let choice = &data["choices"][0];
        let error = data["error"]["message"].as_str().or(choice["error"]["message"].as_str());
        if let Some(message) = error {
            return Err(message.to_owned());
        }
        if let Some(tokens) = data["usage"]["prompt_tokens"].as_u64() {
            self.input_tokens = tokens;
        }
        if let Some(tokens) = data["usage"]["completion_tokens"].as_u64() {
            self.output_tokens = tokens;
        }
        let delta = &choice["delta"];
        if let Some(text) = delta["content"].as_str().filter(|text| !text.is_empty()) {
            self.text.push_str(text);
            emit(Delta::Text(text.to_owned()));
        }
        if let Some(text) = delta["reasoning"].as_str().filter(|text| !text.is_empty()) {
            emit(Delta::Thinking(text.to_owned()));
        }
        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            let entry = self.calls.entry(call["index"].as_u64().unwrap_or(0)).or_default();
            if let Some(id) = call["id"].as_str() {
                entry.0 = id.to_owned();
            }
            if let Some(name) = call["function"]["name"].as_str().filter(|name| !name.is_empty()) {
                entry.1 = name.to_owned();
                emit(Delta::ToolStart(name.to_owned()));
            }
            entry.2.push_str(call["function"]["arguments"].as_str().unwrap_or(""));
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            self.finish_reason = Some(reason.to_owned());
        }
        Ok(())
    }

    // ponytail: reasoning_details aren't replayed. Some OpenRouter models may want them
    // across tool calls; add when one rejects the history or loses context.
    fn finish(self: Box<Self>) -> Result<Turn, String> {
        let Some(reason) = self.finish_reason else { return Err(CUT_OFF.to_owned()) };
        let calls: Vec<ToolCall> = self
            .calls
            .values()
            .map(|(id, name, args)| ToolCall { id: id.clone(), name: name.clone(), input: parse_arguments(args) })
            .collect();
        let stop = match reason.as_str() {
            "stop" => Stop::EndTurn,
            "tool_calls" => Stop::ToolUse,
            "length" => Stop::MaxTokens,
            "content_filter" => Stop::Refusal(Some("content_filter".to_owned())),
            "error" => return Err("The provider reported an error mid-response.".to_owned()),
            other if !calls.is_empty() => Stop::Other(other.to_owned()),
            _ => Stop::EndTurn,
        };
        let mut item = json!({"role": "assistant", "content": if self.text.is_empty() { Value::Null } else { json!(self.text) }});
        if !self.calls.is_empty() {
            let tool_calls: Vec<Value> = self
                .calls
                .values()
                .map(|(id, name, args)| json!({"id": id, "type": "function", "function": {"name": name, "arguments": args}}))
                .collect();
            item["tool_calls"] = json!(tool_calls);
        }
        Ok(Turn { items: vec![item], calls, stop, input_tokens: self.input_tokens, output_tokens: self.output_tokens })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    pub features: ClaudeFeatures,
}

/// Lists the chat models a provider offers, newest first where the API gives dates.
pub fn list_models(provider: Provider, base_url: &str, api_key: &str) -> Result<Vec<ModelInfo>, String> {
    let query = if provider == Provider::Claude { "?limit=1000" } else { "" };
    let url = format!("{base_url}/models{query}");
    let call = agent().get(&url);
    let call = if provider.wire() == Wire::Messages {
        call.header("x-api-key", api_key).header("anthropic-version", ANTHROPIC_VERSION)
    } else {
        call.header("authorization", &format!("Bearer {api_key}"))
    };
    let mut response = call.call().map_err(|err| format!("Network error: {err}"))?;
    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .with_config()
        .limit(64 * 1024 * 1024)
        .read_to_string()
        .map_err(|err| format!("Network error: {err}"))?;
    if !(200..300).contains(&status) {
        return Err(error_text(status, &body));
    }
    let parsed: Value = serde_json::from_str(&body).map_err(|err| format!("Bad data from the provider: {err}"))?;
    Ok(models_from(provider, &parsed))
}

fn models_from(provider: Provider, parsed: &Value) -> Vec<ModelInfo> {
    const NOT_CHAT: [&str; 11] =
        ["embedding", "whisper", "tts", "dall-e", "moderation", "image", "audio", "realtime", "transcribe", "sora", "imagine"];
    let mut entries: Vec<&Value> = parsed["data"].as_array().into_iter().flatten().collect();
    match provider {
        Provider::Claude => {}
        Provider::OpenAi | Provider::Grok => {
            entries.retain(|model| {
                let id = model["id"].as_str().unwrap_or("");
                !NOT_CHAT.iter().any(|word| id.contains(word))
            });
            entries.sort_by_key(|model| std::cmp::Reverse(model["created"].as_u64().unwrap_or(0)));
        }
        Provider::OpenRouter => {
            // Agents need tool calling. Keep models that don't say either way.
            entries.retain(|model| match model["supported_parameters"].as_array() {
                Some(parameters) => parameters.iter().any(|p| p == "tools"),
                None => true,
            });
        }
    }
    entries
        .into_iter()
        .filter_map(|model| {
            let capabilities = &model["capabilities"];
            Some(ModelInfo {
                id: model["id"].as_str()?.to_owned(),
                features: ClaudeFeatures {
                    adaptive_thinking: capabilities["thinking"]["types"]["adaptive"]["supported"] == true,
                    effort: capabilities["effort"]["supported"] == true,
                },
            })
        })
        .collect()
}

#[cfg(test)]
pub mod testing {
    //! A local HTTP server that replays canned responses through the real client.
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;

    pub struct Mock {
        pub url: String,
        /// Each request's path, lowercased headers and body, in order.
        pub requests: Arc<Mutex<Vec<(String, Vec<String>, String)>>>,
    }

    /// Serves one canned (status, content) per request, in order.
    pub fn mock(responses: Vec<(u16, String)>) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        thread::spawn(move || {
            for (status, content) in responses {
                let Ok((mut socket, _)) = listener.accept() else { return };
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                let path = first.split_whitespace().nth(1).unwrap_or("").to_owned();
                let mut headers = Vec::new();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let line = line.trim().to_lowercase();
                    if line.is_empty() {
                        break;
                    }
                    if let Some(value) = line.strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                    headers.push(line);
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                seen.lock().unwrap().push((path, headers, String::from_utf8(body).unwrap()));
                let head = format!("HTTP/1.1 {status} X\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", content.len());
                socket.write_all(head.as_bytes()).unwrap();
                socket.write_all(content.as_bytes()).unwrap();
            }
        });
        Mock { url, requests }
    }

    /// A body of server-sent events, one per JSON line.
    pub fn sse(events: &[serde_json::Value]) -> String {
        events.iter().map(|event| format!("data: {event}\n\n")).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{mock, sse};
    use super::*;

    fn request(wire: Wire, base_url: &str, model: &str) -> Request {
        Request {
            wire,
            base_url: base_url.to_owned(),
            api_key: "test-key".to_owned(),
            model: model.to_owned(),
            system: "be brief".to_owned(),
            tool_names: vec!["read_file", "write_file"],
            history: vec![user_message("hi")],
            claude: ClaudeFeatures { adaptive_thinking: true, effort: true },
            responses_stored: Arc::new(AtomicBool::new(false)),
        }
    }

    fn run(request: &Request) -> (Result<Option<Turn>, String>, Vec<Delta>) {
        let mut deltas = Vec::new();
        let result = stream(request, &AtomicBool::new(false), |delta| deltas.push(delta));
        (result, deltas)
    }

    fn claude_events(blocks: Vec<Value>, stop: &str) -> Vec<Value> {
        let mut events = vec![json!({"type": "message_start", "message": {"usage": {"input_tokens": 10, "cache_read_input_tokens": 5, "output_tokens": 1}}})];
        events.extend(blocks);
        events.push(json!({"type": "message_delta", "delta": {"stop_reason": stop}, "usage": {"output_tokens": 7}}));
        events.push(json!({"type": "message_stop"}));
        events
    }

    #[test]
    fn claude_request_has_the_required_shape() {
        let req = request(Wire::Messages, "http://x", "claude-opus-5-5");
        let (path, body) = build_body(&req);
        assert_eq!(path, "/messages");
        assert_eq!(body["max_tokens"], 64000);
        assert_eq!(body["stream"], true);
        assert_eq!(body["thinking"], json!({"type": "adaptive", "display": "summarized"}));
        assert_eq!(body["output_config"]["effort"], "high");
        assert_eq!(body["cache_control"]["type"], "ephemeral");
        assert_eq!(body["fallbacks"], "default");
        assert_eq!(body["tools"][0]["eager_input_streaming"], true);
        assert!(body["tools"][0]["input_schema"].is_object());
        for banned in ["temperature", "top_p", "top_k", "tool_choice"] {
            assert!(body.get(banned).is_none(), "{banned} returns a 400");
        }

        let mut older = request(Wire::Messages, "http://x", "claude-old");
        older.claude = ClaudeFeatures { adaptive_thinking: false, effort: false };
        let (_, body) = build_body(&older);
        assert!(body.get("thinking").is_none() && body.get("output_config").is_none() && body.get("fallbacks").is_none());
    }

    #[test]
    fn claude_stream_keeps_thinking_verbatim_and_extracts_tools() {
        let events = claude_events(
            vec![
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "hmm"}}),
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "SIG"}}),
                json!({"type": "content_block_stop", "index": 0}),
                json!({"type": "ping"}),
                json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
                json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Hel"}}),
                json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "lo"}}),
                json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "t1", "name": "read_file", "input": {}}}),
                json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"path\":"}}),
                json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "\"a.rs\"}"}}),
            ],
            "tool_use",
        );
        let server = mock(vec![(200, sse(&events))]);
        let (result, deltas) = run(&request(Wire::Messages, &server.url, "claude-opus-5-5"));
        let turn = result.unwrap().unwrap();
        assert_eq!(turn.stop, Stop::ToolUse);
        assert_eq!((turn.input_tokens, turn.output_tokens), (15, 7));
        assert_eq!(
            deltas,
            [Delta::Thinking("hmm".into()), Delta::Text("Hel".into()), Delta::Text("lo".into()), Delta::ToolStart("read_file".into())]
        );
        assert_eq!(turn.calls[0].input.as_ref().unwrap(), &json!({"path": "a.rs"}));
        let content = &turn.items[0]["content"];
        assert_eq!(turn.items[0]["role"], "assistant");
        assert_eq!(content[0], json!({"type": "thinking", "thinking": "hmm", "signature": "SIG"}));
        assert_eq!(content[1]["text"], "Hello");
        assert_eq!(content[2]["input"], json!({"path": "a.rs"}));

        let requests = server.requests.lock().unwrap();
        let (path, headers, _) = &requests[0];
        assert_eq!(path, "/v1/messages");
        assert!(headers.contains(&"x-api-key: test-key".to_owned()));
        assert!(headers.contains(&"anthropic-version: 2026-10-21".to_owned()));
        assert!(headers.contains(&"anthropic-beta: server-side-fallback-2026-07-01".to_owned()));
    }

    #[test]
    fn invalid_tool_json_comes_back_as_raw_text_with_a_valid_history_block() {
        let events = claude_events(
            vec![
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "t1", "name": "write_file", "input": {}}}),
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"path\": \"a\", oops"}}),
            ],
            "tool_use",
        );
        let server = mock(vec![(200, sse(&events))]);
        let turn = run(&request(Wire::Messages, &server.url, "m")).0.unwrap().unwrap();
        assert_eq!(turn.calls[0].input.as_ref().unwrap_err(), "{\"path\": \"a\", oops");
        assert_eq!(turn.items[0]["content"][0]["input"], json!({}), "history must still hold an object");
    }

    #[test]
    fn replay_after_a_mid_output_fallback_keeps_only_text_before_the_boundary() {
        let blocks = vec![
            json!({"type": "thinking", "thinking": "a", "signature": "s"}),
            json!({"type": "redacted_thinking", "data": "x"}),
            json!({"type": "text", "text": "partial"}),
            json!({"type": "tool_use", "id": "gone", "name": "read_file", "input": {}}),
            json!({"type": "fallback", "from": {"model": "a"}, "to": {"model": "b"}}),
            json!({"type": "thinking", "thinking": "b", "signature": "t"}),
            json!({"type": "text", "text": "rest"}),
            json!({"type": "tool_use", "id": "kept", "name": "read_file", "input": {}}),
        ];
        let kept: Vec<String> = replayable_blocks(blocks).iter().map(|b| text_of(&b["type"])).collect();
        assert_eq!(kept, ["text", "fallback", "thinking", "text", "tool_use"]);

        // Without a fallback, everything goes back unchanged except empty text and gaps.
        let plain = vec![json!({"type": "thinking", "thinking": "a"}), Value::Null, json!({"type": "text", "text": ""}), json!({"type": "text", "text": "x"})];
        assert_eq!(replayable_blocks(plain).len(), 2);
    }

    #[test]
    fn refusals_and_truncation_surface_as_stop_reasons() {
        let refusal = vec![
            json!({"type": "message_delta", "delta": {"stop_reason": "refusal", "stop_details": {"category": "cyber"}}}),
            json!({"type": "message_stop"}),
        ];
        let server = mock(vec![(200, sse(&refusal))]);
        let turn = run(&request(Wire::Messages, &server.url, "m")).0.unwrap().unwrap();
        assert_eq!(turn.stop, Stop::Refusal(Some("cyber".into())));
        assert!(turn.items.is_empty());

        let cut_off = vec![json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})];
        let server = mock(vec![(200, sse(&cut_off))]);
        let error = run(&request(Wire::Messages, &server.url, "m")).0.unwrap_err();
        assert!(error.contains("closed before"), "a dropped stream must not look finished: {error}");

        let mid_error = vec![json!({"type": "error", "error": {"message": "overloaded"}})];
        let server = mock(vec![(200, sse(&mid_error))]);
        assert_eq!(run(&request(Wire::Messages, &server.url, "m")).0.unwrap_err(), "overloaded");
    }

    #[test]
    fn responses_stream_returns_output_items_verbatim() {
        let reasoning = json!({"type": "reasoning", "id": "rs_1", "encrypted_content": "abc", "summary": []});
        let call = json!({"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "read_file", "arguments": "{\"path\":\"a\"}"});
        let events = vec![
            json!({"type": "response.output_item.added", "item": {"type": "reasoning"}}),
            json!({"type": "response.reasoning_summary_text.delta", "delta": "why"}),
            json!({"type": "response.output_text.delta", "delta": "Hi"}),
            json!({"type": "response.output_item.added", "item": {"type": "function_call", "name": "read_file"}}),
            json!({"type": "response.function_call_arguments.delta", "delta": "{\"path\""}),
            json!({"type": "response.output_item.done", "item": call}),
            json!({"type": "response.completed", "response": {"status": "completed", "output": [reasoning, call], "usage": {"input_tokens": 3, "output_tokens": 4}}}),
        ];
        let server = mock(vec![(200, format!("{}data: [DONE]\n\n", sse(&events)))]);
        let (result, deltas) = run(&request(Wire::Responses, &server.url, "gpt"));
        let turn = result.unwrap().unwrap();
        assert_eq!(turn.stop, Stop::ToolUse);
        assert_eq!(turn.items, [reasoning, call]);
        assert_eq!(turn.calls[0].id, "call_1");
        assert_eq!((turn.input_tokens, turn.output_tokens), (3, 4));
        assert_eq!(deltas, [Delta::Thinking("why".into()), Delta::Text("Hi".into()), Delta::ToolStart("read_file".into())]);

        let requests = server.requests.lock().unwrap();
        let (path, headers, body) = &requests[0];
        assert_eq!(path, "/v1/responses");
        assert!(headers.contains(&"authorization: bearer test-key".to_owned()));
        let body: Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["instructions"], "be brief");
        assert_eq!(body["store"], false);
        assert_eq!(body["input"][0]["content"], "hi");
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert_eq!(body["tools"][0]["type"], "function");
    }

    #[test]
    fn responses_falls_back_to_stored_mode_when_store_false_is_refused() {
        let done = sse(&[json!({"type": "response.completed", "response": {"status": "completed", "output": []}})]);
        let refused = json!({"error": {"message": "Encrypted content is not supported with this model."}}).to_string();
        let server = mock(vec![(400, refused), (200, done.clone()), (200, done)]);
        let req = request(Wire::Responses, &server.url, "gpt");
        assert_eq!(run(&req).0.unwrap().unwrap().stop, Stop::EndTurn);
        assert!(req.responses_stored.load(Ordering::Relaxed));
        run(&req).0.unwrap();
        let requests = server.requests.lock().unwrap();
        let bodies: Vec<Value> = requests.iter().map(|r| serde_json::from_str(&r.2).unwrap()).collect();
        assert_eq!(bodies.len(), 3, "the refusal costs one extra request, once");
        assert_eq!(bodies[0]["store"], false);
        assert!(bodies[1].get("store").is_none() && bodies[1].get("include").is_none());
        assert!(bodies[2].get("store").is_none());
    }

    #[test]
    fn responses_incomplete_and_failed_are_not_treated_as_success() {
        let incomplete = sse(&[json!({"type": "response.incomplete", "response": {"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}, "output": [
            {"type": "function_call", "call_id": "c", "name": "write_file", "arguments": "{\"path\":\"a\",\"con"}
        ]}})]);
        let server = mock(vec![(200, incomplete)]);
        let turn = run(&request(Wire::Responses, &server.url, "gpt")).0.unwrap().unwrap();
        assert_eq!(turn.stop, Stop::MaxTokens);
        assert!(turn.calls[0].input.is_err());

        let failed = sse(&[json!({"type": "response.failed", "response": {"error": {"message": "boom"}}})]);
        let server = mock(vec![(200, failed)]);
        assert_eq!(run(&request(Wire::Responses, &server.url, "gpt")).0.unwrap_err(), "boom");
    }

    #[test]
    fn chat_completions_assembles_tool_calls_and_ignores_sse_comments() {
        let chunk = |delta: Value, finish: Value| json!({"choices": [{"delta": delta, "finish_reason": finish}]});
        let body = format!(
            ": OPENROUTER PROCESSING\n\n{}: OPENROUTER PROCESSING\n\ndata: [DONE]\n\n",
            sse(&[
                chunk(json!({"content": "Hi", "reasoning": "why"}), Value::Null),
                chunk(json!({"tool_calls": [{"index": 0, "id": "c1", "function": {"name": "read_file", "arguments": ""}}]}), Value::Null),
                chunk(json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"path\":"}}]}), Value::Null),
                chunk(json!({"tool_calls": [{"index": 0, "function": {"arguments": "\"a\"}"}}, {"index": 1, "id": "c2", "function": {"name": "read_file", "arguments": "{\"path\":\"b\"}"}}]}), json!("tool_calls")),
                json!({"choices": [], "usage": {"prompt_tokens": 8, "completion_tokens": 9}}),
            ])
        );
        let server = mock(vec![(200, body)]);
        let (result, deltas) = run(&request(Wire::ChatCompletions, &server.url, "x/y"));
        let turn = result.unwrap().unwrap();
        assert_eq!(turn.stop, Stop::ToolUse);
        assert_eq!(turn.calls.len(), 2);
        assert_eq!(turn.calls[0].input.as_ref().unwrap(), &json!({"path": "a"}));
        assert_eq!(turn.calls[1].id, "c2");
        assert_eq!((turn.input_tokens, turn.output_tokens), (8, 9));
        assert_eq!(turn.items[0]["content"], "Hi");
        assert_eq!(turn.items[0]["tool_calls"][0]["function"]["arguments"], "{\"path\":\"a\"}");
        assert!(deltas.contains(&Delta::Thinking("why".into())));

        let requests = server.requests.lock().unwrap();
        let body: Value = serde_json::from_str(&requests[0].2).unwrap();
        assert_eq!(requests[0].0, "/v1/chat/completions");
        assert_eq!(body["messages"][0], json!({"role": "system", "content": "be brief"}));
        assert_eq!(body["messages"][1]["content"], "hi");
        assert_eq!(body["tools"][0]["function"]["name"], "read_file");
    }

    #[test]
    fn chat_completions_reports_mid_stream_errors_and_length_stops() {
        let server = mock(vec![(200, sse(&[json!({"error": {"code": 502, "message": "upstream died"}, "choices": [{"delta": {}, "finish_reason": "error"}]})]))]);
        assert_eq!(run(&request(Wire::ChatCompletions, &server.url, "x")).0.unwrap_err(), "upstream died");
        let length = sse(&[json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c", "function": {"name": "write_file", "arguments": "{\"pa"}}]}, "finish_reason": "length"}]})]);
        let server = mock(vec![(200, length)]);
        let turn = run(&request(Wire::ChatCompletions, &server.url, "x")).0.unwrap().unwrap();
        assert_eq!(turn.stop, Stop::MaxTokens);
    }

    #[test]
    fn rate_limits_retry_and_client_errors_do_not() {
        let done = sse(&[json!({"type": "message_stop"})]);
        let limited = (429, json!({"error": {"message": "slow down"}}).to_string());
        let server = mock(vec![limited.clone(), (200, done)]);
        // The mock can't send retry-after, so this retry waits the default 2 seconds.
        let started = std::time::Instant::now();
        assert!(run(&request(Wire::Messages, &server.url, "m")).0.is_ok());
        assert!(started.elapsed() >= Duration::from_secs(2));
        assert_eq!(server.requests.lock().unwrap().len(), 2);

        let bad_key = json!({"type": "error", "error": {"type": "authentication_error", "message": "invalid x-api-key"}}).to_string();
        let server = mock(vec![(401, bad_key)]);
        assert_eq!(run(&request(Wire::Messages, &server.url, "m")).0.unwrap_err(), "HTTP 401: invalid x-api-key");
        assert_eq!(server.requests.lock().unwrap().len(), 1);

        assert_eq!(error_text(502, "<html>bad gateway</html>"), "HTTP 502: <html>bad gateway</html>");
    }

    #[test]
    fn cancelling_stops_reading_the_stream_and_retry_waits() {
        let cancel = AtomicBool::new(true);
        let server = mock(vec![(200, sse(&[json!({"type": "message_stop"})]))]);
        let req = request(Wire::Messages, &server.url, "m");
        assert!(stream(&req, &cancel, |_| {}).unwrap().is_none());
        assert!(!wait(Duration::from_secs(60), &cancel), "a cancelled wait returns at once");

        let mut events = 0;
        let reader = std::io::Cursor::new("data: {}\n\ndata: {}\n\n");
        let flag = AtomicBool::new(false);
        let finished = read_sse(reader, &flag, |_| {
            events += 1;
            flag.store(true, Ordering::Relaxed);
            Ok(())
        });
        assert_eq!(finished, Ok(false));
        assert_eq!(events, 1);
    }

    #[test]
    fn sse_parser_handles_multiline_data_and_crlf() {
        let mut seen = Vec::new();
        let input = "event: x\r\ndata: a\r\ndata: b\r\n\r\ndata:c\n\n: comment\n\nretry: 5\n\ndata: end";
        read_sse(std::io::Cursor::new(input), &AtomicBool::new(false), |data| {
            seen.push(data.to_owned());
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, ["a\nb", "c", "end"]);
    }

    #[test]
    fn tool_results_use_each_formats_shape_and_stay_together() {
        let results = vec![
            ("a".to_owned(), ToolOutput { text: "fine".into(), is_error: false }),
            ("b".to_owned(), ToolOutput { text: "bad".into(), is_error: true }),
        ];
        let claude = tool_results(Wire::Messages, &results);
        assert_eq!(claude.len(), 1, "Claude needs one user message with every result");
        assert_eq!(claude[0]["content"][0], json!({"type": "tool_result", "tool_use_id": "a", "content": "fine"}));
        assert_eq!(claude[0]["content"][1]["is_error"], true);

        let responses = tool_results(Wire::Responses, &results);
        assert_eq!(responses[1], json!({"type": "function_call_output", "call_id": "b", "output": "ERROR: bad"}));

        let chat = tool_results(Wire::ChatCompletions, &results);
        assert_eq!(chat[0], json!({"role": "tool", "tool_call_id": "a", "content": "fine"}));
    }

    #[test]
    fn model_lists_are_filtered_for_chat_use() {
        let claude = json!({"data": [
            {"id": "new", "capabilities": {"thinking": {"types": {"adaptive": {"supported": true}}}, "effort": {"supported": true}}},
            {"id": "old", "capabilities": {"thinking": {"types": {"adaptive": {"supported": false}}}, "effort": {"supported": false}}},
            {"id": "bare"}
        ]});
        let models = models_from(Provider::Claude, &claude);
        assert_eq!(models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["new", "old", "bare"]);
        assert_eq!(models[0].features, ClaudeFeatures { adaptive_thinking: true, effort: true });
        assert_eq!(models[1].features, ClaudeFeatures { adaptive_thinking: false, effort: false });
        assert_eq!(models[2].features, ClaudeFeatures { adaptive_thinking: false, effort: false });

        let openai = json!({"data": [
            {"id": "gpt-old", "created": 1},
            {"id": "text-embedding-3", "created": 9},
            {"id": "gpt-new", "created": 5},
            {"id": "whisper-1", "created": 7}
        ]});
        let ids: Vec<String> = models_from(Provider::OpenAi, &openai).into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["gpt-new", "gpt-old"]);

        let router = json!({"data": [
            {"id": "a/tools", "supported_parameters": ["tools", "temperature"]},
            {"id": "b/notools", "supported_parameters": ["temperature"]},
            {"id": "c/unknown"}
        ]});
        let ids: Vec<String> = models_from(Provider::OpenRouter, &router).into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["a/tools", "c/unknown"]);
    }

    #[test]
    fn list_models_sends_auth_and_reports_errors() {
        let body = json!({"data": [{"id": "m1"}]}).to_string();
        let server = mock(vec![(200, body)]);
        let models = list_models(Provider::Claude, &server.url, "k").unwrap();
        assert_eq!(models[0].id, "m1");
        {
            let requests = server.requests.lock().unwrap();
            assert_eq!(requests[0].0, "/v1/models?limit=1000");
            assert!(requests[0].1.contains(&"x-api-key: k".to_owned()));
        }
        let server = mock(vec![(401, json!({"error": {"message": "nope"}}).to_string())]);
        assert_eq!(list_models(Provider::Grok, &server.url, "k").unwrap_err(), "HTTP 401: nope");
    }

    /// Real TLS and JSON against a live public endpoint. Needs no credentials.
    #[test]
    #[ignore = "uses the network"]
    fn live_openrouter_model_list_without_a_key() {
        let models = list_models(Provider::OpenRouter, &Provider::OpenRouter.base_url(), "").unwrap();
        assert!(models.len() > 10, "got {} models", models.len());
    }

    /// Real API error path, with a key that can't work.
    #[test]
    #[ignore = "uses the network"]
    fn live_claude_rejects_a_bad_key_with_a_readable_error() {
        let error = list_models(Provider::Claude, &Provider::Claude.base_url(), "not-a-real-key").unwrap_err();
        assert!(error.starts_with("HTTP 401"), "{error}");
    }

    fn live_turn(provider: Provider, model: String) {
        let key = provider.api_key().unwrap_or_else(|| panic!("set {}", provider.key_env()));
        let request = Request {
            wire: provider.wire(),
            base_url: provider.base_url(),
            api_key: key,
            model,
            system: "You are a test. Follow the user's instruction exactly.".to_owned(),
            tool_names: vec!["read_file"],
            history: vec![user_message("Call read_file with path README.md and say nothing else.")],
            claude: ClaudeFeatures { adaptive_thinking: true, effort: false },
            responses_stored: Arc::new(AtomicBool::new(false)),
        };
        let (result, _) = run(&request);
        let turn = result.unwrap().unwrap();
        assert_eq!(turn.stop, Stop::ToolUse, "{turn:?}");
        assert_eq!(turn.calls[0].input.as_ref().unwrap()["path"], "README.md");

        // Send the result back: this is what proves the history format is accepted.
        let mut history = request.history.clone();
        history.extend(turn.items);
        let output = ToolOutput { text: "1\t# hello".to_owned(), is_error: false };
        history.extend(tool_results(provider.wire(), &[(turn.calls[0].id.clone(), output)]));
        let followup = Request { history, ..request };
        let (result, deltas) = run(&followup);
        result.unwrap().unwrap();
        assert!(deltas.iter().any(|delta| matches!(delta, Delta::Text(_))), "{deltas:?}");
    }

    #[test]
    #[ignore = "needs ANTHROPIC_API_KEY and spends money"]
    fn live_claude_tool_round_trip() {
        live_turn(Provider::Claude, std::env::var("KALIPSO_TEST_MODEL").unwrap_or(DEFAULT_CLAUDE_MODEL.to_owned()));
    }

    #[test]
    #[ignore = "needs OPENAI_API_KEY and KALIPSO_TEST_MODEL, and spends money"]
    fn live_openai_tool_round_trip() {
        live_turn(Provider::OpenAi, std::env::var("KALIPSO_TEST_MODEL").expect("set KALIPSO_TEST_MODEL"));
    }

    #[test]
    #[ignore = "needs XAI_API_KEY and KALIPSO_TEST_MODEL, and spends money"]
    fn live_grok_tool_round_trip() {
        live_turn(Provider::Grok, std::env::var("KALIPSO_TEST_MODEL").expect("set KALIPSO_TEST_MODEL"));
    }

    #[test]
    #[ignore = "needs OPENROUTER_API_KEY and KALIPSO_TEST_MODEL, and spends money"]
    fn live_openrouter_tool_round_trip() {
        live_turn(Provider::OpenRouter, std::env::var("KALIPSO_TEST_MODEL").expect("set KALIPSO_TEST_MODEL"));
    }
}
