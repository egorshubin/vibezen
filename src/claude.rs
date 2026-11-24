//! Claude Code agent sessions — one long-lived `claude` process per project,
//! driven over the stream-json protocol (the same protocol the Agent SDK uses).
//! Permission prompts come back to the IDE as `can_use_tool` control requests.

use crate::util::hidden_cmd;
use base64::Engine;
use eframe::egui;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::Instant;

pub const MODELS: &[(&str, &str)] = &[
    ("default", "Default"),
    ("fable", "Fable"),
    ("opus", "Opus"),
    ("sonnet", "Sonnet"),
    ("haiku", "Haiku"),
];

pub const MODES: &[(&str, &str, &str)] = &[
    ("manual", "Ask", "Ask before edits and commands"),
    ("acceptEdits", "Accept edits", "Edit files freely, ask before commands"),
    ("auto", "Auto", "Claude decides what is safe; asks when unsure"),
    ("plan", "Plan", "Read-only planning, then propose a plan"),
    ("bypassPermissions", "Bypass", "Never ask (dangerous)"),
];

/// Built-in slash commands that work in a `-p` stream-json session.
const BUILTIN_COMMANDS: &[(&str, &str)] = &[
    ("compact", "Summarize the conversation to free up context"),
    ("context", "Show what fills the context window"),
    ("cost", "Show token usage and cost of this session"),
    ("init", "Create a CLAUDE.md with notes about this codebase"),
    ("review", "Review a pull request"),
    ("security-review", "Security review of the pending changes"),
    ("pr-comments", "Fetch the comments of a GitHub pull request"),
    ("release-notes", "Show the Claude Code release notes"),
];

/// Command names from the latest `init` message of any session (it also lists plugin commands);
/// kept on disk so they are offered before the first message of the next run.
static REPORTED_COMMANDS: std::sync::Mutex<Option<Vec<String>>> = std::sync::Mutex::new(None);

fn reported_commands_file() -> PathBuf {
    std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("VibeZen").join("slash_commands.json")
}

fn reported_commands() -> Vec<String> {
    let Ok(mut r) = REPORTED_COMMANDS.lock() else { return vec![] };
    r.get_or_insert_with(|| {
        std::fs::read_to_string(reported_commands_file()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    })
    .clone()
}

fn set_reported_commands(names: Vec<String>) {
    if let Ok(mut r) = REPORTED_COMMANDS.lock() {
        if r.as_ref() != Some(&names) {
            let p = reported_commands_file();
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(p, serde_json::to_string(&names).unwrap_or_default());
            *r = Some(names);
        }
    }
}

pub struct SlashCommand {
    pub name: String,
    pub desc: String,
}

/// `description:` from a markdown file's front matter, else its first line of text.
fn md_description(path: &Path) -> String {
    let s = std::fs::read_to_string(path).unwrap_or_default();
    let mut lines = s.lines();
    if s.starts_with("---") {
        lines.next();
        for l in lines.by_ref() {
            if l.trim() == "---" {
                break;
            }
            if let Some(d) = l.strip_prefix("description:") {
                return d.trim().trim_matches(['"', '\'']).to_owned();
            }
        }
    }
    lines.map(|l| l.trim().trim_start_matches('#').trim()).find(|l| !l.is_empty()).unwrap_or("").to_owned()
}

/// `<dir>/commands/**/*.md` (subfolders become `folder:name`) and `<dir>/skills/*/SKILL.md`.
fn scan_commands(dir: &Path, out: &mut Vec<SlashCommand>) {
    fn walk(d: &Path, prefix: &str, out: &mut Vec<SlashCommand>) {
        let Ok(rd) = std::fs::read_dir(d) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            if p.is_dir() {
                walk(&p, &format!("{prefix}{stem}:"), out);
            } else if p.extension().is_some_and(|x| x == "md") {
                out.push(SlashCommand { name: format!("{prefix}{stem}"), desc: md_description(&p) });
            }
        }
    }
    walk(&dir.join("commands"), "", out);
    if let Ok(rd) = std::fs::read_dir(dir.join("skills")) {
        for e in rd.flatten() {
            let skill = e.path().join("SKILL.md");
            if skill.is_file() {
                out.push(SlashCommand { name: e.file_name().to_string_lossy().into_owned(), desc: md_description(&skill) });
            }
        }
    }
}

/// Slash commands offered in the input box: built-ins, the project's and the user's own
/// commands and skills, and whatever else the CLI reported (plugins).
pub fn slash_commands(cwd: &Path) -> Vec<SlashCommand> {
    let mut out: Vec<SlashCommand> =
        BUILTIN_COMMANDS.iter().map(|(n, d)| SlashCommand { name: n.to_string(), desc: d.to_string() }).collect();
    scan_commands(&cwd.join(".claude"), &mut out);
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        scan_commands(&PathBuf::from(home).join(".claude"), &mut out);
    }
    for n in reported_commands() {
        out.push(SlashCommand { name: n, desc: String::new() });
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|c| !c.name.is_empty() && !c.name.starts_with("__") && seen.insert(c.name.clone()));
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub enum CEvent {
    Json(Value),
    Stderr(String),
    Exited(Option<i32>),
}

struct Session {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<CEvent>,
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

/// A file attached to the next message (see `attach.rs`).
pub struct Attachment {
    pub name: String,
    /// content block sent as it is
    pub block: Value,
    /// images only; documents show as a named chip
    pub thumb: Option<egui::TextureHandle>,
    /// full-size texture, decoded from `block` the first time the preview opens
    pub full: Option<egui::TextureHandle>,
}

impl Attachment {
    /// Full-size texture of an image attachment (decoded on first use); `None` for documents.
    pub fn full_texture(&mut self, ctx: &egui::Context) -> Option<egui::TextureHandle> {
        self.thumb.as_ref()?;
        if self.full.is_none() {
            let data = self.block.pointer("/source/data")?.as_str()?;
            let bytes = base64::engine::general_purpose::STANDARD.decode(data).ok()?;
            let rgba = image::load_from_memory(&bytes).ok()?.to_rgba8();
            let ci = egui::ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw());
            self.full = Some(ctx.load_texture(format!("att-full-{}", self.name), ci, egui::TextureOptions::LINEAR));
        }
        self.full.clone()
    }
}

#[derive(PartialEq, Clone, Copy)]
pub enum PermState {
    Pending,
    Allowed,
    Denied,
}

pub enum Item {
    User {
        text: String,
        thumbs: Vec<egui::TextureHandle>,
        /// names of attached documents
        files: Vec<String>,
    },
    Assistant(String),
    Thinking(String),
    Tool {
        id: String,
        name: String,
        input: Value,
        result: Option<(String, bool)>,
        sub: bool,
    },
    Permission {
        request_id: String,
        tool: String,
        input: Value,
        suggestions: Value,
        state: PermState,
    },
    Question {
        request_id: String,
        input: Value,
        answers: HashMap<String, String>,
        state: PermState,
    },
    Result {
        text: String,
        is_error: bool,
    },
    System(String),
    Error(String),
}

#[derive(Clone)]
pub struct Todo {
    pub content: String,
    pub status: String,
}

/// Plan limit reported by Claude Code (`rate_limit_event`); account-wide.
#[derive(Clone)]
pub struct RateLimit {
    /// five_hour, seven_day, seven_day_opus, ...
    pub kind: String,
    /// allowed, allowed_warning, rejected
    pub status: String,
    /// unix secs
    pub resets_at: Option<u64>,
    /// 0..1
    pub utilization: Option<f64>,
    pub at: Instant,
}

/// Usage of the running Claude process (resets with a new conversation).
#[derive(Clone, Copy, Default)]
pub struct Usage {
    pub cost_usd: f64,
    pub turns: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
}

pub enum Signal {
    Done { summary: String, is_error: bool },
    NeedsApproval { tool: String },
    FilesTouched,
}

pub struct Chat {
    /// unique per conversation tab: keys its widget state (several tabs share one cwd)
    pub uid: u64,
    pub cwd: PathBuf,
    pub items: Vec<Item>,
    pub input: String,
    pub attachments: Vec<Attachment>,
    /// index into `attachments` of the image shown in the preview modal
    pub preview: Option<usize>,
    session: Option<Session>,
    pub session_id: Option<String>,
    pub resume_id: Option<String>,
    pub busy: bool,
    pub busy_since: Option<Instant>,
    pub live_text: String,
    pub model: String,
    pub mode: String,
    pub todos: Vec<Todo>,
    pub stats: Option<String>,
    pub md_cache: egui_commonmark::CommonMarkCache,
    tool_index: HashMap<String, usize>,
    req_counter: u64,
    ctx: egui::Context,
    pub unseen: bool,
    pub stderr_tail: Vec<String>,
    /// past conversations (cached for the history menu)
    pub history: Vec<SessionInfo>,
    pub history_at: Option<Instant>,
    /// put the cursor into the input box on the next frame
    pub focus_input: bool,
    /// set when a fresh conversation starts: the app re-applies the saved default model and mode
    pub take_defaults: bool,
    pub usage: Usage,
    pub rate_limits: HashMap<String, RateLimit>,
    /// restored tab: the transcript of `resume_id` is read when the tab is first shown
    lazy: bool,
    /// title of a restored tab, known before its transcript is loaded
    title_hint: Option<String>,
    /// slash commands for the input box (cached, see `commands()`)
    commands: Vec<SlashCommand>,
    commands_at: Option<Instant>,
    /// highlighted entry of the slash command popup
    pub cmd_sel: usize,
    /// the popup was closed with Esc for this input text
    pub cmd_hidden_for: Option<String>,
}

fn find_claude() -> (String, Vec<String>) {
    // Prefer a real executable on PATH, fall back to `cmd /C claude` (npm shim).
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            let exe = dir.join("claude.exe");
            if exe.is_file() {
                return (exe.to_string_lossy().into_owned(), vec![]);
            }
        }
    }
    if let Some(home) = std::env::var_os("USERPROFILE") {
        let exe = PathBuf::from(home).join(".local").join("bin").join("claude.exe");
        if exe.is_file() {
            return (exe.to_string_lossy().into_owned(), vec![]);
        }
    }
    ("cmd".into(), vec!["/C".into(), "claude".into()])
}

/// Run a single non-interactive prompt (used for commit messages).
pub fn one_shot(cwd: &Path, prompt: &str, model: &str) -> Result<String, String> {
    let (prog, pre) = find_claude();
    let mut child = hidden_cmd(&prog)
        .args(&pre)
        .args(["-p", "--model", model, "--permission-prompts", "none"])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start claude: {e}"))?;
    if let Some(mut si) = child.stdin.take() {
        si.write_all(prompt.as_bytes()).map_err(|e| e.to_string())?;
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
    }
}

impl Chat {
    pub fn new(cwd: PathBuf, ctx: egui::Context, model: String, mode: String, resume: Option<String>) -> Self {
        static NEXT_UID: AtomicU64 = AtomicU64::new(1);
        Self {
            uid: NEXT_UID.fetch_add(1, Ordering::Relaxed),
            cwd,
            items: vec![],
            input: String::new(),
            attachments: vec![],
            preview: None,
            session: None,
            session_id: None,
            resume_id: resume,
            busy: false,
            busy_since: None,
            live_text: String::new(),
            model,
            mode,
            todos: vec![],
            stats: None,
            md_cache: Default::default(),
            tool_index: HashMap::new(),
            req_counter: 0,
            ctx,
            unseen: false,
            stderr_tail: vec![],
            history: vec![],
            history_at: None,
            focus_input: false,
            take_defaults: false,
            usage: Usage::default(),
            rate_limits: HashMap::new(),
            lazy: false,
            title_hint: None,
            commands: vec![],
            commands_at: None,
            cmd_sel: 0,
            cmd_hidden_for: None,
        }
    }

    /// Slash commands, re-read from disk at most every few seconds.
    pub fn commands(&mut self) -> &[SlashCommand] {
        if self.commands_at.is_none_or(|t| t.elapsed().as_secs() > 5) {
            self.commands = slash_commands(&self.cwd);
            self.commands_at = Some(Instant::now());
        }
        &self.commands
    }

    pub fn is_running(&self) -> bool {
        self.session.is_some()
    }

    pub fn pending_approvals(&self) -> usize {
        self.items
            .iter()
            .filter(|i| {
                matches!(i, Item::Permission { state: PermState::Pending, .. } | Item::Question { state: PermState::Pending, .. })
            })
            .count()
    }

    fn start(&mut self, resume: Option<String>) -> Result<(), String> {
        let (prog, pre) = find_claude();
        let mut cmd = hidden_cmd(&prog);
        cmd.args(&pre).args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--permission-prompt-tool",
            "stdio",
            "--permission-mode",
            &self.mode,
            // Claude in Chrome: off in `-p` sessions unless asked for
            "--chrome",
        ]);
        if self.model != "default" && !self.model.is_empty() {
            cmd.args(["--model", &self.model]);
        }
        if let Some(id) = &resume {
            cmd.args(["--resume", id]);
        }
        cmd.current_dir(&self.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| {
            format!("Cannot start Claude Code ({e}). Is `claude` installed and on PATH?")
        })?;
        crate::util::kill_with_ide(&child);
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let stderr = child.stderr.take().ok_or("no stderr")?;
        let (tx, rx) = channel();
        {
            let tx = tx.clone();
            let ctx = self.ctx.clone();
            std::thread::spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    if line.trim().is_empty() {
                        continue;
                    }
                    let ev = match serde_json::from_str::<Value>(&line) {
                        Ok(v) => CEvent::Json(v),
                        Err(_) => CEvent::Stderr(line),
                    };
                    if tx.send(ev).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
                let _ = tx.send(CEvent::Exited(None));
                ctx.request_repaint();
            });
        }
        {
            let ctx = self.ctx.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines() {
                    let Ok(line) = line else { break };
                    if tx.send(CEvent::Stderr(line)).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            });
        }
        self.session = Some(Session { child, stdin, rx });
        Ok(())
    }

    fn write(&mut self, v: &Value) -> Result<(), String> {
        let s = self.session.as_mut().ok_or("Claude is not running")?;
        let mut line = serde_json::to_string(v).map_err(|e| e.to_string())?;
        line.push('\n');
        s.stdin.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
        s.stdin.flush().map_err(|e| e.to_string())
    }

    fn control(&mut self, request: Value) {
        self.req_counter += 1;
        let id = format!("vs_{}", self.req_counter);
        let _ = self.write(&json!({"type": "control_request", "request_id": id, "request": request}));
    }

    pub fn send(&mut self) {
        let text = self.input.trim().to_owned();
        if text.is_empty() && self.attachments.is_empty() {
            return;
        }
        if self.session.is_none() {
            let resume = self.resume_id.take();
            if let Err(e) = self.start(resume) {
                self.items.push(Item::Error(e));
                return;
            }
        }
        let atts = std::mem::take(&mut self.attachments);
        let mut content: Vec<Value> = atts.iter().map(|a| a.block.clone()).collect();
        let fallback = if atts.iter().all(|a| a.thumb.is_some()) { "(see attached image)" } else { "(see attached files)" };
        content.push(json!({"type": "text", "text": if text.is_empty() { fallback.to_owned() } else { text.clone() }}));
        let msg = json!({
            "type": "user",
            "message": {"role": "user", "content": content},
            "parent_tool_use_id": null,
            "session_id": self.session_id.clone().unwrap_or_else(|| "default".into()),
        });
        match self.write(&msg) {
            Ok(()) => {
                let (images, docs): (Vec<_>, Vec<_>) = atts.into_iter().partition(|a| a.thumb.is_some());
                self.items.push(Item::User {
                    text,
                    thumbs: images.into_iter().filter_map(|a| a.thumb).collect(),
                    files: docs.into_iter().map(|a| a.name).collect(),
                });
                self.input.clear();
                self.busy = true;
                self.busy_since.get_or_insert_with(Instant::now);
            }
            Err(e) => {
                self.items.push(Item::Error(format!("Failed to send: {e}")));
                self.session = None;
            }
        }
    }

    pub fn interrupt(&mut self) {
        if self.session.is_some() {
            self.control(json!({"subtype": "interrupt"}));
            // deny anything still waiting
            let pending: Vec<String> = self
                .items
                .iter()
                .filter_map(|i| match i {
                    Item::Permission { request_id, state: PermState::Pending, .. } => Some(request_id.clone()),
                    Item::Question { request_id, state: PermState::Pending, .. } => Some(request_id.clone()),
                    _ => None,
                })
                .collect();
            for id in pending {
                self.answer(&id, json!({"behavior": "deny", "message": "Interrupted by user", "interrupt": true}));
                self.mark(&id, PermState::Denied);
            }
        }
    }

    /// A fresh conversation in the same folder; it picks up the saved default model and mode.
    pub fn sibling(&self) -> Chat {
        let mut c = Chat::new(self.cwd.clone(), self.ctx.clone(), self.model.clone(), self.mode.clone(), None);
        c.take_defaults = true;
        c
    }

    /// Nothing typed, sent or restored yet: safe to reuse for another conversation.
    pub fn is_blank(&self) -> bool {
        self.items.is_empty() && !self.busy && self.session.is_none() && self.resume_id.is_none() && self.input.is_empty() && self.attachments.is_empty()
    }

    /// This tab holds the stored conversation `id` (running or waiting to resume).
    pub fn holds_session(&self, id: &str) -> bool {
        self.session_id.as_deref() == Some(id) || (self.session_id.is_none() && self.resume_id.as_deref() == Some(id))
    }

    pub fn input_id(&self) -> egui::Id {
        egui::Id::new(("claude_input", self.uid))
    }

    /// Kill the process and start fresh.
    pub fn new_chat(&mut self) {
        self.session = None;
        self.session_id = None;
        self.resume_id = None;
        self.items.clear();
        self.todos.clear();
        self.tool_index.clear();
        self.busy = false;
        self.busy_since = None;
        self.live_text.clear();
        self.stats = None;
        self.usage = Usage::default();
        self.take_defaults = true;
        self.lazy = false;
        self.title_hint = None;
    }

    pub fn resume_last(&mut self, id: Option<String>) {
        self.new_chat();
        self.resume_id = id.clone();
        self.items.push(Item::System(match id {
            Some(i) => format!("Will resume session {}… on your next message.", &i[..i.len().min(8)]),
            None => "No previous session for this project.".into(),
        }));
    }

    pub fn set_mode(&mut self, mode: &str) {
        self.mode = mode.to_owned();
        if self.session.is_some() {
            self.control(json!({"subtype": "set_permission_mode", "mode": mode}));
        }
    }

    pub fn set_model(&mut self, model: &str) {
        self.model = model.to_owned();
        if self.session.is_some() {
            let m = if model == "default" { Value::Null } else { json!(model) };
            self.control(json!({"subtype": "set_model", "model": m}));
        }
    }

    fn answer(&mut self, request_id: &str, response: Value) {
        let _ = self.write(&json!({
            "type": "control_response",
            "response": {"subtype": "success", "request_id": request_id, "response": response}
        }));
    }

    fn mark(&mut self, request_id: &str, st: PermState) {
        for it in self.items.iter_mut() {
            match it {
                Item::Permission { request_id: r, state, .. } | Item::Question { request_id: r, state, .. }
                    if r == request_id =>
                {
                    *state = st
                }
                _ => {}
            }
        }
    }

    /// Respond to a permission card.
    pub fn respond_permission(&mut self, idx: usize, allow: bool, always: bool, plan_mode_after: Option<&str>) {
        let (rid, input, suggestions, tool) = match self.items.get(idx) {
            Some(Item::Permission { request_id, input, suggestions, tool, state: PermState::Pending }) => {
                (request_id.clone(), input.clone(), suggestions.clone(), tool.clone())
            }
            _ => return,
        };
        if let Some(m) = plan_mode_after {
            self.set_mode(m);
        }
        let resp = if allow {
            let mut r = json!({"behavior": "allow", "updatedInput": input});
            if always && suggestions.is_array() {
                r["updatedPermissions"] = suggestions.clone();
                for s in suggestions.as_array().unwrap() {
                    if s["type"] == "setMode" {
                        if let Some(m) = s["mode"].as_str() {
                            self.mode = m.to_owned();
                        }
                    }
                }
            }
            r
        } else {
            json!({"behavior": "deny", "message": format!("The user denied {tool}. Ask how to proceed.")})
        };
        self.answer(&rid, resp);
        self.mark(&rid, if allow { PermState::Allowed } else { PermState::Denied });
    }

    pub fn respond_question(&mut self, idx: usize, submit: bool) {
        let (rid, mut input, answers) = match self.items.get(idx) {
            Some(Item::Question { request_id, input, answers, state: PermState::Pending }) => {
                (request_id.clone(), input.clone(), answers.clone())
            }
            _ => return,
        };
        if submit {
            input["answers"] = json!(answers);
            self.answer(&rid, json!({"behavior": "allow", "updatedInput": input}));
            self.mark(&rid, PermState::Allowed);
        } else {
            self.answer(&rid, json!({"behavior": "deny", "message": "The user skipped the question."}));
            self.mark(&rid, PermState::Denied);
        }
    }

    pub fn question_answers_mut(&mut self, idx: usize) -> Option<&mut HashMap<String, String>> {
        match self.items.get_mut(idx) {
            Some(Item::Question { answers, .. }) => Some(answers),
            _ => None,
        }
    }

    /// Drain process output. Call every frame (also for background projects).
    pub fn poll(&mut self) -> Vec<Signal> {
        let mut signals = vec![];
        let mut events = vec![];
        if let Some(s) = &self.session {
            while let Ok(ev) = s.rx.try_recv() {
                events.push(ev);
            }
        }
        for ev in events {
            match ev {
                CEvent::Json(v) => self.handle(v, &mut signals),
                CEvent::Stderr(line) => {
                    self.stderr_tail.push(line);
                    if self.stderr_tail.len() > 40 {
                        self.stderr_tail.remove(0);
                    }
                }
                CEvent::Exited(_) => {
                    let was_busy = self.busy;
                    self.session = None;
                    self.busy = false;
                    self.busy_since = None;
                    self.live_text.clear();
                    let tail = self.stderr_tail.join("\n");
                    if was_busy || !tail.trim().is_empty() {
                        self.items.push(Item::Error(format!(
                            "Claude process exited.{}",
                            if tail.trim().is_empty() { String::new() } else { format!("\n{tail}") }
                        )));
                        signals.push(Signal::Done { summary: "Claude process exited".into(), is_error: true });
                    }
                    self.stderr_tail.clear();
                    // allow continuing the same conversation transparently
                    self.resume_id = self.session_id.clone();
                }
            }
        }
        signals
    }

    fn handle(&mut self, v: Value, signals: &mut Vec<Signal>) {
        let ty = v["type"].as_str().unwrap_or("");
        let sub = !v["parent_tool_use_id"].is_null();
        match ty {
            "system" => match v["subtype"].as_str().unwrap_or("") {
                "init" => {
                    let new_id = v["session_id"].as_str().map(|s| s.to_owned());
                    if self.session_id != new_id {
                        self.session_id = new_id;
                    }
                    if let Some(m) = v["permissionMode"].as_str() {
                        // the CLI reports "manual" as "default"
                        self.mode = if m == "default" { "manual".to_owned() } else { m.to_owned() };
                    }
                    if let Some(cmds) = v["slash_commands"].as_array() {
                        let names = cmds.iter().filter_map(|c| c.as_str().or(c["name"].as_str())).map(|s| s.trim_start_matches('/').to_owned());
                        set_reported_commands(names.collect());
                        self.commands_at = None;
                    }
                }
                "compact_boundary" => self.items.push(Item::System("Conversation compacted".into())),
                _ => {}
            },
            "stream_event" => {
                if sub {
                    return;
                }
                let e = &v["event"];
                match e["type"].as_str().unwrap_or("") {
                    "content_block_start" => self.live_text.clear(),
                    "content_block_delta" => {
                        if let Some(t) = e["delta"]["text"].as_str() {
                            self.live_text.push_str(t);
                        }
                    }
                    _ => {}
                }
            }
            "assistant" => {
                let blocks = v["message"]["content"].as_array().cloned().unwrap_or_default();
                for b in blocks {
                    match b["type"].as_str().unwrap_or("") {
                        "text" if !sub => {
                            let t = b["text"].as_str().unwrap_or("").to_owned();
                            if !t.trim().is_empty() {
                                self.items.push(Item::Assistant(t));
                            }
                            self.live_text.clear();
                        }
                        "thinking" if !sub => {
                            let t = b["thinking"].as_str().unwrap_or("").to_owned();
                            if !t.trim().is_empty() {
                                self.items.push(Item::Thinking(t));
                            }
                        }
                        "tool_use" => {
                            let id = b["id"].as_str().unwrap_or("").to_owned();
                            let name = b["name"].as_str().unwrap_or("?").to_owned();
                            if name == "TodoWrite" && !sub {
                                self.todos = b["input"]["todos"]
                                    .as_array()
                                    .map(|a| {
                                        a.iter()
                                            .map(|t| Todo {
                                                content: t["content"].as_str().unwrap_or("").to_owned(),
                                                status: t["status"].as_str().unwrap_or("").to_owned(),
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default();
                            }
                            self.tool_index.insert(id.clone(), self.items.len());
                            self.items.push(Item::Tool {
                                id,
                                name,
                                input: b["input"].clone(),
                                result: None,
                                sub,
                            });
                        }
                        _ => {}
                    }
                }
            }
            "user" => {
                if let Some(blocks) = v["message"]["content"].as_array() {
                    for b in blocks {
                        if b["type"] != "tool_result" {
                            continue;
                        }
                        let id = b["tool_use_id"].as_str().unwrap_or("");
                        let text = match &b["content"] {
                            Value::String(s) => s.clone(),
                            Value::Array(a) => a
                                .iter()
                                .filter_map(|c| c["text"].as_str())
                                .collect::<Vec<_>>()
                                .join("\n"),
                            _ => String::new(),
                        };
                        let is_err = b["is_error"].as_bool().unwrap_or(false);
                        if let Some(&idx) = self.tool_index.get(id) {
                            if let Some(Item::Tool { result, name, .. }) = self.items.get_mut(idx) {
                                *result = Some((text, is_err));
                                if matches!(
                                    name.as_str(),
                                    "Edit" | "Write" | "MultiEdit" | "NotebookEdit" | "Bash" | "PowerShell"
                                ) {
                                    signals.push(Signal::FilesTouched);
                                }
                            }
                        }
                    }
                }
            }
            "control_request" => {
                let rid = v["request_id"].as_str().unwrap_or("").to_owned();
                let req = &v["request"];
                if req["subtype"] == "can_use_tool" {
                    let tool = req["tool_name"].as_str().unwrap_or("?").to_owned();
                    if tool == "AskUserQuestion" {
                        self.items.push(Item::Question {
                            request_id: rid,
                            input: req["input"].clone(),
                            answers: HashMap::new(),
                            state: PermState::Pending,
                        });
                    } else {
                        self.items.push(Item::Permission {
                            request_id: rid,
                            tool: tool.clone(),
                            input: req["input"].clone(),
                            suggestions: req["permission_suggestions"].clone(),
                            state: PermState::Pending,
                        });
                    }
                    signals.push(Signal::NeedsApproval { tool });
                } else {
                    let _ = self.write(&json!({
                        "type": "control_response",
                        "response": {"subtype": "error", "request_id": rid, "error": "Not supported by VibeZen"}
                    }));
                }
            }
            "result" => {
                self.busy = false;
                let secs = self.busy_since.take().map(|t| t.elapsed().as_secs_f32()).unwrap_or(0.0);
                self.live_text.clear();
                let is_error = v["is_error"].as_bool().unwrap_or(false);
                let cost = v["total_cost_usd"].as_f64().unwrap_or(0.0);
                let turns = v["num_turns"].as_u64().unwrap_or(0);
                let text = format!("{:.1}s · {} turn{} · ${:.3}", secs, turns, if turns == 1 { "" } else { "s" }, cost);
                if is_error {
                    let err = v["result"].as_str().unwrap_or(v["subtype"].as_str().unwrap_or("error"));
                    self.items.push(Item::Error(err.to_owned()));
                }
                // total_cost_usd is cumulative for the process; usage is per query
                self.usage.cost_usd = self.usage.cost_usd.max(cost);
                self.usage.turns += turns;
                let u = &v["usage"];
                self.usage.input_tokens += u["input_tokens"].as_u64().unwrap_or(0) + u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                self.usage.cache_read_tokens += u["cache_read_input_tokens"].as_u64().unwrap_or(0);
                self.usage.output_tokens += u["output_tokens"].as_u64().unwrap_or(0);
                self.items.push(Item::Result { text: text.clone(), is_error });
                self.stats = Some(text);
                let summary = v["result"].as_str().unwrap_or("").lines().find(|l| !l.trim().is_empty()).unwrap_or("Task finished").to_owned();
                self.unseen = true;
                signals.push(Signal::Done { summary, is_error });
            }
            "rate_limit_event" => {
                let r = &v["rate_limit_info"];
                let kind = r["rateLimitType"].as_str().unwrap_or("five_hour").to_owned();
                let utilization = r["utilization"].as_f64().map(|u| if u > 1.0 { u / 100.0 } else { u });
                let prev = self.rate_limits.get(&kind);
                self.rate_limits.insert(
                    kind.clone(),
                    RateLimit {
                        status: r["status"].as_str().unwrap_or("allowed").to_owned(),
                        resets_at: r["resetsAt"].as_u64().or_else(|| r["resetsAt"].as_f64().map(|f| f as u64)),
                        // events without a figure keep the last known one
                        utilization: utilization.or(prev.and_then(|p| p.utilization)),
                        kind,
                        at: Instant::now(),
                    },
                );
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------- images

const MAX_DIM: u32 = 2000;
const MAX_BYTES: usize = 3_700_000;

pub fn prepare_image(ctx: &egui::Context, name: &str, bytes: Vec<u8>) -> Result<Attachment, String> {
    let fmt = image::guess_format(&bytes).map_err(|e| format!("{name}: not an image ({e})"))?;
    let img = image::load_from_memory_with_format(&bytes, fmt).map_err(|e| format!("{name}: {e}"))?;
    let native = match fmt {
        image::ImageFormat::Png => Some("image/png"),
        image::ImageFormat::Jpeg => Some("image/jpeg"),
        image::ImageFormat::Gif => Some("image/gif"),
        image::ImageFormat::WebP => Some("image/webp"),
        _ => None,
    };
    let too_big = img.width() > MAX_DIM || img.height() > MAX_DIM;
    let (media_type, data) = if let (Some(mt), false, true) = (native, too_big, bytes.len() <= MAX_BYTES) {
        (mt.to_owned(), bytes)
    } else {
        let img = if too_big { img.resize(MAX_DIM, MAX_DIM, image::imageops::FilterType::Triangle) } else { img.clone() };
        let mut png = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .map_err(|e| e.to_string())?;
        if png.len() <= MAX_BYTES {
            ("image/png".to_owned(), png)
        } else {
            let mut jpg = Vec::new();
            let rgb = image::DynamicImage::ImageRgb8(img.to_rgb8());
            let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpg, 85);
            rgb.write_with_encoder(enc).map_err(|e| e.to_string())?;
            ("image/jpeg".to_owned(), jpg)
        }
    };
    let thumb = img.thumbnail(160, 160).to_rgba8();
    let ci = egui::ColorImage::from_rgba_unmultiplied([thumb.width() as usize, thumb.height() as usize], thumb.as_raw());
    let tex = ctx.load_texture(format!("att-{name}"), ci, egui::TextureOptions::LINEAR);
    Ok(Attachment {
        name: name.to_owned(),
        block: json!({
            "type": "image",
            "source": {"type": "base64", "media_type": media_type, "data": base64::engine::general_purpose::STANDARD.encode(data)}
        }),
        thumb: Some(tex),
        full: None,
    })
}

pub fn color_image_to_png(img: &egui::ColorImage) -> Vec<u8> {
    let [w, h] = img.size;
    let mut raw = Vec::with_capacity(w * h * 4);
    for c in &img.pixels {
        raw.extend_from_slice(&c.to_srgba_unmultiplied());
    }
    let mut out = Vec::new();
    if let Some(buf) = image::RgbaImage::from_raw(w as u32, h as u32, raw) {
        let _ = image::DynamicImage::ImageRgba8(buf).write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png);
    }
    out
}

/// One-line description of a tool call.
pub fn tool_summary(name: &str, input: &Value, cwd: &Path) -> String {
    let s = |k: &str| input[k].as_str().unwrap_or("").to_owned();
    let rel = |p: String| {
        let pb = PathBuf::from(&p);
        pb.strip_prefix(cwd).map(|r| r.to_string_lossy().replace('\\', "/")).unwrap_or(p)
    };
    let out = match name {
        "Read" | "Write" | "Edit" | "MultiEdit" => rel(s("file_path")),
        "NotebookEdit" => rel(s("notebook_path")),
        "Bash" | "PowerShell" => s("command").lines().next().unwrap_or("").to_owned(),
        "Grep" => format!("{}  {}", s("pattern"), s("path")),
        "Glob" => s("pattern"),
        "WebFetch" => s("url"),
        "WebSearch" => s("query"),
        "Task" | "Agent" => s("description"),
        "Skill" => s("skill"),
        "TodoWrite" => format!("{} tasks", input["todos"].as_array().map(|a| a.len()).unwrap_or(0)),
        "ExitPlanMode" => "Plan ready for review".into(),
        _ => {
            let j = input.to_string();
            j
        }
    };
    crate::util::truncate(&out, 140)
}

pub fn tool_file_path(name: &str, input: &Value) -> Option<PathBuf> {
    match name {
        "Read" | "Write" | "Edit" | "MultiEdit" => input["file_path"].as_str().map(PathBuf::from),
        "NotebookEdit" => input["notebook_path"].as_str().map(PathBuf::from),
        _ => None,
    }
}

// ------------------------------------------------------------ past conversations

/// A conversation stored by Claude Code in ~/.claude/projects/<cwd>/<id>.jsonl
#[derive(Clone)]
pub struct SessionInfo {
    pub id: String,
    pub title: String,
    pub modified: u64,
}

/// ~/.claude/projects/<cwd with every non-alphanumeric char replaced by '-'>
fn sessions_dir(cwd: &Path) -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    let base = PathBuf::from(home).join(".claude").join("projects");
    let want: String = cwd.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    std::fs::read_dir(&base)
        .ok()?
        .flatten()
        .find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(&want))
        .map(|e| e.path())
}

/// Text of a user message, or None for tool results / injected meta messages.
fn user_text(v: &Value) -> Option<String> {
    if v["isMeta"].as_bool() == Some(true) {
        return None;
    }
    let c = &v["message"]["content"];
    let t = match c {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .filter(|t| !t.trim_start().starts_with('<')) // injected context blocks
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    let t = t.trim();
    if t.is_empty() || t.starts_with('<') {
        return None; // command wrappers, system reminders
    }
    Some(t.to_owned())
}

/// Title of a stored conversation: its summary, else the first user message, else the last prompt.
/// Reads only up to the first one found; `None` for sessions with nothing typed.
fn read_title(path: &Path) -> Option<String> {
    let f = std::fs::File::open(path).ok()?;
    let mut last_prompt = None;
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        if v["type"] == "summary" {
            if let Some(s) = v["summary"].as_str() {
                return Some(s.to_owned());
            }
        }
        if v["type"] == "user" {
            if let Some(t) = user_text(&v) {
                return Some(t.lines().next().unwrap_or("").to_owned());
            }
        }
        if v["type"] == "last-prompt" && last_prompt.is_none() {
            last_prompt = v["lastPrompt"].as_str().map(|s| s.lines().next().unwrap_or("").to_owned());
        }
    }
    last_prompt
}

/// `read_title` memoized: a found title never changes, a missing one is retried when the file grows.
fn cached_title(path: &Path, len: u64) -> Option<String> {
    static CACHE: std::sync::Mutex<Option<HashMap<PathBuf, (u64, Option<String>)>>> = std::sync::Mutex::new(None);
    let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let cache = guard.get_or_insert_with(HashMap::new);
    match cache.get(path) {
        Some((_, Some(t))) => return Some(t.clone()),
        Some((l, None)) if *l == len => return None,
        _ => {}
    }
    let t = read_title(path).map(|t| crate::util::truncate(&t, 80));
    cache.insert(path.to_owned(), (len, t.clone()));
    t
}

/// Title of the stored conversation `id` in `cwd`, without loading its transcript.
pub fn session_title(cwd: &Path, id: &str) -> Option<String> {
    let path = sessions_dir(cwd)?.join(format!("{id}.jsonl"));
    let len = std::fs::metadata(&path).ok()?.len();
    cached_title(&path, len)
}

/// Most recent conversations first.
pub fn list_sessions(cwd: &Path) -> Vec<SessionInfo> {
    let Some(dir) = sessions_dir(cwd) else { return vec![] };
    let mut files: Vec<(u64, u64, PathBuf)> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
                .filter_map(|e| {
                    let meta = e.metadata().ok()?;
                    let secs = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
                    Some((secs, meta.len(), e.path()))
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    files
        .into_iter()
        .filter_map(|(modified, len, path)| {
            let id = path.file_stem()?.to_string_lossy().into_owned();
            let title = cached_title(&path, len)?; // skip empty sessions
            Some(SessionInfo { id, title, modified })
        })
        .collect()
}

/// Rebuild the chat view from a stored conversation.
pub fn load_transcript(cwd: &Path, id: &str) -> Vec<Item> {
    let Some(dir) = sessions_dir(cwd) else { return vec![] };
    let Ok(f) = std::fs::File::open(dir.join(format!("{id}.jsonl"))) else { return vec![] };
    let mut items: Vec<Item> = vec![];
    let mut tools: HashMap<String, usize> = HashMap::new();
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        let side = v["isSidechain"].as_bool() == Some(true);
        match v["type"].as_str() {
            Some("user") => {
                if let Some(arr) = v["message"]["content"].as_array() {
                    for b in arr.iter().filter(|b| b["type"] == "tool_result") {
                        let tid = b["tool_use_id"].as_str().unwrap_or("");
                        if let Some(&i) = tools.get(tid) {
                            if let Some(Item::Tool { result, .. }) = items.get_mut(i) {
                                let text = match &b["content"] {
                                    Value::String(s) => s.clone(),
                                    Value::Array(a) => a.iter().filter_map(|x| x["text"].as_str()).collect::<Vec<_>>().join("\n"),
                                    _ => String::new(),
                                };
                                *result = Some((crate::util::truncate(&text, 4000), b["is_error"].as_bool() == Some(true)));
                            }
                        }
                    }
                }
                if !side {
                    if let Some(t) = user_text(&v) {
                        items.push(Item::User { text: t, thumbs: vec![], files: vec![] });
                    }
                }
            }
            Some("assistant") => {
                let Some(arr) = v["message"]["content"].as_array() else { continue };
                for b in arr {
                    match b["type"].as_str() {
                        Some("text") if !side => {
                            let t = b["text"].as_str().unwrap_or("").to_owned();
                            if !t.trim().is_empty() {
                                items.push(Item::Assistant(t));
                            }
                        }
                        Some("thinking") if !side => {
                            let t = b["thinking"].as_str().unwrap_or("").to_owned();
                            if !t.trim().is_empty() {
                                items.push(Item::Thinking(t));
                            }
                        }
                        Some("tool_use") => {
                            let tid = b["id"].as_str().unwrap_or("").to_owned();
                            tools.insert(tid.clone(), items.len());
                            items.push(Item::Tool {
                                id: tid,
                                name: b["name"].as_str().unwrap_or("").to_owned(),
                                input: b["input"].clone(),
                                result: None,
                                sub: side,
                            });
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    // keep the view light: last 500 entries
    if items.len() > 500 {
        items.drain(..items.len() - 500);
    }
    // any tool still without a result was interrupted
    for it in &mut items {
        if let Item::Tool { result: r @ None, .. } = it {
            *r = Some(("(no result recorded)".into(), false));
        }
    }
    items
}

impl Chat {
    /// Continue a stored conversation: show its history and resume it on the next message.
    pub fn open_session(&mut self, id: &str) {
        self.new_chat();
        self.items = load_transcript(&self.cwd, id);
        self.items.push(Item::System("Conversation restored — your next message continues it.".into()));
        self.resume_id = Some(id.to_owned());
        self.session_id = None;
    }

    /// Like `open_session`, but only remembers the id: nothing is read until `ensure_loaded`.
    pub fn restore_session(&mut self, id: &str) {
        self.new_chat();
        self.resume_id = Some(id.to_owned());
        self.title_hint = session_title(&self.cwd, id);
        self.lazy = true;
    }

    /// Load the transcript of a restored tab (call before showing it).
    pub fn ensure_loaded(&mut self) {
        if self.lazy {
            if let Some(id) = self.resume_id.clone() {
                self.open_session(&id);
            }
        }
    }

    /// Title of the current conversation (first user message).
    pub fn title(&self) -> Option<String> {
        self.items
            .iter()
            .find_map(|i| match i {
                Item::User { text, .. } if !text.is_empty() => Some(crate::util::truncate(text.lines().next().unwrap_or(""), 60)),
                _ => None,
            })
            .or_else(|| self.title_hint.as_ref().map(|t| crate::util::truncate(t, 60)))
    }
}

#[cfg(test)]
mod history_tests {
    use super::*;

    #[test]
    #[ignore = "reads this machine's ~/.claude sessions; run with --ignored"]
    fn reads_real_sessions() {
        // VS_TEST_CWD picks the project folder (default: the home folder)
        let cwd = PathBuf::from(std::env::var("VS_TEST_CWD").or_else(|_| std::env::var("USERPROFILE")).unwrap());
        let t = std::time::Instant::now();
        let list = list_sessions(&cwd);
        eprintln!("sessions: {} in {:?}", list.len(), t.elapsed());
        let t = std::time::Instant::now();
        let again = list_sessions(&cwd);
        eprintln!("again (cached titles): {} in {:?}", again.len(), t.elapsed());
        for s in list.iter().take(3) {
            eprintln!("  {} | {} | {}", &s.id[..8], s.title, crate::util::fmt_ago(s.modified));
        }
        if let Some(s) = list.get(1) {
            let t = std::time::Instant::now();
            let items = load_transcript(&cwd, &s.id);
            let tools = items.iter().filter(|i| matches!(i, Item::Tool { .. })).count();
            let users = items.iter().filter(|i| matches!(i, Item::User { .. })).count();
            eprintln!("transcript: {} items ({users} user, {tools} tools) in {:?}", items.len(), t.elapsed());
        }
    }
}
