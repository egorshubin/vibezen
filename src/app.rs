//! Main application: multi-project workspace in a single window.

use crate::claude::{Chat, MODELS, MODES, Signal};
use crate::claude_ui::{self, ChatAction};
use crate::editor::{self, Doc, EditorCtx};
use crate::git::{GitFile, GitState, GitUiAction};
use crate::highlight::Highlighting;
use crate::plan::{self, PlanUsage};
use crate::search::{self, FileIndex, FindInFiles};
use crate::settings::{ProjectMemo, Settings};
use crate::theme::{self, Palette};
use crate::tree::{FileTree, TreeAction, file_badge};
use crate::util;
use eframe::egui::{
    self, Align, Align2, Color32, FontId, Frame, Id, Key, KeyboardShortcut, Layout, Margin, Modifiers, RichText,
    Sense, Stroke, Ui, ViewportCommand, pos2, vec2,
};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Quiet time after the last keystroke before an edited file is auto-saved.
const AUTO_SAVE_DELAY: Duration = Duration::from_millis(1500);

#[derive(PartialEq, Clone, Copy)]
enum Attention {
    None,
    Done,
    Error,
}

pub struct Project {
    pub root: PathBuf,
    pub name: String,
    tree: FileTree,
    docs: Vec<Doc>,
    /// `None` = the Claude Code tab `active_chat`
    active_doc: Option<usize>,
    /// last real file viewed, offered as an @-mention in the Claude tab
    last_file: Option<PathBuf>,
    git: GitState,
    /// one tab per conversation, shown before the files; never empty
    chats: Vec<Chat>,
    active_chat: usize,
    index: FileIndex,
    find: FindInFiles,
    attention: Attention,
    last_session: Option<String>,
    /// unix secs of the last action in this project
    last_active: u64,
}

fn memo_key(p: &Path) -> String {
    p.to_string_lossy().to_lowercase()
}

impl Project {
    fn new(root: PathBuf, ctx: &egui::Context, s: &Settings, hl: &Highlighting) -> Self {
        let name = root
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.display().to_string());
        let memo = s.memo.get(&memo_key(&root)).cloned().unwrap_or_default();
        let mut p = Project {
            name,
            tree: FileTree::new(root.clone()),
            docs: vec![],
            active_doc: None,
            last_file: None,
            git: GitState::new(root.clone(), ctx.clone()),
            chats: vec![Chat::new(root.clone(), ctx.clone(), s.claude_model.clone(), s.claude_mode.clone(), None)],
            active_chat: 0,
            index: FileIndex::build(root.clone(), ctx.clone()),
            find: FindInFiles::default(),
            attention: Attention::None,
            last_session: memo.last_session.clone(),
            last_active: if memo.last_active > 0 { memo.last_active } else { util::unix_now() },
            root,
        };
        // conversation tabs come back as bare session ids: the transcript is read when a tab is
        // first shown, the process starts on the next message
        // no saved tab list (older settings, or only blank tabs) → at least the last conversation
        let chats: Vec<String> = if memo.chats.is_empty() { memo.last_session.iter().cloned().collect() } else { memo.chats.clone() };
        for (n, id) in chats.iter().enumerate() {
            if n > 0 {
                let c = p.chats[0].sibling();
                p.chats.push(c);
            }
            p.chats[n].restore_session(id);
        }
        p.active_chat = memo
            .active_chat
            .as_ref()
            .and_then(|a| p.chats.iter().position(|c| c.holds_session(a)))
            .unwrap_or(0);
        for f in &memo.open_files {
            let path = PathBuf::from(f);
            if path.is_file() {
                p.docs.push(Doc::open(&path, hl));
            }
        }
        p.active_doc = memo
            .active_file
            .as_ref()
            .and_then(|a| p.docs.iter().position(|d| d.path.to_string_lossy() == *a));
        p
    }

    fn chat_ids(&self) -> impl Iterator<Item = &String> {
        self.chats.iter().filter_map(|c| c.session_id.as_ref().or(c.resume_id.as_ref()))
    }

    /// Changes whenever the set of open tabs or the tab in front does; triggers a settings save.
    fn tabs_sig(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for d in &self.docs {
            d.path.hash(&mut h);
        }
        self.chat_ids().for_each(|id| id.hash(&mut h));
        (self.active_doc, self.active_chat).hash(&mut h);
        h.finish()
    }

    fn memo(&self) -> ProjectMemo {
        ProjectMemo {
            open_files: self
                .docs
                .iter()
                .filter(|d| !d.is_virtual())
                .map(|d| d.path.to_string_lossy().into_owned())
                .collect(),
            active_file: self
                .active_doc
                .and_then(|i| self.docs.get(i))
                .filter(|d| !d.is_virtual())
                .map(|d| d.path.to_string_lossy().into_owned()),
            chats: self.chat_ids().cloned().collect(),
            active_chat: self.chat().session_id.clone().or(self.chat().resume_id.clone()),
            last_session:self.chat().session_id.clone().or(self.last_session.clone()),
            last_active: self.last_active,
        }
    }

    fn check_disk(&mut self, hl: &Highlighting) {
        for d in &mut self.docs {
            d.check_disk(hl);
        }
    }

    fn dirty_count(&self) -> usize {
        self.docs.iter().filter(|d| d.dirty).count()
    }

    fn chat(&self) -> &Chat {
        &self.chats[self.active_chat]
    }

    fn chat_mut(&mut self) -> &mut Chat {
        &mut self.chats[self.active_chat]
    }

    fn busy(&self) -> bool {
        self.chats.iter().any(|c| c.busy)
    }

    fn pending_approvals(&self) -> usize {
        self.chats.iter().map(|c| c.pending_approvals()).sum()
    }

    /// The conversation that needs the user most: approval > working > unread reply > current.
    fn status_chat(&self) -> &Chat {
        self.chats
            .iter()
            .find(|c| c.pending_approvals() > 0)
            .or_else(|| self.chats.iter().find(|c| c.busy))
            .unwrap_or(self.chat())
    }

    fn show_chat(&mut self, i: usize) {
        self.active_chat = i.min(self.chats.len() - 1);
        self.active_doc = None;
        let c = self.chat_mut();
        c.ensure_loaded();
        c.unseen = false;
        c.focus_input = true;
    }

    /// An untouched conversation tab, or a new one.
    fn blank_chat(&mut self) -> usize {
        if let Some(i) = self.chats.iter().position(|c| c.is_blank()) {
            return i;
        }
        let c = self.chats[0].sibling();
        self.chats.push(c);
        self.chats.len() - 1
    }

    fn new_session(&mut self) {
        let i = self.blank_chat();
        self.show_chat(i);
    }

    /// Show a stored conversation: its tab if already open, else a new tab.
    fn open_session(&mut self, id: &str) {
        let i = match self.chats.iter().position(|c| c.holds_session(id)) {
            Some(i) => i,
            None => {
                let i = self.blank_chat();
                self.chats[i].open_session(id);
                i
            }
        };
        self.show_chat(i);
    }

    /// Close a conversation tab (stops its process). The last one is replaced by a new session.
    fn close_chat(&mut self, i: usize) {
        if i >= self.chats.len() {
            return;
        }
        let viewing = self.active_doc.is_none() && self.active_chat == i;
        if self.chats.len() == 1 {
            let c = self.chats[0].sibling();
            self.chats.push(c);
        }
        if let Some(id) = self.chats[i].session_id.clone() {
            self.last_session = Some(id);
        }
        self.chats.remove(i);
        if self.active_chat > i || self.active_chat >= self.chats.len() {
            self.active_chat -= 1;
        }
        if viewing {
            self.show_chat(self.active_chat);
        }
    }
}

#[derive(PartialEq, Clone, Copy)]
enum LeftTab {
    Project,
    Commit,
    Log,
    Find,
    Claude,
}

enum PromptKind {
    NewFile(PathBuf),
    NewFolder(PathBuf),
    Rename(PathBuf),
    GotoLine,
    NewBranch,
}

struct Prompt {
    kind: PromptKind,
    value: String,
    focus: bool,
}

enum Confirm {
    Delete(PathBuf),
    Rollback(Vec<GitFile>),
    CloseTab(usize),
    /// a conversation tab whose agent is still working
    CloseChat(usize),
    CloseProject(usize),
    Exit,
}

struct QuickOpen {
    query: String,
    results: Vec<String>,
    computed: Option<String>,
    sel: usize,
    focus: bool,
}

enum Action {
    SwitchView(usize, View),
    OpenProjectView(PathBuf, View),
    ForgetRecent(String),
    OpenFile(PathBuf, Option<usize>),
    /// permission mode / model chosen in a Claude input: applies to all projects, persisted
    ClaudeMode(String),
    ClaudeModel(String),
    /// open a file and select a search match
    OpenFileAt { path: PathBuf, line: usize, col: usize, len: usize },
    OpenDiff { title: String, key: String, text: String },
    Switch(usize),
    AddProjectDialog,
    OpenProject(PathBuf),
    CloseProject(usize),
    CloseTab(usize),
    CloseOthers(usize),
    /// Claude Code conversation tabs of the active project
    ShowChat(usize),
    CloseChat(usize),
    NewSession,
    OpenSession(String),
    Prompt(PromptKind),
    Confirm(Confirm),
    Terminal(PathBuf),
    Reveal(PathBuf),
    Copy(String),
    AskClaude(String),
    SaveAll,
}

pub struct IdeApp {
    settings: Settings,
    hl: Highlighting,
    projects: Vec<Project>,
    active: usize,
    left: Option<LeftTab>,
    prompt: Option<Prompt>,
    confirm: Option<Confirm>,
    quick: Option<QuickOpen>,
    show_settings: bool,
    focused: bool,
    allow_close: bool,
    title_for: Option<(usize, String)>,
    message: Option<(String, bool)>,
    titlebar_themed: bool,
    /// in-app notification cards (bottom-right)
    toasts: Vec<InAppToast>,
    /// project roots whose Windows toast was clicked
    toast_click_tx: std::sync::mpsc::Sender<PathBuf>,
    toast_click_rx: std::sync::mpsc::Receiver<PathBuf>,
    /// account-wide plan limits from Anthropic (all clients, not just our agents)
    plan: PlanUsage,
    /// `Project::tabs_sig` of the active project at the last save
    tabs_sig: u64,
}

struct InAppToast {
    title: String,
    body: String,
    root: PathBuf,
    color: Color32,
    at: std::time::Instant,
}

pub(crate) fn setup_fonts(ctx: &egui::Context) {
    use egui::{FontData, FontFamily, epaint::text::{FontInsert, FontPriority, InsertFontFamily}};
    let win = PathBuf::from(std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into())).join("Fonts");
    let local = std::env::var_os("LOCALAPPDATA")
        .map(|l| PathBuf::from(l).join("Microsoft").join("Windows").join("Fonts"));
    let read = |names: &[&str]| -> Option<(String, Vec<u8>)> {
        for n in names {
            for d in [Some(win.clone()), local.clone()].into_iter().flatten() {
                if let Ok(b) = std::fs::read(d.join(n)) {
                    return Some((n.to_string(), b));
                }
            }
        }
        None
    };
    let fam = |f: FontFamily, p: FontPriority| InsertFontFamily { family: f, priority: p };
    if let Some((n, b)) = read(&["segoeui.ttf"]) {
        ctx.add_font(FontInsert::new(&n, FontData::from_owned(b), vec![fam(FontFamily::Proportional, FontPriority::Highest)]));
    }
    if let Some((n, b)) = read(&["JetBrainsMono-Regular.ttf", "CascadiaMono.ttf", "CascadiaCode.ttf", "consola.ttf"]) {
        ctx.add_font(FontInsert::new(&n, FontData::from_owned(b), vec![fam(FontFamily::Monospace, FontPriority::Highest)]));
    }
    if let Some((n, b)) = read(&["seguisym.ttf"]) {
        ctx.add_font(FontInsert::new(
            &n,
            FontData::from_owned(b),
            vec![fam(FontFamily::Proportional, FontPriority::Lowest), fam(FontFamily::Monospace, FontPriority::Lowest)],
        ));
    }
}

impl IdeApp {
    pub fn new(cc: &eframe::CreationContext<'_>, arg: Option<String>) -> Self {
        let ctx = cc.egui_ctx.clone();
        setup_fonts(&ctx);
        let settings = Settings::load();
        theme::apply(&ctx, settings.dark, settings.ui_font_size);
        ctx.send_viewport_cmd(ViewportCommand::SetTheme(if settings.dark { egui::SystemTheme::Dark } else { egui::SystemTheme::Light }));
        let hl = Highlighting::new();
        let (toast_click_tx, toast_click_rx) = std::sync::mpsc::channel();
        let mut app = IdeApp {
            toasts: vec![],
            toast_click_tx,
            toast_click_rx,
            plan: PlanUsage::new(),
            hl,
            projects: vec![],
            active: 0,
            // VIBEZEN_VIEW=git / log / find opens that tool window on start
            left: Some(match std::env::var("VIBEZEN_VIEW").as_deref() {
                Ok("git") => LeftTab::Commit,
                Ok("log") => LeftTab::Log,
                Ok("find") => LeftTab::Find,
                _ => LeftTab::Project,
            }),
            prompt: None,
            confirm: None,
            quick: None,
            show_settings: false,
            focused: true,
            allow_close: false,
            title_for: None,
            message: None,
            titlebar_themed: false,
            tabs_sig: 0,
            settings,
        };
        // Only the project worked with last is loaded (or the one passed on the command line);
        // the others wait in RECENT, so no agents start for them.
        let open: Vec<String> = app.settings.open_projects.clone();
        let last = open.get(app.settings.active_project).or(open.first()).cloned();
        for p in open.iter().rev() {
            app.settings.push_recent(p);
        }
        if let (None, Some(p)) = (&arg, last) {
            let pb = PathBuf::from(&p);
            if pb.is_dir() {
                app.add_project(pb, &ctx, true);
            }
        }
        app.persist();
        if let Some(a) = arg {
            let pb = PathBuf::from(a);
            if pb.is_dir() {
                app.add_project(pb, &ctx, true);
            } else if pb.is_file() {
                if let Some(parent) = pb.parent() {
                    app.add_project(parent.to_path_buf(), &ctx, true);
                    app.open_file(&pb, None);
                }
            }
        }
        app
    }

    fn add_project(&mut self, root: PathBuf, ctx: &egui::Context, switch: bool) {
        let root = std::fs::canonicalize(&root)
            .map(|p| PathBuf::from(p.to_string_lossy().trim_start_matches(r"\\?\")))
            .unwrap_or(root);
        if let Some(i) = self.projects.iter().position(|p| memo_key(&p.root) == memo_key(&root)) {
            if switch {
                self.switch(i);
            }
            return;
        }
        let mut p = Project::new(root.clone(), ctx, &self.settings, &self.hl);
        if switch {
            p.last_active = util::unix_now();
        }
        self.projects.push(p); // open projects stay in load order
        self.settings.push_recent(&root.to_string_lossy());
        if switch {
            self.switch(self.projects.len() - 1);
        }
        self.persist();
    }

    fn switch(&mut self, i: usize) {
        if i < self.projects.len() {
            if i != self.active {
                self.auto_save(true);
            }
            self.active = i;
            let p = &mut self.projects[i];
            p.attention = Attention::None;
            // back to the tab that was in front when the project was left
            if p.active_doc.is_none_or(|d| d >= p.docs.len()) {
                p.show_chat(p.active_chat);
            }
            p.check_disk(&self.hl);
            p.git.refresh_status();
            self.persist();
        }
    }

    fn persist(&mut self) {
        self.settings.open_projects = self.projects.iter().map(|p| p.root.to_string_lossy().into_owned()).collect();
        self.settings.active_project = self.active;
        for p in &self.projects {
            self.settings.memo.insert(memo_key(&p.root), p.memo());
        }
        self.tabs_sig = self.projects.get(self.active).map(|p| p.tabs_sig()).unwrap_or(0);
        self.settings.save();
    }

    fn show_view(&mut self, v: View) {
        match v {
            View::Files => self.left = Some(LeftTab::Project),
            View::Git => {
                self.left = Some(LeftTab::Commit);
                if let Some(p) = self.proj() {
                    p.git.refresh_all();
                }
            }
        }
    }

    fn show_claude(&mut self) {
        if let Some(p) = self.proj() {
            p.show_chat(p.active_chat);
        }
    }

    fn close_chat(&mut self, i: usize, force: bool) {
        let Some(p) = self.projects.get_mut(self.active) else { return };
        if !force && p.chats.get(i).is_some_and(|c| c.busy) {
            self.confirm = Some(Confirm::CloseChat(i));
            return;
        }
        p.close_chat(i);
        self.persist();
    }

    fn proj(&mut self) -> Option<&mut Project> {
        self.projects.get_mut(self.active)
    }

    fn open_file(&mut self, path: &Path, line: Option<usize>) {
        let hl = &self.hl;
        let Some(p) = self.projects.get_mut(self.active) else { return };
        let idx = match p.docs.iter().position(|d| d.path == path) {
            Some(i) => i,
            None => {
                if !path.is_file() {
                    self.message = Some((format!("File not found: {}", path.display()), true));
                    return;
                }
                p.docs.push(Doc::open(path, hl));
                p.docs.len() - 1
            }
        };
        p.active_doc = Some(idx);
        p.tree.reveal(path);
        if let Some(l) = line {
            p.docs[idx].goto_line(l, 1);
        } else {
            let id = p.docs[idx].id;
            let _ = id;
        }
    }

    fn open_virtual(&mut self, title: String, key: String, text: String) {
        let Some(p) = self.proj() else { return };
        if let Some(i) = p.docs.iter().position(|d| d.path == PathBuf::from(format!("vibezen://{key}"))) {
            p.docs[i] = Doc::virtual_doc(&title, &key, text);
            p.active_doc = Some(i);
        } else {
            p.docs.push(Doc::virtual_doc(&title, &key, text));
            p.active_doc = Some(p.docs.len() - 1);
        }
    }

    fn close_tab(&mut self, i: usize, force: bool) {
        let Some(p) = self.projects.get_mut(self.active) else { return };
        if i >= p.docs.len() {
            return;
        }
        if self.settings.auto_save && !force {
            let _ = p.docs[i].auto_save(&self.hl);
        }
        if p.docs[i].dirty && !force {
            self.confirm = Some(Confirm::CloseTab(i));
            return;
        }
        p.docs.remove(i);
        p.active_doc = match p.active_doc {
            _ if p.docs.is_empty() => None,
            Some(a) if a > i => Some(a - 1),
            Some(a) if a == i => Some(i.min(p.docs.len() - 1)),
            other => other,
        };
        self.persist();
    }

    fn close_project(&mut self, i: usize, force: bool) {
        if i >= self.projects.len() {
            return;
        }
        if !force && (self.projects[i].dirty_count() > 0 || self.projects[i].busy()) {
            self.confirm = Some(Confirm::CloseProject(i));
            return;
        }
        let memo = self.projects[i].memo();
        self.settings.memo.insert(memo_key(&self.projects[i].root), memo);
        let root = self.projects[i].root.to_string_lossy().into_owned();
        self.settings.push_recent(&root); // reappears in RECENT with a grey folder
        self.projects.remove(i); // drops the Claude process
        if self.active >= self.projects.len() {
            self.active = self.projects.len().saturating_sub(1);
        } else if self.active > i {
            self.active -= 1;
        }
        self.persist();
    }

    /// Auto-save edited files: those idle for `AUTO_SAVE_DELAY`, or all of them with `now`
    /// (focus lost, project left, exit). Files changed on disk meanwhile are left alone.
    fn auto_save(&mut self, now: bool) {
        if !self.settings.auto_save {
            return;
        }
        let mut errs = vec![];
        for p in &mut self.projects {
            let mut saved = false;
            for d in &mut p.docs {
                if !d.dirty || !(now || d.edited_at.is_some_and(|t| t.elapsed() >= AUTO_SAVE_DELAY)) {
                    continue;
                }
                match d.auto_save(&self.hl) {
                    Ok(s) => saved |= s,
                    Err(e) => errs.push(e),
                }
                d.edited_at = None; // one attempt per burst of edits, no retry loop
            }
            if saved {
                p.git.refresh_status();
                p.last_active = util::unix_now();
            }
        }
        if !errs.is_empty() {
            self.message = Some((errs.join("; "), true));
        }
    }

    fn save_all(&mut self) {
        let mut errs = vec![];
        if let Some(p) = self.projects.get_mut(self.active) {
            for d in &mut p.docs {
                if let Err(e) = d.save() {
                    errs.push(e);
                }
            }
            p.git.refresh_status();
            p.last_active = util::unix_now();
        }
        if !errs.is_empty() {
            self.message = Some((errs.join("; "), true));
        }
    }

    fn notify(&mut self, ctx: &egui::Context, title: &str, body: &str, project_idx: usize, color: Color32) {
        if !self.settings.notify {
            return;
        }
        let Some(root) = self.projects.get(project_idx).map(|p| p.root.clone()) else { return };
        // in-app card: always, so it's seen even while working in the IDE
        self.toasts.retain(|t| t.root != root);
        self.toasts.push(InAppToast { title: title.to_owned(), body: body.to_owned(), root: root.clone(), color, at: std::time::Instant::now() });
        if self.toasts.len() > 4 {
            self.toasts.remove(0);
        }
        let visible = self.focused && project_idx == self.active;
        if self.settings.notify_only_unfocused && visible {
            return;
        }
        // Windows toast; clicking it brings VibeZen to the front on that project
        let tx = self.toast_click_tx.clone();
        let c = ctx.clone();
        util::notify(
            title,
            body,
            self.settings.notify_sound,
            Some(Box::new(move || {
                let _ = tx.send(root.clone());
                c.request_repaint();
            })),
        );
    }

    /// Bottom-right notification cards. Click = go to the project.
    fn toasts_ui(&mut self, ctx: &egui::Context, pal: &Palette) {
        const LIFE: f32 = 9.0;
        self.toasts.retain(|t| t.at.elapsed().as_secs_f32() < LIFE);
        if self.toasts.is_empty() {
            return;
        }
        let mut go: Option<PathBuf> = None;
        let mut close: Option<usize> = None;
        egui::Area::new(Id::new("inapp_toasts"))
            .order(egui::Order::Foreground)
            .anchor(Align2::RIGHT_BOTTOM, vec2(-14.0, -34.0))
            .show(ctx, |ui| {
                ui.set_max_width(360.0);
                for (i, t) in self.toasts.iter().enumerate().rev() {
                    let r = Frame::NONE
                        .fill(pal.card)
                        .stroke(Stroke::new(1.0, pal.border))
                        .corner_radius(8)
                        .shadow(egui::Shadow { offset: [0, 4], blur: 16, spread: 0, color: Color32::from_black_alpha(90) })
                        .inner_margin(Margin { left: 12, right: 10, top: 9, bottom: 9 })
                        .show(ui, |ui| {
                            ui.set_width(330.0);
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("●").color(t.color));
                                ui.add(egui::Label::new(RichText::new(&t.title).strong()).truncate());
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if ui.add(egui::Button::new(RichText::new("✕").small()).frame(false)).clicked() {
                                        close = Some(i);
                                    }
                                });
                            });
                            if !t.body.is_empty() {
                                ui.add(egui::Label::new(RichText::new(&t.body).small().color(pal.text_weak)).wrap());
                            }
                        });
                    let resp = ui.interact(r.response.rect, Id::new(("toast_click", i)), Sense::click());
                    if resp.hovered() {
                        ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    if resp.clicked() && close.is_none() {
                        go = Some(t.root.clone());
                    }
                    ui.add_space(6.0);
                }
            });
        if let Some(i) = close {
            self.toasts.remove(i);
        }
        if let Some(root) = go {
            self.toasts.retain(|t| t.root != root);
            if let Some(i) = self.projects.iter().position(|p| p.root == root) {
                self.switch(i);
            }
        }
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    // ------------------------------------------------------------------ shortcuts

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let mut sc = |m: Modifiers, k: Key| {
            ctx.input_mut(|i| {
                let hit = i.consume_shortcut(&KeyboardShortcut::new(m, k));
                if hit {
                    // Alt+digit etc. also produce a text event on Windows; swallow it.
                    i.events.retain(|e| !matches!(e, egui::Event::Text(_)));
                }
                hit
            })
        };
        let cs = Modifiers::COMMAND | Modifiers::SHIFT;
        let ca = Modifiers::COMMAND | Modifiers::ALT;
        if sc(cs, Key::N) {
            self.quick = Some(QuickOpen { query: String::new(), results: vec![], computed: None, sel: 0, focus: true });
        }
        if sc(Modifiers::ALT, Key::Insert) {
            if let Some(p) = self.proj() {
                let dir = p.tree.new_target_dir();
                self.prompt = Some(Prompt { kind: PromptKind::NewFile(dir), value: String::new(), focus: true });
            }
        }
        if sc(cs, Key::F) {
            self.left = Some(LeftTab::Find);
            if let Some(p) = self.projects.get_mut(self.active) {
                p.find.focus = true;
                // PhpStorm: the editor selection becomes the query
                let sel = p.active_doc.and_then(|i| p.docs.get(i)).and_then(|d| d.selected_text(ctx));
                if let Some(s) = sel {
                    if s != p.find.query || !p.find.up_to_date() {
                        p.find.query = s;
                        p.find.start(p.root.clone(), ctx.clone());
                    }
                }
            }
        }
        if sc(cs, Key::K) {
            if let Some(p) = self.proj() {
                p.git.push();
            }
        }
        if sc(ca, Key::OpenBracket) && !self.projects.is_empty() {
            let n = self.projects.len();
            self.switch((self.active + n - 1) % n);
        }
        if sc(ca, Key::CloseBracket) && !self.projects.is_empty() {
            let n = self.projects.len();
            self.switch((self.active + 1) % n);
        }
        if sc(Modifiers::COMMAND, Key::S) {
            self.save_all();
        }
        if sc(Modifiers::COMMAND, Key::F) {
            if let Some(p) = self.proj() {
                if let Some(d) = p.active_doc.and_then(|i| p.docs.get_mut(i)) {
                    d.open_find();
                }
            }
        }
        if sc(Modifiers::COMMAND, Key::G) && self.proj().map(|p| p.active_doc.is_some()).unwrap_or(false) {
            self.prompt = Some(Prompt { kind: PromptKind::GotoLine, value: String::new(), focus: true });
        }
        if sc(Modifiers::COMMAND, Key::W) || sc(Modifiers::COMMAND, Key::F4) {
            match self.proj().map(|p| (p.active_doc, p.active_chat)) {
                Some((Some(i), _)) => self.close_tab(i, false),
                Some((None, c)) => self.close_chat(c, false),
                None => {}
            }
        }
        if sc(Modifiers::COMMAND, Key::Tab) {
            if let Some(p) = self.proj() {
                // the Claude tabs sit before the first file
                match p.active_doc {
                    None if p.active_chat + 1 < p.chats.len() => p.show_chat(p.active_chat + 1),
                    None if !p.docs.is_empty() => p.active_doc = Some(0),
                    Some(a) if a + 1 < p.docs.len() => p.active_doc = Some(a + 1),
                    _ => p.show_chat(0),
                }
            }
        }
        if sc(Modifiers::COMMAND, Key::K) {
            self.left = Some(LeftTab::Commit);
            if let Some(p) = self.proj() {
                let id = Id::new(("commit_msg", p.root.clone()));
                ctx.memory_mut(|m| m.request_focus(id));
            }
        }
        if sc(Modifiers::COMMAND, Key::T) {
            if let Some(p) = self.proj() {
                p.git.pull();
            }
        }
        if sc(Modifiers::ALT, Key::Num1) {
            self.left = if self.left == Some(LeftTab::Project) { None } else { Some(LeftTab::Project) };
        }
        if sc(Modifiers::ALT, Key::Num0) {
            self.left = if self.left == Some(LeftTab::Commit) { None } else { Some(LeftTab::Commit) };
        }
        if sc(Modifiers::ALT, Key::Num9) {
            self.left = if self.left == Some(LeftTab::Log) { None } else { Some(LeftTab::Log) };
        }
        if sc(Modifiers::ALT, Key::Num2) {
            self.show_claude();
        }
        if sc(Modifiers::ALT, Key::Num3) {
            self.left = if self.left == Some(LeftTab::Claude) { None } else { Some(LeftTab::Claude) };
        }
        if sc(Modifiers::COMMAND, Key::Equals) || sc(Modifiers::COMMAND, Key::Plus) {
            self.settings.editor_font_size = (self.settings.editor_font_size + 1.0).min(32.0);
        }
        if sc(Modifiers::COMMAND, Key::Minus) {
            self.settings.editor_font_size = (self.settings.editor_font_size - 1.0).max(8.0);
        }
        // Ctrl+Enter in commit message
        if let Some(p) = self.projects.get_mut(self.active) {
            if p.git.msg_focused && ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter)) {
                p.git.commit(false);
            }
        }
    }

    // ------------------------------------------------------------------ panels

    fn sidebar(&mut self, ui: &mut Ui, pal: &Palette, acts: &mut Vec<Action>) {
        let collapsed = self.settings.sidebar_collapsed;
        let now = ui.input(|i| i.time);
        ui.add_space(6.0);
        if collapsed {
            ui.vertical_centered(|ui| {
                if ui.small_button("»").on_hover_text("Expand sidebar").clicked() {
                    self.settings.sidebar_collapsed = false;
                    self.settings.save();
                }
                if ui.small_button("+").on_hover_text("Open project folder").clicked() {
                    acts.push(Action::AddProjectDialog);
                }
            });
        } else {
            ui.horizontal(|ui| {
                ui.add_space(4.0);
                ui.label(RichText::new("PROJECTS").small().strong().color(pal.text_weak));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("«").on_hover_text("Collapse sidebar").clicked() {
                        self.settings.sidebar_collapsed = true;
                        self.settings.save();
                    }
                    if ui.small_button("+").on_hover_text("Open project folder").clicked() {
                        acts.push(Action::AddProjectDialog);
                    }
                });
            });
        }
        ui.add_space(4.0);

        // recent (closed) projects
        let open: Vec<String> = self.projects.iter().map(|p| memo_key(&p.root)).collect();
        let recent: Vec<String> = self
            .settings
            .recent
            .iter()
            .filter(|r| !open.contains(&r.to_lowercase()))
            .cloned()
            .collect();
        let last_active = |r: &String| self.settings.memo.get(&r.to_lowercase()).map(|m| m.last_active).unwrap_or(0);
        let mut recent = recent;
        recent.sort_by_key(|r| std::cmp::Reverse(last_active(r))); // stable: unknown times keep list order
        recent.truncate(if collapsed { 10 } else { 15 });

        egui::ScrollArea::vertical().id_salt("projects_sb").auto_shrink([false, false]).show(ui, |ui| {
            for (i, p) in self.projects.iter().enumerate() {
                let approvals = p.pending_approvals();
                let pulse = 0.72 + 0.28 * ((now * 3.0).sin() as f32 * 0.5 + 0.5);
                // folder colour = agent state
                let (status, scol, fcol): (String, Color32, Color32) = if approvals > 0 {
                    (format!("⚠ needs approval ({approvals})"), FOLDER_ALERT, FOLDER_ALERT)
                } else if p.busy() {
                    let secs = p.chats.iter().filter_map(|c| c.busy_since).map(|t| t.elapsed().as_secs()).max().unwrap_or(0);
                    (format!("agent working {}", util::fmt_duration(secs)), FOLDER_WORKING, FOLDER_WORKING.gamma_multiply(pulse))
                } else if p.attention == Attention::Done {
                    ("✔ agent finished".into(), FOLDER_DONE, FOLDER_DONE)
                } else if p.attention == Attention::Error {
                    ("✖ agent stopped with error".into(), FOLDER_ALERT, FOLDER_ALERT)
                } else {
                    let ch = p.git.status.files.len();
                    let b = if p.git.status.branch.is_empty() { "—".to_owned() } else { p.git.status.branch.clone() };
                    (if ch > 0 { format!("⎇ {b} · {ch} changed") } else { format!("⎇ {b}") }, pal.text_weak, FOLDER_OPEN)
                };
                let row = ProjectRow {
                    name: &p.name,
                    path: &p.root,
                    status: &status,
                    status_color: scol,
                    folder: fcol,
                    lit: true,
                    active: i == self.active,
                    badge: if approvals > 0 { Some("!") } else { None },
                    unload: true,
                };
                match project_row(ui, pal, collapsed, row, ("open", i)) {
                    RowClick::Main => acts.push(Action::Switch(i)),
                    RowClick::Files => acts.push(Action::SwitchView(i, View::Files)),
                    RowClick::Git => acts.push(Action::SwitchView(i, View::Git)),
                    RowClick::Unload => acts.push(Action::CloseProject(i)),
                    RowClick::Menu(resp) => {
                        resp.context_menu(|ui| {
                            if ui.button("🗀  Project files").clicked() {
                                acts.push(Action::SwitchView(i, View::Files));
                                ui.close();
                            }
                            if ui.button("⎇  Git").clicked() {
                                acts.push(Action::SwitchView(i, View::Git));
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("⏏  Unload from memory (move to Recent)").clicked() {
                                acts.push(Action::CloseProject(i));
                                ui.close();
                            }
                            if ui.button("Copy Path").clicked() {
                                acts.push(Action::Copy(p.root.display().to_string()));
                                ui.close();
                            }
                            if ui.button("Reveal in Explorer").clicked() {
                                acts.push(Action::Reveal(p.root.clone()));
                                ui.close();
                            }
                            if ui.button("Open Terminal").clicked() {
                                acts.push(Action::Terminal(p.root.clone()));
                                ui.close();
                            }
                        });
                    }
                }
            }

            if !recent.is_empty() {
                ui.add_space(8.0);
                if collapsed {
                    ui.separator();
                } else {
                    ui.horizontal(|ui| {
                        ui.add_space(4.0);
                        ui.label(RichText::new("RECENT").small().strong().color(pal.text_weak));
                    });
                }
                for (k, r) in recent.iter().enumerate() {
                    let path = PathBuf::from(r);
                    let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or(r.clone());
                    let parent = path.parent().map(|p| p.display().to_string()).unwrap_or_default();
                    let ts = last_active(r);
                    let parent = if ts > 0 { format!("{} · {}", util::fmt_ago(ts), parent) } else { parent };
                    let row = ProjectRow {
                        name: &name,
                        path: &path,
                        status: &parent,
                        status_color: pal.text_weak.gamma_multiply(0.8),
                        folder: if self.settings.dark { FOLDER_GREY_DARK } else { FOLDER_GREY_LIGHT },
                        lit: false,
                        active: false,
                        badge: None,
                        unload: false,
                    };
                    match project_row(ui, pal, collapsed, row, ("recent", k)) {
                        RowClick::Main | RowClick::Files => acts.push(Action::OpenProjectView(path.clone(), View::Files)),
                        RowClick::Git => acts.push(Action::OpenProjectView(path.clone(), View::Git)),
                        RowClick::Unload => {}
                        RowClick::Menu(resp) => {
                            resp.context_menu(|ui| {
                                if ui.button("🗀  Open — project files").clicked() {
                                    acts.push(Action::OpenProjectView(path.clone(), View::Files));
                                    ui.close();
                                }
                                if ui.button("⎇  Open — git").clicked() {
                                    acts.push(Action::OpenProjectView(path.clone(), View::Git));
                                    ui.close();
                                }
                                if ui.button("Copy Path").clicked() {
                                    acts.push(Action::Copy(path.display().to_string()));
                                    ui.close();
                                }
                                if ui.button("Remove from Recent").clicked() {
                                    acts.push(Action::ForgetRecent(r.clone()));
                                    ui.close();
                                }
                            });
                        }
                    }
                }
            }
        });
    }

    fn toolbar(&mut self, ui: &mut Ui, pal: &Palette, acts: &mut Vec<Action>) {
        ui.horizontal_centered(|ui| {
            ui.add_space(4.0);
            ui.label(RichText::new("⚡").color(pal.warn).size(16.0));
            let title = self.projects.get(self.active).map(|p| p.name.clone()).unwrap_or_else(|| "No project".into());
            ui.menu_button(RichText::new(format!("{title} ▾")).strong(), |ui| {
                if ui.button("Open Project Folder…").clicked() {
                    acts.push(Action::AddProjectDialog);
                    ui.close();
                }
                if !self.projects.is_empty() && ui.button("Close This Project").clicked() {
                    acts.push(Action::CloseProject(self.active));
                    ui.close();
                }
                ui.separator();
                ui.label(RichText::new("Recent").small().color(pal.text_weak));
                for r in self.settings.recent.iter().take(15) {
                    if ui.button(r).clicked() {
                        acts.push(Action::OpenProject(PathBuf::from(r)));
                        ui.close();
                    }
                }
            });
            let Some(p) = self.projects.get_mut(self.active) else { return };
            if p.git.status.is_repo {
                let b = p.git.status.branch.clone();
                ui.menu_button(format!("⎇ {b} ▾"), |ui| {
                    if ui.button("+ New Branch…").clicked() {
                        acts.push(Action::Prompt(PromptKind::NewBranch));
                        ui.close();
                    }
                    ui.separator();
                    ui.label(RichText::new("Local").small().color(pal.text_weak));
                    egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                        for lb in p.git.local_branches.clone() {
                            let cur = lb == b;
                            if ui.selectable_label(cur, &lb).clicked() && !cur {
                                p.git.checkout(&lb, false);
                                ui.close();
                            }
                        }
                        if !p.git.remote_branches.is_empty() {
                            ui.separator();
                            ui.label(RichText::new("Remote").small().color(pal.text_weak));
                            for rb in p.git.remote_branches.clone() {
                                if ui.button(&rb).clicked() {
                                    p.git.checkout(&rb, true);
                                    ui.close();
                                }
                            }
                        }
                    });
                });
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(6.0);
                if ui.button("⚙").on_hover_text("Settings").clicked() {
                    self.show_settings = true;
                }
                if ui
                    .button(if self.settings.dark { "☀" } else { "🌙" })
                    .on_hover_text("Toggle light / dark theme")
                    .clicked()
                {
                    self.settings.dark = !self.settings.dark;
                    theme::apply(ui.ctx(), self.settings.dark, self.settings.ui_font_size);
                    ui.ctx().send_viewport_cmd(ViewportCommand::SetTheme(if self.settings.dark { egui::SystemTheme::Dark } else { egui::SystemTheme::Light }));
                    self.settings.save();
                }
                if ui.button(">_").on_hover_text("Open terminal in project").clicked() {
                    acts.push(Action::Terminal(p.root.clone()));
                }
            });
        });
    }

    fn stripe_button(ui: &mut Ui, pal: &Palette, icon: &str, tip: &str, on: bool) -> bool {
        Self::stripe_button_col(ui, pal, icon, tip, on, None, 0)
    }

    /// `badge` > 0 draws a counter pill on the icon's top-right corner.
    fn stripe_button_col(
        ui: &mut Ui,
        pal: &Palette,
        icon: &str,
        tip: &str,
        on: bool,
        color: Option<Color32>,
        badge: usize,
    ) -> bool {
        let (rect, resp) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::click());
        if on {
            ui.painter().rect_filled(rect, 6.0, pal.selection);
        } else if resp.hovered() {
            ui.painter().rect_filled(rect, 6.0, pal.hover);
        }
        let col = color.unwrap_or(if on { pal.text } else { pal.text_weak });
        ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon, FontId::proportional(15.0), col);
        if badge > 0 {
            let text = if badge > 99 { "99+".to_owned() } else { badge.to_string() };
            let g = ui.painter().layout_no_wrap(text, FontId::proportional(9.5), Color32::WHITE);
            let w = (g.size().x + 7.0).max(14.0);
            let pill = egui::Rect::from_min_size(pos2(rect.max.x - w + 3.0, rect.min.y - 2.0), vec2(w, 14.0));
            ui.painter().rect_filled(pill.expand(1.5), 8.5, pal.panel_bg);
            ui.painter().rect_filled(pill, 7.0, pal.accent);
            ui.painter().galley(pill.center() - g.size() / 2.0, g, Color32::WHITE);
        }
        resp.on_hover_text(tip).clicked()
    }

    fn status_bar(&mut self, ui: &mut Ui, pal: &Palette) {
        let now = ui.input(|i| i.time);
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 14.0;
            let Some(p) = self.projects.get_mut(self.active) else {
                ui.label(RichText::new("VibeZen").small().color(pal.text_weak));
                return;
            };
            if !p.git.busy.is_empty() {
                let spin = ["◐", "◓", "◑", "◒"][((now * 6.0) as usize) % 4];
                ui.label(RichText::new(format!("{spin} {}…", p.git.busy.join(", "))).small().color(pal.accent));
            } else if let Some(e) = p.git.last_error.clone() {
                let r = ui.add(
                    egui::Label::new(RichText::new(format!("✖ {}", util::truncate(e.lines().next().unwrap_or(""), 120))).small().color(pal.error))
                        .sense(Sense::click()),
                );
                if r.on_hover_text(format!("{e}\n\n(click to dismiss)")).clicked() {
                    p.git.last_error = None;
                }
            } else if let Some((m, err)) = self.message.clone() {
                let r = ui.add(egui::Label::new(RichText::new(m).small().color(if err { pal.error } else { pal.text_weak })).sense(Sense::click()));
                if r.clicked() {
                    self.message = None;
                }
            } else if let Some(i) = &p.git.last_info {
                ui.label(RichText::new(util::truncate(i, 120)).small().color(pal.text_weak));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(8.0);
                let c = p.status_chat();
                let (ct, cc) = if c.pending_approvals() > 0 {
                    ("Claude: needs approval".to_owned(), pal.warn)
                } else if c.busy {
                    (format!("Claude: working {}", util::fmt_duration(c.busy_since.map(|t| t.elapsed().as_secs()).unwrap_or(0))), pal.accent)
                } else {
                    ("Claude: idle".to_owned(), pal.text_weak)
                };
                let c = c.uid;
                if ui.add(egui::Label::new(RichText::new(ct).small().color(cc)).sense(Sense::click())).clicked() {
                    let i = p.chats.iter().position(|x| x.uid == c).unwrap_or(p.active_chat);
                    p.show_chat(i);
                }
                if p.git.status.is_repo {
                    let s = &p.git.status;
                    let mut t = format!("⎇ {}", s.branch);
                    if s.ahead > 0 {
                        t.push_str(&format!(" ↑{}", s.ahead));
                    }
                    if s.behind > 0 {
                        t.push_str(&format!(" ↓{}", s.behind));
                    }
                    ui.label(RichText::new(t).small().color(pal.text_weak));
                }
                if let Some(d) = p.active_doc.and_then(|i| p.docs.get(i)) {
                    if d.kind == editor::DocKind::Text {
                        ui.label(RichText::new("UTF-8").small().color(pal.text_weak));
                        ui.label(RichText::new(d.line_ending()).small().color(pal.text_weak));
                        ui.label(RichText::new(format!("{}:{}", d.cursor.0, d.cursor.1)).small().color(pal.text_weak));
                        if let Some(s) = &d.syntax {
                            ui.label(RichText::new(s).small().color(pal.text_weak));
                        }
                    }
                }
            });
        });
    }

    fn tabs_bar(&mut self, ui: &mut Ui, pal: &Palette, acts: &mut Vec<Action>) {
        let Some(p) = self.projects.get_mut(self.active) else { return };
        let font = FontId::proportional(13.0);
        // natural width of every tab (chats, then files), shrunk together to fit the bar
        let text_w = |ui: &Ui, s: &str| ui.fonts_mut(|f| f.layout_no_wrap(s.to_owned(), font.clone(), pal.text).size().x);
        let chat_labels: Vec<String> = p
            .chats
            .iter()
            .map(|c| c.title().as_deref().map(|t| util::truncate(t, 28)).unwrap_or_else(|| "New session".into()))
            .collect();
        let chat_pad = |c: &Chat| if c.pending_approvals() > 0 || c.busy || c.unseen { 80.0 } else { 62.0 };
        let mut widths: Vec<f32> = p
            .chats
            .iter()
            .zip(&chat_labels)
            .map(|(c, l)| text_w(ui, l) + chat_pad(c))
            .chain(p.docs.iter().map(|d| text_w(ui, &d.title) + 62.0))
            .collect();
        // room for the "new session" button after the chat tabs
        const NEW_BTN_W: f32 = 30.0;
        fit_tabs(&mut widths, ui.available_width() - NEW_BTN_W, 96.0);
        // text cut to what's left of the tab after icon, marks and close button
        let tab_text = |ui: &Ui, s: &str, color: Color32, max_w: f32| {
            let mut job = egui::text::LayoutJob::simple_singleline(s.to_owned(), font.clone(), color);
            job.wrap = egui::text::TextWrapping::truncate_at_width(max_w.max(12.0));
            ui.fonts_mut(|f| f.layout_job(job))
        };
        egui::ScrollArea::horizontal()
            .id_salt("tabs")
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    let mut widths = widths.into_iter();
                    // Claude Code tabs, one per conversation, before the files
                    for (i, c) in p.chats.iter().enumerate() {
                        let active = p.active_doc.is_none() && p.active_chat == i;
                        let (mark, mcol) = if c.pending_approvals() > 0 {
                            ("⚠", pal.warn)
                        } else if c.busy {
                            (["◐", "◓", "◑", "◒"][((ui.input(|i| i.time) * 6.0) as usize) % 4], pal.accent)
                        } else if c.unseen {
                            ("●", FOLDER_DONE)
                        } else {
                            ("", pal.text_weak)
                        };
                        let title = c.title();
                        let w = widths.next().unwrap_or(96.0);
                        let label = tab_text(ui, &chat_labels[i], pal.text, w - chat_pad(c));
                        let (rect, resp) = ui.allocate_exact_size(vec2(w, 32.0), Sense::click());
                        let painter = ui.painter();
                        if active {
                            painter.rect_filled(rect, 0.0, pal.editor_bg);
                            painter.rect_filled(egui::Rect::from_min_max(pos2(rect.min.x, rect.max.y - 2.0), rect.max), 0.0, pal.accent);
                        } else if resp.hovered() {
                            painter.rect_filled(rect, 0.0, pal.hover);
                        }
                        painter.text(pos2(rect.min.x + 18.0, rect.center().y), Align2::CENTER_CENTER, "✻", FontId::proportional(15.0), claude_ui::CLAUDE);
                        painter.galley(pos2(rect.min.x + 32.0, rect.center().y - label.size().y / 2.0), label, pal.text);
                        if !mark.is_empty() {
                            painter.text(pos2(rect.max.x - 32.0, rect.center().y), Align2::CENTER_CENTER, mark, FontId::proportional(12.0), mcol);
                        }
                        let close_r = egui::Rect::from_center_size(pos2(rect.max.x - 14.0, rect.center().y), vec2(16.0, 16.0));
                        let close_hover = ui.rect_contains_pointer(close_r);
                        if resp.hovered() || active {
                            if close_hover {
                                painter.rect_filled(close_r, 3.0, pal.hover);
                            }
                            theme::paint_glyph(painter, close_r.center(), "×", FontId::proportional(14.0), pal.text_weak);
                        }
                        painter.line_segment([rect.right_top(), rect.right_bottom()], Stroke::new(1.0, pal.border.gamma_multiply(0.5)));
                        let resp = resp.on_hover_text(format!("{} (Alt+2)", title.as_deref().unwrap_or("New Claude Code session")));
                        if resp.clicked() {
                            if close_hover {
                                acts.push(Action::CloseChat(i));
                            } else {
                                acts.push(Action::ShowChat(i));
                            }
                        }
                        if resp.middle_clicked() {
                            acts.push(Action::CloseChat(i));
                        }
                        resp.context_menu(|ui| {
                            if ui.button("Close").clicked() {
                                acts.push(Action::CloseChat(i));
                                ui.close();
                            }
                            if ui.button("New session").clicked() {
                                acts.push(Action::NewSession);
                                ui.close();
                            }
                        });
                    }
                    // new Claude session, right after the last chat tab
                    let (rect, resp) = ui.allocate_exact_size(vec2(NEW_BTN_W, 32.0), Sense::click());
                    let br = egui::Rect::from_center_size(rect.center(), vec2(22.0, 22.0));
                    if resp.hovered() {
                        ui.painter().rect_filled(br, 4.0, pal.hover);
                    }
                    theme::paint_glyph(ui.painter(), br.center(), "✚", FontId::proportional(13.0), pal.text_weak);
                    ui.painter().line_segment([rect.right_top(), rect.right_bottom()], Stroke::new(1.0, pal.border.gamma_multiply(0.5)));
                    if resp.on_hover_text("New Claude Code session").clicked() {
                        acts.push(Action::NewSession);
                    }
                    for (i, d) in p.docs.iter().enumerate() {
                        let active = p.active_doc == Some(i);
                        let name_col = p
                            .git
                            .status_map
                            .get(&util::rel_path(&p.root, &d.path))
                            .map(|s| s.color(pal))
                            .unwrap_or(pal.text);
                        let w = widths.next().unwrap_or(96.0);
                        let label = tab_text(ui, &d.title, name_col, w - 62.0);
                        let (rect, resp) = ui.allocate_exact_size(vec2(w, 32.0), Sense::click());
                        let painter = ui.painter();
                        if active {
                            painter.rect_filled(rect, 0.0, pal.editor_bg);
                            painter.rect_filled(
                                egui::Rect::from_min_max(pos2(rect.min.x, rect.max.y - 2.0), rect.max),
                                0.0,
                                pal.accent,
                            );
                        } else if resp.hovered() {
                            painter.rect_filled(rect, 0.0, pal.hover);
                        }
                        let (badge, bcol) = if d.is_virtual() { ("Δ", pal.hunk) } else { file_badge(&d.title) };
                        let br = egui::Rect::from_min_size(pos2(rect.min.x + 10.0, rect.center().y - 7.0), vec2(16.0, 14.0));
                        painter.rect_filled(br, 3.0, bcol);
                        painter.text(br.center(), Align2::CENTER_CENTER, badge, FontId::proportional(9.5), Color32::WHITE);
                        painter.galley(pos2(br.max.x + 6.0, rect.center().y - label.size().y / 2.0), label, name_col);
                        let close_r = egui::Rect::from_center_size(pos2(rect.max.x - 14.0, rect.center().y), vec2(16.0, 16.0));
                        let close_hover = ui.rect_contains_pointer(close_r);
                        if d.dirty && !close_hover {
                            painter.circle_filled(close_r.center(), 3.5, pal.text_weak);
                        } else if resp.hovered() || active {
                            if close_hover {
                                painter.rect_filled(close_r, 3.0, pal.hover);
                            }
                            theme::paint_glyph(painter, close_r.center(), "×", FontId::proportional(14.0), pal.text_weak);
                        }
                        painter.line_segment([rect.right_top(), rect.right_bottom()], Stroke::new(1.0, pal.border.gamma_multiply(0.5)));
                        let resp = resp.on_hover_text(d.path.display().to_string());
                        if resp.clicked() {
                            if close_hover {
                                acts.push(Action::CloseTab(i));
                            } else {
                                p.active_doc = Some(i);
                            }
                        }
                        if resp.middle_clicked() {
                            acts.push(Action::CloseTab(i));
                        }
                        resp.context_menu(|ui| {
                            if ui.button("Close").clicked() {
                                acts.push(Action::CloseTab(i));
                                ui.close();
                            }
                            if ui.button("Close Others").clicked() {
                                acts.push(Action::CloseOthers(i));
                                ui.close();
                            }
                            if !d.is_virtual() {
                                if ui.button("Copy Path").clicked() {
                                    acts.push(Action::Copy(d.path.display().to_string()));
                                    ui.close();
                                }
                                if ui.button("Reveal in Explorer").clicked() {
                                    acts.push(Action::Reveal(d.path.clone()));
                                    ui.close();
                                }
                                if ui.button("🤖 Ask Claude about this file").clicked() {
                                    acts.push(Action::AskClaude(util::rel_path(&p.root, &d.path)));
                                    ui.close();
                                }
                            }
                        });
                    }
                });
            });
    }

    /// Left tool window: plan usage, new session, all past sessions of the project.
    fn claude_panel(&mut self, ui: &mut Ui, pal: &Palette) {
        // account-wide figures from Anthropic (every client, like Claude Code's /usage);
        // until they load, the freshest rate_limit_event from any of our projects
        let (mut limits, plan_error, plan_fetched) = {
            let s = self.plan.state.lock().unwrap();
            (s.limits.clone(), s.error.clone(), s.fetched)
        };
        let plan_busy = self.plan.busy();
        if plan_fetched.is_none() {
            for r in self.projects.iter().flat_map(|p| &p.chats).flat_map(|c| c.rate_limits.values()) {
                match limits.iter_mut().find(|l| l.kind == r.kind) {
                    Some(l) if l.at < r.at => *l = r.clone(),
                    Some(_) => {}
                    None => limits.push(r.clone()),
                }
            }
        }
        limits.sort_by_key(|l| plan::order(&l.kind));
        let total_cost: f64 = self.projects.iter().flat_map(|p| &p.chats).map(|c| c.usage.cost_usd).sum();
        let multi = self.projects.len() > 1;
        let p = &mut self.projects[self.active];

        ui.horizontal(|ui| {
            ui.label(RichText::new("✻").color(claude_ui::CLAUDE).size(16.0));
            ui.label(RichText::new("Claude Code").strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("«").on_hover_text("Hide panel").clicked() {
                    self.left = None;
                }
            });
        });
        ui.add_space(6.0);
        let new_btn = egui::Button::new(RichText::new("✚  New session").color(Color32::WHITE)).fill(pal.accent);
        if ui.add_sized(vec2(ui.available_width(), 28.0), new_btn).clicked() {
            p.new_session();
        }

        ui.add_space(10.0);
        let mut refresh_plan = false;
        ui.horizontal(|ui| {
            ui.label(RichText::new("USAGE").small().strong().color(pal.text_weak))
                .on_hover_text("Plan limits for the whole account: every Claude Code session, IDE and claude.ai");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if plan_busy {
                    ui.add(egui::Spinner::new().size(12.0));
                } else if ui.small_button("⟳").on_hover_text("Refresh usage").clicked() {
                    refresh_plan = true;
                }
                if let Some(t) = plan_fetched {
                    let s = t.elapsed().as_secs();
                    let ago = if s < 60 { "just now".to_owned() } else { format!("{} min ago", s / 60) };
                    ui.label(RichText::new(format!("updated {ago}")).small().color(pal.text_weak));
                }
            });
        });
        ui.add_space(2.0);
        if let Some(e) = &plan_error {
            ui.add(egui::Label::new(RichText::new(format!("Account usage unavailable: {e}")).small().color(pal.warn)).wrap());
        }
        if limits.is_empty() && !plan_busy {
            ui.label(RichText::new("Plan limits show up after the next reply from Claude.").small().color(pal.text_weak));
        }
        let now = util::unix_now();
        for l in &limits {
            let name = plan::label(&l.kind);
            let col = match l.status.as_str() {
                "rejected" => pal.error,
                "allowed_warning" => pal.warn,
                _ => pal.accent,
            };
            ui.horizontal(|ui| {
                ui.label(RichText::new(name).small());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let t = match (l.utilization, l.status.as_str()) {
                        (Some(u), _) => format!("{:.0}%", u * 100.0),
                        (None, "rejected") => "limit reached".into(),
                        (None, "allowed_warning") => "close to limit".into(),
                        _ => "ok".into(),
                    };
                    ui.label(RichText::new(t).small().color(col));
                });
            });
            let frac = l.utilization.unwrap_or(if l.status == "rejected" { 1.0 } else { 0.0 }) as f32;
            let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 5.0), Sense::hover());
            ui.painter().rect_filled(r, 2.5, pal.hover);
            ui.painter().rect_filled(egui::Rect::from_min_size(r.min, vec2(r.width() * frac.clamp(0.0, 1.0), r.height())), 2.5, col);
            if let Some(t) = l.resets_at.filter(|t| *t > now) {
                let s = t - now;
                let left = if s >= 86400 { format!("{} d {} h", s / 86400, (s / 3600) % 24) } else { format!("{} h {} min", s / 3600, (s / 60) % 60) };
                ui.label(RichText::new(format!("resets in {left}")).small().color(pal.text_weak));
            }
            ui.add_space(4.0);
        }
        if refresh_plan {
            self.plan.refresh(ui.ctx());
        }
        let p = &mut self.projects[self.active];
        let u = p.chat().usage;
        let k = |n: u64| if n >= 1000 { format!("{:.1}k", n as f64 / 1000.0) } else { n.to_string() };
        ui.add(
            egui::Label::new(
                RichText::new(format!(
                    "This conversation: ${:.2} · {} turn{} · {} in / {} out · {} cached",
                    u.cost_usd,
                    u.turns,
                    if u.turns == 1 { "" } else { "s" },
                    k(u.input_tokens),
                    k(u.output_tokens),
                    k(u.cache_read_tokens)
                ))
                .small()
                .color(pal.text_weak),
            )
            .wrap(),
        );
        if multi {
            ui.label(RichText::new(format!("All open projects: ${total_cost:.2}")).small().color(pal.text_weak));
        }

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("SESSIONS").small().strong().color(pal.text_weak));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("⟳").on_hover_text("Refresh").clicked() {
                    p.chat_mut().history_at = None;
                }
            });
        });
        if p.chat().history_at.is_none_or(|t| t.elapsed().as_secs() > 15) {
            let h = crate::claude::list_sessions(&p.root);
            let c = p.chat_mut();
            c.history = h;
            c.history_at = Some(std::time::Instant::now());
        }
        if p.chat().history.is_empty() {
            ui.label(RichText::new("No saved conversations for this folder").small().color(pal.text_weak));
        }
        let mut pick = None;
        egui::ScrollArea::vertical().id_salt("claude_sessions").auto_shrink([false, false]).show(ui, |ui| {
            for s in &p.chat().history {
                let current = p.active_doc.is_none() && p.chat().holds_session(&s.id);
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::click());
                if current {
                    ui.painter().rect_filled(rect, 5.0, pal.selection);
                } else if resp.hovered() {
                    ui.painter().rect_filled(rect, 5.0, pal.hover);
                }
                let pc = ui.painter().with_clip_rect(rect.shrink2(vec2(8.0, 0.0)));
                pc.text(pos2(rect.min.x + 8.0, rect.min.y + 12.0), Align2::LEFT_CENTER, &s.title, FontId::proportional(13.0), pal.text);
                pc.text(pos2(rect.min.x + 8.0, rect.min.y + 29.0), Align2::LEFT_CENTER, util::fmt_ago(s.modified), FontId::proportional(11.0), pal.text_weak);
                if resp.on_hover_text(&s.title).clicked() {
                    pick = Some(s.id.clone());
                }
            }
        });
        if let Some(id) = pick {
            p.open_session(&id);
        }
    }

    fn find_panel(&mut self, ui: &mut Ui, pal: &Palette, acts: &mut Vec<Action>) {
        let ctx = ui.ctx().clone();
        let Some(p) = self.projects.get_mut(self.active) else { return };
        let root = p.root.clone();
        let f = &mut p.find;
        let id = Id::new(("fif", root.clone()));
        let mut changed = false;
        let mut run_now = false;
        let mut open_sel = false;
        let mut close = false;

        {
            let r = ui.add(
                egui::TextEdit::singleline(&mut f.query)
                    .id(id)
                    .hint_text("Find in project…")
                    .desired_width(f32::INFINITY),
            )
            .on_hover_text("Enter: open  ·  ↑↓: navigate  ·  Esc: close");
            if f.focus {
                r.request_focus();
                f.focus = false;
            }
            changed |= r.changed();
            if r.has_focus() {
                ui.input_mut(|i| {
                    if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                        f.step(1);
                    }
                    if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                        f.step(-1);
                    }
                    if i.consume_key(Modifiers::NONE, Key::Escape) {
                        close = true;
                    }
                });
            }
            if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                if f.up_to_date() && !f.rows.is_empty() && !f.running {
                    open_sel = true;
                } else {
                    run_now = true;
                    r.request_focus();
                }
            }
        }
        ui.horizontal(|ui| {
            for (val, label, tip) in [
                (&mut f.case, "Aa", "Match case"),
                (&mut f.word, "W", "Whole words"),
                (&mut f.regex, ".*", "Regular expression"),
            ] {
                if ui.selectable_label(*val, RichText::new(label).monospace().strong()).on_hover_text(tip).clicked() {
                    *val = !*val;
                    changed = true;
                }
            }
            ui.add_space(4.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("⟳").on_hover_text("Search again").clicked() {
                    run_now = true;
                }
                let m = ui.add(
                    egui::TextEdit::singleline(&mut f.mask)
                        .hint_text("File mask: *.rs, *.ts, src/")
                        .desired_width(ui.available_width()),
                );
                changed |= m.changed();
                if m.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    run_now = true;
                }
            });
        });
        if let Some(e) = &f.error {
            ui.label(RichText::new(format!("✖ {e}")).color(pal.error));
        } else if f.running {
            ui.label(RichText::new(format!("Searching…  {} matches", f.results.len())).color(pal.accent));
        } else if let Some(q) = &f.searched {
            let more = if f.truncated { " (showing first 5000)" } else { "" };
            ui.label(
                RichText::new(format!(
                    "{} matches in {} files for “{}”{more}  ·  {} files searched",
                    f.results.len(),
                    f.file_count(),
                    util::truncate(q, 40),
                    f.scanned
                ))
                .color(pal.text_weak)
                .size(11.5),
            );
        }

        // search as you type (debounced)
        if changed {
            f.dirty_at = Some(std::time::Instant::now());
        }
        if let Some(t) = f.dirty_at {
            let wait = Duration::from_millis(280).saturating_sub(t.elapsed());
            if wait.is_zero() {
                run_now = true;
            } else {
                ctx.request_repaint_after(wait);
            }
        }
        if run_now {
            f.start(root.clone(), ctx.clone());
        }
        if open_sel {
            if let Some(h) = f.selected_hit() {
                acts.push(Action::OpenFileAt { path: root.join(&h.path), line: h.line, col: h.col, len: h.len });
            }
        }

        ui.separator();
        if f.rows.is_empty() {
            let hint = if f.running {
                "Searching…"
            } else if f.searched.is_some() {
                "Nothing found."
            } else {
                "Type to search in all project files (node_modules, target, .git… are skipped).\nAa = match case · W = whole words · .* = regex · mask limits files, e.g. *.php, src/"
            };
            ui.add_space(6.0);
            ui.label(RichText::new(hint).color(pal.text_weak));
        }

        let row_h = 20.0;
        let step = row_h + ui.spacing().item_spacing.y;
        let mut sa = egui::ScrollArea::vertical().id_salt(("fif_results", root.clone())).auto_shrink([false, false]);
        if f.scroll_to_sel {
            if let Some(s) = f.sel {
                let view = ui.available_height();
                sa = sa.vertical_scroll_offset((s as f32 * step - view / 2.0).max(0.0));
            }
            f.scroll_to_sel = false;
        }
        let mut clicked: Option<usize> = None;
        let mono = FontId::monospace(12.5);
        sa.show_rows(ui, row_h, f.rows.len(), |ui, range| {
            for ri in range {
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
                let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
                match &f.rows[ri] {
                    search::Row::File { path, count } => {
                        if resp.hovered() {
                            painter.rect_filled(rect, 3.0, pal.hover);
                        }
                        let (dir, name) = match path.rfind('/') {
                            Some(i) => (&path[..i], &path[i + 1..]),
                            None => ("", path.as_str()),
                        };
                        let (badge, bcol) = file_badge(name);
                        let mut x = rect.min.x + 6.0;
                        let bg = painter.layout_no_wrap(badge.to_owned(), FontId::proportional(10.0), bcol);
                        painter.galley(pos2(x, rect.center().y - bg.size().y / 2.0), bg.clone(), bcol);
                        x += bg.size().x.max(12.0) + 6.0;
                        let ng = painter.layout_no_wrap(name.to_owned(), FontId::proportional(13.0), pal.text);
                        painter.galley(pos2(x, rect.center().y - ng.size().y / 2.0), ng.clone(), pal.text);
                        x += ng.size().x + 10.0;
                        let info = if dir.is_empty() { format!("{count}") } else { format!("{dir}   {count}") };
                        painter.text(pos2(x, rect.center().y), Align2::LEFT_CENTER, info, FontId::proportional(12.0), pal.text_weak);
                        if resp.clicked() {
                            acts.push(Action::OpenFile(root.join(path), None));
                        }
                    }
                    search::Row::Hit(hi) => {
                        let h = &f.results[*hi];
                        if f.sel == Some(ri) {
                            painter.rect_filled(rect, 3.0, pal.selection);
                        } else if resp.hovered() {
                            painter.rect_filled(rect, 3.0, pal.hover);
                        }
                        let num_right = rect.min.x + 44.0;
                        painter.text(
                            pos2(num_right, rect.center().y),
                            Align2::RIGHT_CENTER,
                            h.line.to_string(),
                            mono.clone(),
                            pal.gutter_text,
                        );
                        let normal = egui::TextFormat { font_id: mono.clone(), color: pal.text, ..Default::default() };
                        let hl = egui::TextFormat {
                            font_id: mono.clone(),
                            color: pal.text,
                            background: pal.find_match,
                            ..Default::default()
                        };
                        let mut job = egui::text::LayoutJob::default();
                        let mut pos = 0;
                        for &(a, b) in &h.ranges {
                            if a < pos || b > h.text.len() || !h.text.is_char_boundary(a) || !h.text.is_char_boundary(b) {
                                continue;
                            }
                            job.append(&h.text[pos..a], 0.0, normal.clone());
                            job.append(&h.text[a..b], 0.0, hl.clone());
                            pos = b;
                        }
                        job.append(&h.text[pos..], 0.0, normal);
                        let g = painter.layout_job(job);
                        painter.galley(pos2(num_right + 12.0, rect.center().y - g.size().y / 2.0), g, pal.text);
                        if resp.clicked() {
                            clicked = Some(ri);
                        }
                    }
                }
            }
        });
        if let Some(ri) = clicked {
            f.sel = Some(ri);
            if let search::Row::Hit(hi) = f.rows[ri] {
                let h = &f.results[hi];
                acts.push(Action::OpenFileAt { path: root.join(&h.path), line: h.line, col: h.col, len: h.len });
            }
        }
        if close {
            self.left = None;
        }
    }

    fn welcome(&mut self, ui: &mut Ui, pal: &Palette, acts: &mut Vec<Action>) {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.22);
            let logo = ui.ctx().data_mut(|d| d.get_temp::<egui::TextureHandle>(egui::Id::new("app-logo"))).unwrap_or_else(|| {
                let ic = util::app_icon();
                let ci = egui::ColorImage::from_rgba_unmultiplied([ic.width as usize, ic.height as usize], &ic.rgba);
                let tex = ui.ctx().load_texture("app-logo", ci, egui::TextureOptions::LINEAR);
                ui.ctx().data_mut(|d| d.insert_temp(egui::Id::new("app-logo"), tex.clone()));
                tex
            });
            ui.add(egui::Image::new(&logo).fit_to_exact_size(egui::vec2(72.0, 72.0)));
            ui.add_space(6.0);
            ui.label(RichText::new("VibeZen").size(30.0).color(pal.accent).strong());
            ui.label(RichText::new("Fast native editor with git and parallel Claude Code agents").color(pal.text_weak));
            ui.add_space(18.0);
            if ui.add(egui::Button::new(RichText::new("  Open Project Folder…  ").color(Color32::WHITE).size(15.0)).fill(pal.accent)).clicked() {
                acts.push(Action::AddProjectDialog);
            }
            ui.add_space(8.0);
            ui.label(RichText::new("…or drop a folder onto this window").small().color(pal.text_weak));
            ui.add_space(18.0);
            for r in self.settings.recent.iter().take(10) {
                if ui.link(r).clicked() {
                    acts.push(Action::OpenProject(PathBuf::from(r)));
                }
            }
        });
    }

    fn modals(&mut self, ctx: &egui::Context, pal: &Palette) {
        // ---- prompt
        if let Some(pr) = &mut self.prompt {
            // where a new file / folder lands, relative to the project
            let into = |dir: &Path| {
                let rel = self.projects.get(self.active).map(|p| util::rel_path(&p.root, dir)).unwrap_or_default();
                if rel.is_empty() { String::new() } else { format!(" in {rel}/") }
            };
            let title = match &pr.kind {
                PromptKind::NewFile(d) => format!("New File{}", into(d)),
                PromptKind::NewFolder(d) => format!("New Folder{}", into(d)),
                PromptKind::Rename(_) => "Rename".into(),
                PromptKind::GotoLine => "Go to Line[:Column]".into(),
                PromptKind::NewBranch => "New Branch".into(),
            };
            let mut done = None;
            let resp = egui::Modal::new(Id::new("prompt")).show(ctx, |ui| {
                ui.set_width(380.0);
                ui.label(RichText::new(title).strong());
                let r = ui.add(egui::TextEdit::singleline(&mut pr.value).desired_width(f32::INFINITY));
                if pr.focus {
                    r.request_focus();
                    pr.focus = false;
                }
                if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    done = Some(true);
                }
                ui.horizontal(|ui| {
                    if ui.button("OK").clicked() {
                        done = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        done = Some(false);
                    }
                });
            });
            if resp.should_close() && done.is_none() {
                done = Some(false);
            }
            if let Some(ok) = done {
                let pr = self.prompt.take().unwrap();
                if ok && !pr.value.trim().is_empty() {
                    self.apply_prompt(pr);
                }
            }
        }
        // ---- confirm
        if let Some(c) = &self.confirm {
            let (title, body, ok_label): (String, String, &str) = match c {
                Confirm::Delete(p) => ("Delete".into(), format!("Delete {}?\nThis cannot be undone.", p.display()), "Delete"),
                Confirm::Rollback(f) => ("Rollback changes".into(), format!("Discard local changes in {} file(s)?\nUntracked files will be deleted.", f.len()), "Rollback"),
                Confirm::CloseTab(i) => {
                    let name = self.projects.get(self.active).and_then(|p| p.docs.get(*i)).map(|d| d.title.clone()).unwrap_or_default();
                    ("Unsaved changes".into(), format!("Save changes to {name}?"), "Save")
                }
                Confirm::CloseChat(i) => {
                    let name = self.projects.get(self.active).and_then(|p| p.chats.get(*i)).and_then(|c| c.title()).unwrap_or_else(|| "this conversation".into());
                    ("Close conversation".into(), format!("Claude is still working on \"{name}\".\nClosing the tab stops it."), "Close")
                }
                Confirm::CloseProject(i) => {
                    let p = &self.projects[*i];
                    ("Close project".into(), format!("Close {}?{}{}", p.name, if p.dirty_count() > 0 { format!("\n{} unsaved file(s) will be lost.", p.dirty_count()) } else { String::new() }, if p.busy() { "\nIts running Claude tasks will be stopped." } else { "" }), "Close")
                }
                Confirm::Exit => {
                    let dirty: usize = self.projects.iter().map(|p| p.dirty_count()).sum();
                    let busy: usize = self.projects.iter().map(|p| p.chats.iter().filter(|c| c.busy).count()).sum();
                    ("Exit VibeZen".into(), format!("{}{}", if dirty > 0 { format!("{dirty} unsaved file(s).\n") } else { String::new() }, if busy > 0 { format!("{busy} Claude task(s) still running.") } else { String::new() }), "Save all & exit")
                }
            };
            let mut res: Option<u8> = None; // 1 = ok, 2 = alt, 0 = cancel
            let is_tab = matches!(c, Confirm::CloseTab(_));
            let is_exit = matches!(c, Confirm::Exit);
            let r = egui::Modal::new(Id::new("confirm")).show(ctx, |ui| {
                ui.set_width(400.0);
                ui.label(RichText::new(title).strong());
                ui.add_space(4.0);
                ui.label(body);
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let danger = matches!(self.confirm, Some(Confirm::Delete(_)) | Some(Confirm::Rollback(_)));
                    if ui.add(egui::Button::new(RichText::new(ok_label).color(Color32::WHITE)).fill(if danger { pal.error } else { pal.accent })).clicked() {
                        res = Some(1);
                    }
                    if (is_tab || is_exit) && ui.button(if is_tab { "Don't Save" } else { "Exit without saving" }).clicked() {
                        res = Some(2);
                    }
                    if ui.button("Cancel").clicked() {
                        res = Some(0);
                    }
                });
            });
            if r.should_close() && res.is_none() {
                res = Some(0);
            }
            if let Some(v) = res {
                let c = self.confirm.take().unwrap();
                if v > 0 {
                    self.apply_confirm(c, v == 1, ctx);
                }
            }
        }
        // ---- quick open
        let mut open_rel: Option<String> = None;
        let mut close_quick = false;
        if let (Some(q), Some(p)) = (&mut self.quick, self.projects.get(self.active)) {
            let files = p.index.files.lock().unwrap();
            if q.computed.as_deref() != Some(q.query.as_str()) {
                q.results = search::quick_matches(&files, &q.query, 40);
                q.computed = Some(q.query.clone());
                q.sel = 0;
            }
            drop(files);
            let r = egui::Modal::new(Id::new("quick")).show(ctx, |ui| {
                ui.set_width(560.0);
                ui.label(RichText::new("Go to File").strong());
                let input_id = Id::new("quick_input");
                if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowDown)) {
                    q.sel = (q.sel + 1).min(q.results.len().saturating_sub(1));
                }
                if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp)) {
                    q.sel = q.sel.saturating_sub(1);
                }
                let enter = ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
                let te = ui.add(egui::TextEdit::singleline(&mut q.query).id(input_id).hint_text("Type a file name…").desired_width(f32::INFINITY));
                if q.focus {
                    te.request_focus();
                    q.focus = false;
                }
                if enter {
                    open_rel = q.results.get(q.sel).cloned();
                }
                egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                    for (i, f) in q.results.iter().enumerate() {
                        let (dir, name) = match f.rfind('/') {
                            Some(k) => (&f[..k], &f[k + 1..]),
                            None => ("", f.as_str()),
                        };
                        let resp = ui.add(egui::Button::selectable(
                            i == q.sel,
                            RichText::new(format!("{name}   ")).color(pal.text),
                        ).right_text(RichText::new(dir).small().color(pal.text_weak)));
                        if i == q.sel {
                            resp.scroll_to_me(None);
                        }
                        if resp.clicked() {
                            open_rel = Some(f.clone());
                        }
                    }
                    if !*p.index.ready.lock().unwrap() {
                        ui.label(RichText::new("Indexing…").small().color(pal.text_weak));
                    }
                });
            });
            if r.should_close() {
                close_quick = true;
            }
        }
        if let Some(rel) = open_rel {
            self.quick = None;
            if let Some(root) = self.projects.get(self.active).map(|p| p.root.clone()) {
                self.open_file(&root.join(rel), None);
            }
        } else if close_quick {
            self.quick = None;
        }
        // ---- settings
        if self.show_settings {
            let mut close = false;
            let mut retheme = false;
            let r = egui::Modal::new(Id::new("settings")).show(ctx, |ui| {
                ui.set_width(420.0);
                ui.heading("Settings");
                ui.add_space(6.0);
                egui::Grid::new("settings_grid").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                    ui.label("Theme");
                    ui.horizontal(|ui| {
                        if ui.selectable_label(self.settings.dark, "🌙 Dark").clicked() {
                            self.settings.dark = true;
                            retheme = true;
                        }
                        if ui.selectable_label(!self.settings.dark, "☀ Light").clicked() {
                            self.settings.dark = false;
                            retheme = true;
                        }
                    });
                    ui.end_row();
                    ui.label("Editor font size");
                    ui.add(egui::Slider::new(&mut self.settings.editor_font_size, 9.0..=28.0).step_by(0.5));
                    ui.end_row();
                    ui.label("UI font size");
                    if ui.add(egui::Slider::new(&mut self.settings.ui_font_size, 11.0..=18.0).step_by(0.5)).drag_stopped() {
                        retheme = true;
                    }
                    ui.end_row();
                    ui.label("Editor");
                    ui.checkbox(&mut self.settings.auto_save, "Auto-save edited files").on_hover_text(
                        "Saves 1.5 s after you stop typing, when VibeZen loses focus and when you switch projects.
Files changed on disk meanwhile (e.g. by an agent) are not overwritten.",
                    );
                    ui.end_row();
                    ui.label("Notifications");
                    ui.vertical(|ui| {
                        ui.checkbox(&mut self.settings.notify, "Notify when Claude finishes / needs approval");
                        ui.checkbox(&mut self.settings.notify_only_unfocused, "Only when I'm not looking at that project");
                        ui.checkbox(&mut self.settings.notify_sound, "Play sound");
                        if ui.button("Send test notification").clicked() {
                            util::notify("VibeZen", "Notifications are working ✔", self.settings.notify_sound, None);
                        }
                    });
                    ui.end_row();
                    ui.label("New chats: model");
                    egui::ComboBox::from_id_salt("def_model")
                        .selected_text(MODELS.iter().find(|m| m.0 == self.settings.claude_model).map(|m| m.1).unwrap_or("Default"))
                        .show_ui(ui, |ui| {
                            for (id, l) in MODELS {
                                ui.selectable_value(&mut self.settings.claude_model, id.to_string(), *l);
                            }
                        });
                    ui.end_row();
                    ui.label("New chats: permissions");
                    egui::ComboBox::from_id_salt("def_mode")
                        .selected_text(MODES.iter().find(|m| m.0 == self.settings.claude_mode).map(|m| m.1).unwrap_or("Ask"))
                        .show_ui(ui, |ui| {
                            for (id, l, tip) in MODES {
                                ui.selectable_value(&mut self.settings.claude_mode, id.to_string(), *l).on_hover_text(*tip);
                            }
                        });
                    ui.end_row();
                });
                ui.add_space(8.0);
                ui.label(RichText::new("Shortcuts: Ctrl+Shift+N go to file · Ctrl+Shift+F find in files · Ctrl+F find · Ctrl+G go to line · Ctrl+S save all · Ctrl+K commit · Ctrl+Shift+K push · Ctrl+T pull · Alt+1 project · Alt+0 commit · Alt+9 git log · Alt+2 Claude tab · Alt+3 Claude sessions & usage · Ctrl+Alt+[ / ] switch project · Ctrl+± font size").small().color(pal.text_weak));
                ui.add_space(8.0);
                if ui.button("Close").clicked() {
                    close = true;
                }
            });
            if retheme {
                theme::apply(ctx, self.settings.dark, self.settings.ui_font_size);
                ctx.send_viewport_cmd(ViewportCommand::SetTheme(if self.settings.dark { egui::SystemTheme::Dark } else { egui::SystemTheme::Light }));
            }
            if close || r.should_close() {
                self.show_settings = false;
                self.settings.save();
            }
        }
    }

    fn apply_prompt(&mut self, pr: Prompt) {
        let v = pr.value.trim().to_owned();
        match pr.kind {
            PromptKind::NewFile(dir) => {
                let path = dir.join(&v);
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                    Ok(_) => {
                        if let Some(p) = self.proj() {
                            p.tree.refresh();
                            p.git.refresh_status();
                        }
                        self.open_file(&path, None);
                    }
                    Err(e) => self.message = Some((format!("Cannot create file: {e}"), true)),
                }
            }
            PromptKind::NewFolder(dir) => match std::fs::create_dir_all(dir.join(&v)) {
                Ok(_) => {
                    if let Some(p) = self.proj() {
                        p.tree.refresh();
                    }
                }
                Err(e) => self.message = Some((format!("Cannot create folder: {e}"), true)),
            },
            PromptKind::Rename(path) => {
                let new = path.parent().map(|d| d.join(&v)).unwrap_or_else(|| PathBuf::from(&v));
                match std::fs::rename(&path, &new) {
                    Ok(_) => {
                        let hl = &self.hl;
                        if let Some(p) = self.projects.get_mut(self.active) {
                            for d in &mut p.docs {
                                if d.path.starts_with(&path) {
                                    let rest = d.path.strip_prefix(&path).unwrap().to_path_buf();
                                    let np = if rest.as_os_str().is_empty() { new.clone() } else { new.join(rest) };
                                    *d = Doc::open(&np, hl);
                                }
                            }
                            p.tree.refresh();
                            p.git.refresh_status();
                        }
                    }
                    Err(e) => self.message = Some((format!("Rename failed: {e}"), true)),
                }
            }
            PromptKind::GotoLine => {
                let mut it = v.split(':');
                let line = it.next().and_then(|s| s.trim().parse::<usize>().ok()).unwrap_or(1);
                let col = it.next().and_then(|s| s.trim().parse::<usize>().ok()).unwrap_or(1);
                if let Some(p) = self.proj() {
                    if let Some(d) = p.active_doc.and_then(|i| p.docs.get_mut(i)) {
                        d.goto_line(line, col);
                    }
                }
            }
            PromptKind::NewBranch => {
                if let Some(p) = self.proj() {
                    p.git.new_branch(&v);
                }
            }
        }
    }

    fn apply_confirm(&mut self, c: Confirm, primary: bool, ctx: &egui::Context) {
        match c {
            Confirm::Delete(path) => {
                let r = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
                if let Err(e) = r {
                    self.message = Some((format!("Delete failed: {e}"), true));
                }
                if let Some(p) = self.proj() {
                    let to_close: Vec<usize> = p.docs.iter().enumerate().filter(|(_, d)| d.path.starts_with(&path)).map(|(i, _)| i).collect();
                    for i in to_close.into_iter().rev() {
                        p.docs.remove(i);
                    }
                    p.active_doc = if p.docs.is_empty() { None } else { Some(p.docs.len() - 1) };
                    p.tree.refresh();
                    p.git.refresh_status();
                }
            }
            Confirm::Rollback(files) => {
                if let Some(p) = self.proj() {
                    p.git.rollback(files);
                }
            }
            Confirm::CloseTab(i) => {
                if primary {
                    if let Some(p) = self.proj() {
                        if let Some(d) = p.docs.get_mut(i) {
                            if let Err(e) = d.save() {
                                self.message = Some((e, true));
                                return;
                            }
                        }
                    }
                }
                self.close_tab(i, true);
            }
            Confirm::CloseChat(i) => self.close_chat(i, true),
            Confirm::CloseProject(i) => self.close_project(i, true),
            Confirm::Exit => {
                if primary {
                    for p in &mut self.projects {
                        for d in &mut p.docs {
                            let _ = d.save();
                        }
                    }
                }
                self.allow_close = true;
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
        }
    }

    fn apply(&mut self, acts: Vec<Action>, ctx: &egui::Context) {
        for a in acts {
            match a {
                Action::OpenFile(p, line) => self.open_file(&p, line),
                // the choice applies to this conversation and becomes the default for new ones;
                // other open conversations keep theirs
                Action::ClaudeMode(m) => {
                    if let Some(p) = self.proj() {
                        if p.chat().mode != m {
                            p.chat_mut().set_mode(&m);
                        }
                    }
                    self.settings.claude_mode = m;
                    self.settings.save();
                }
                Action::ClaudeModel(m) => {
                    if let Some(p) = self.proj() {
                        if p.chat().model != m {
                            p.chat_mut().set_model(&m);
                        }
                    }
                    self.settings.claude_model = m;
                    self.settings.save();
                }
                Action::OpenFileAt { path, line, col, len } => {
                    self.open_file(&path, None);
                    if let Some(p) = self.projects.get_mut(self.active) {
                        if let Some(d) = p.active_doc.and_then(|i| p.docs.get_mut(i)) {
                            if d.path == path {
                                d.select_at(line, col, len);
                            }
                        }
                    }
                }
                Action::OpenDiff { title, key, text } => self.open_virtual(title, key, text),
                Action::Switch(i) => self.switch(i),
                Action::SwitchView(i, v) => {
                    self.switch(i);
                    self.show_view(v);
                }
                Action::OpenProjectView(p, v) => {
                    if p.is_dir() {
                        self.add_project(p, ctx, true);
                        self.show_view(v);
                    } else {
                        self.message = Some((format!("Folder not found: {}", p.display()), true));
                        self.settings.recent.retain(|r| PathBuf::from(r) != p);
                        self.settings.save();
                    }
                }
                Action::ForgetRecent(r) => {
                    self.settings.recent.retain(|x| x != &r);
                    self.settings.save();
                }
                Action::AddProjectDialog => {
                    if let Some(dir) = rfd::FileDialog::new().set_title("Open project folder").pick_folder() {
                        self.add_project(dir, ctx, true);
                    }
                }
                Action::OpenProject(p) => {
                    if p.is_dir() {
                        self.add_project(p, ctx, true);
                    } else {
                        self.message = Some((format!("Folder not found: {}", p.display()), true));
                        self.settings.recent.retain(|r| PathBuf::from(r) != p);
                    }
                }
                Action::CloseProject(i) => self.close_project(i, false),
                Action::CloseTab(i) => self.close_tab(i, false),
                Action::ShowChat(i) => {
                    if let Some(p) = self.proj() {
                        p.show_chat(i);
                    }
                }
                Action::CloseChat(i) => self.close_chat(i, false),
                Action::NewSession => {
                    if let Some(p) = self.proj() {
                        p.new_session();
                    }
                }
                Action::OpenSession(id) => {
                    if let Some(p) = self.proj() {
                        p.open_session(&id);
                    }
                }
                Action::CloseOthers(i) => {
                    if let Some(p) = self.proj() {
                        let keep = p.docs.remove(i);
                        let dirty: Vec<Doc> = p.docs.drain(..).filter(|d| d.dirty).collect();
                        p.docs = dirty;
                        p.docs.insert(0, keep);
                        p.active_doc = Some(0);
                    }
                }
                Action::Prompt(kind) => {
                    let value = match &kind {
                        PromptKind::Rename(p) => p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
                        _ => String::new(),
                    };
                    self.prompt = Some(Prompt { kind, value, focus: true });
                }
                Action::Confirm(c) => self.confirm = Some(c),
                Action::Terminal(p) => util::open_terminal(&p),
                Action::Reveal(p) => util::reveal_in_explorer(&p),
                Action::Copy(s) => ctx.copy_text(s),
                Action::AskClaude(rel) => {
                    if let Some(p) = self.proj() {
                        p.show_chat(p.active_chat);
                        let c = p.chat_mut();
                        if !c.input.is_empty() && !c.input.ends_with(' ') {
                            c.input.push(' ');
                        }
                        c.input.push_str(&format!("@{rel} "));
                    }
                }
                Action::SaveAll => self.save_all(),
            }
        }
    }
}


// ---------------------------------------------------------------- sidebar rows

const FOLDER_OPEN: Color32 = theme::hex(0x3D8BFF); // open project, agent idle: blue
const FOLDER_WORKING: Color32 = theme::hex(0xE8B04B); // agent working: amber (pulsing)
const FOLDER_DONE: Color32 = theme::hex(0x4CC38A); // agent finished: green
const FOLDER_ALERT: Color32 = theme::hex(0xF0524F); // agent asks a question / approval, or error: red
const FOLDER_GREY_DARK: Color32 = theme::hex(0x5C6068);
const FOLDER_GREY_LIGHT: Color32 = theme::hex(0xB4B8C0);

#[derive(Clone, Copy, PartialEq)]
enum View {
    Files,
    Git,
}

struct ProjectRow<'a> {
    name: &'a str,
    path: &'a Path,
    status: &'a str,
    status_color: Color32,
    folder: Color32,
    lit: bool,
    active: bool,
    badge: Option<&'static str>,
    /// show the ⏏ "unload from memory" quick button
    unload: bool,
}

enum RowClick {
    Main,
    Files,
    Git,
    Unload,
    Menu(egui::Response),
}

/// Shrink tab widths to fit `avail`, browser-style: the widest tabs are capped first and
/// narrow ones keep their size, but none goes below `min` (past that the bar scrolls).
fn fit_tabs(widths: &mut [f32], avail: f32, min: f32) {
    if widths.iter().sum::<f32>() <= avail {
        return;
    }
    let mut sorted = widths.to_vec();
    sorted.sort_by(f32::total_cmp);
    let mut left = avail;
    let mut cap = min;
    for (k, &w) in sorted.iter().enumerate() {
        let rest = (sorted.len() - k) as f32;
        if w * rest > left {
            cap = left / rest;
            break;
        }
        left -= w;
    }
    for w in widths {
        *w = w.min(cap.max(min));
    }
}

/// Paint a folder glyph centred in `r`. Lit = bright front flap.
fn paint_folder(p: &egui::Painter, r: egui::Rect, color: Color32, lit: bool) {
    let w = r.width();
    let h = w * 0.74;
    let body = egui::Rect::from_center_size(r.center() + vec2(0.0, w * 0.05), vec2(w, h));
    let tab = egui::Rect::from_min_size(body.min - vec2(0.0, w * 0.13), vec2(w * 0.44, w * 0.22));
    let flap = egui::Rect::from_min_max(body.min + vec2(0.0, h * 0.24), body.max);
    let (back, front) = if lit { (0.78, 1.0) } else { (0.7, 0.85) };
    p.rect_filled(tab, 2.5, color.gamma_multiply(back));
    p.rect_filled(body, 3.0, color.gamma_multiply(back));
    p.rect_filled(flap, 3.0, color.gamma_multiply(front));
}

fn project_row(ui: &mut Ui, pal: &Palette, collapsed: bool, row: ProjectRow, salt: (&str, usize)) -> RowClick {
    let h = if collapsed { 40.0 } else { 44.0 };
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click());
    let painter = ui.painter().clone();
    let hovered = ui.rect_contains_pointer(rect);
    if row.active {
        painter.rect_filled(rect.shrink2(vec2(3.0, 1.0)), 6.0, pal.selection.gamma_multiply(0.75));
        painter.rect_filled(
            egui::Rect::from_min_size(pos2(rect.min.x, rect.min.y + 8.0), vec2(3.0, h - 16.0)),
            2.0,
            pal.accent,
        );
    } else if hovered {
        painter.rect_filled(rect.shrink2(vec2(3.0, 1.0)), 6.0, pal.hover);
    }
    let icon_size = if collapsed { 24.0 } else { 26.0 };
    let icon_x = if collapsed { rect.center().x } else { rect.min.x + 12.0 + icon_size / 2.0 };
    let icon = egui::Rect::from_center_size(pos2(icon_x, rect.center().y), vec2(icon_size, icon_size));
    paint_folder(&painter, icon, row.folder, row.lit);
    if let Some(b) = row.badge {
        let c = pos2(icon.max.x - 1.0, icon.min.y + 3.0);
        painter.circle_filled(c, 6.0, pal.sidebar_bg);
        painter.circle_filled(c, 5.0, FOLDER_ALERT);
        painter.text(c, Align2::CENTER_CENTER, b, FontId::proportional(9.0), Color32::WHITE);
    }

    let mut sub: Option<RowClick> = None;
    if !collapsed {
        let x = icon.max.x + 10.0;
        let btn = 22.0;
        let mut buttons: Vec<(&str, &str, RowClick)> =
            vec![("🗀", "Project files", RowClick::Files), ("⎇", "Git", RowClick::Git)];
        if row.unload {
            buttons.push(("⏏", "Unload from memory: stop the agent, close tabs, move to Recent", RowClick::Unload));
        }
        let nb = buttons.len() as f32;
        let text_right = if hovered { rect.max.x - nb * (btn + 2.0) - 10.0 } else { rect.max.x - 6.0 };
        let clip = egui::Rect::from_min_max(pos2(x, rect.min.y), pos2(text_right, rect.max.y));
        let pc = painter.with_clip_rect(clip);
        let name_col = if row.lit { pal.text } else { pal.text_weak };
        pc.text(pos2(x, rect.center().y - 8.0), Align2::LEFT_CENTER, row.name, FontId::proportional(13.5), name_col);
        pc.text(pos2(x, rect.center().y + 9.0), Align2::LEFT_CENTER, row.status, FontId::proportional(11.0), row.status_color);
        // quick buttons: project files / git (appear on hover)
        if hovered {
            let last = buttons.len() - 1;
            for (k, (glyph, tip, click)) in buttons.into_iter().enumerate() {
                let cx = rect.max.x - 8.0 - btn / 2.0 - (last - k) as f32 * (btn + 2.0);
                let r = egui::Rect::from_center_size(pos2(cx, rect.center().y), vec2(btn, btn));
                let br = ui.interact(r, Id::new(("sb_btn", salt.0, salt.1, k)), Sense::click());
                painter.rect_filled(r, 5.0, if br.hovered() { pal.button_bg } else { pal.hover });
                painter.text(r.center(), Align2::CENTER_CENTER, glyph, FontId::proportional(13.0), pal.text);
                if br.on_hover_text(tip).clicked() {
                    sub = Some(click);
                }
            }
        }
    }
    if let Some(s) = sub {
        return s;
    }
    let resp = resp.on_hover_text(format!("{}\n{}\n\nClick: open · right-click: more", row.path.display(), row.status));
    if resp.clicked() {
        return RowClick::Main;
    }
    RowClick::Menu(resp)
}

impl eframe::App for IdeApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.auto_save(false);
        if self.settings.auto_save {
            let pending = self.projects.iter().flat_map(|p| &p.docs).filter_map(|d| d.edited_at).min();
            if let Some(t) = pending {
                ctx.request_repaint_after(AUTO_SAVE_DELAY.saturating_sub(t.elapsed()));
            }
        }
        let mut notes: Vec<(String, String, usize, Color32)> = vec![];
        let mut attention = false;
        let mut replied = false;
        for (i, p) in self.projects.iter_mut().enumerate() {
            let mut touched = false;
            let signals: Vec<Signal> = p.chats.iter_mut().flat_map(|c| c.poll()).collect();
            for s in signals {
                match s {
                    Signal::Done { summary, is_error } => {
                        replied = true;
                        p.attention = if is_error { Attention::Error } else { Attention::Done };
                        let t = if is_error { format!("✖ {} — Claude stopped", p.name) } else { format!("✔ {} — Claude finished", p.name) };
                        notes.push((t, util::truncate(&summary, 180), i, if is_error { FOLDER_ALERT } else { FOLDER_DONE }));
                        touched = true;
                        attention = true;
                    }
                    Signal::NeedsApproval { tool } => {
                        notes.push((format!("⚠ {} — Claude needs you", p.name), format!("Allow {tool}?"), i, FOLDER_ALERT));
                        attention = true;
                    }
                    Signal::FilesTouched => touched = true,
                }
            }
            if p.git.poll() {
                touched = true;
            }
            if touched {
                p.check_disk(&self.hl);
                p.tree.refresh();
                p.git.refresh_status();
            }
            p.find.poll();
            if touched || p.busy() {
                p.last_active = util::unix_now();
            }
            if p.busy() {
                p.attention = Attention::None; // a new task started
            }
        }
        for (t, b, i, col) in notes {
            self.notify(ctx, &t, &b, i, col);
        }
        // plan usage: at most once a minute while the panel is open or after replies,
        // else every 5 min (the endpoint answers HTTP 429 when polled more often)
        let panel = self.left == Some(LeftTab::Claude);
        let max_age = if panel || replied { Duration::from_secs(60) } else { Duration::from_secs(300) };
        self.plan.refresh_if_older(max_age, ctx);
        ctx.request_repaint_after(if panel { Duration::from_secs(30) } else { max_age });
        // a Windows toast was clicked → bring the window up on that project
        while let Ok(root) = self.toast_click_rx.try_recv() {
            if let Some(i) = self.projects.iter().position(|p| p.root == root) {
                self.switch(i);
            }
            ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(ViewportCommand::Focus);
        }
        if attention && !self.focused {
            ctx.send_viewport_cmd(ViewportCommand::RequestUserAttention(egui::UserAttentionType::Informational));
        }
        if self.projects.iter().any(|p| p.busy() || !p.git.busy.is_empty()) {
            // gentle animation tick instead of continuous repainting
            ctx.request_repaint_after(Duration::from_millis(160));
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let pal = theme::palette(self.settings.dark);
        self.toasts_ui(&ctx, &pal);

        // focus tracking → reload files changed by external tools
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        if !focused && self.focused {
            self.auto_save(true);
        }
        if focused && !self.focused {
            let hl = &self.hl;
            if let Some(p) = self.projects.get_mut(self.active) {
                p.check_disk(hl);
                p.tree.refresh();
                p.git.refresh_status();
            }
        }
        self.focused = focused;

        // dark/light native title bar (needs the window to exist, so do it on the first frames)
        if !self.titlebar_themed {
            ctx.send_viewport_cmd(ViewportCommand::SetTheme(if self.settings.dark { egui::SystemTheme::Dark } else { egui::SystemTheme::Light }));
            self.titlebar_themed = ctx.cumulative_frame_nr() > 2;
        }

        // window title
        let title = self.projects.get(self.active).map(|p| format!("{} — VibeZen", p.name)).unwrap_or_else(|| "VibeZen".into());
        if self.title_for.as_ref().map(|t| &t.1) != Some(&title) {
            ctx.send_viewport_cmd(ViewportCommand::Title(title.clone()));
            self.title_for = Some((self.active, title));
        }

        // close request
        if ctx.input(|i| i.viewport().close_requested()) && !self.allow_close {
            self.auto_save(true);
            let dirty = self.projects.iter().any(|p| p.dirty_count() > 0);
            let busy = self.projects.iter().any(|p| p.busy());
            if dirty || busy {
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
                self.confirm = Some(Confirm::Exit);
            }
        }

        // drag & drop
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
        for path in dropped {
            if path.is_dir() {
                self.add_project(path, &ctx, true);
                continue;
            }
            let claude_tab = self.projects.get(self.active).is_some_and(|p| p.active_doc.is_none());
            if claude_tab && crate::attach::is_attachable(&path) {
                if let Some(p) = self.projects.get_mut(self.active) {
                    match crate::attach::load(&ctx, &path) {
                        Ok(a) => p.chat_mut().attachments.push(a),
                        Err(e) => self.message = Some((e, true)),
                    }
                }
            } else if self.projects.is_empty() {
                if let Some(parent) = path.parent() {
                    self.add_project(parent.to_path_buf(), &ctx, true);
                }
                self.open_file(&path, None);
            } else {
                self.open_file(&path, None);
            }
        }

        if self.prompt.is_none() && self.confirm.is_none() && self.quick.is_none() {
            self.shortcuts(&ctx);
        }

        for p in &mut self.projects {
            for c in &mut p.chats {
                if std::mem::take(&mut c.take_defaults) {
                    c.model = self.settings.claude_model.clone();
                    c.mode = self.settings.claude_mode.clone();
                }
            }
        }

        let mut acts: Vec<Action> = vec![];

        egui::Panel::top("toolbar")
            .exact_size(40.0)
            .frame(Frame::NONE.fill(pal.panel_bg).inner_margin(Margin::symmetric(4, 0)))
            .show(ui, |ui| self.toolbar(ui, &pal, &mut acts));

        egui::Panel::bottom("status")
            .exact_size(24.0)
            .frame(Frame::NONE.fill(pal.panel_bg).inner_margin(Margin::symmetric(8, 0)))
            .show(ui, |ui| self.status_bar(ui, &pal));

        egui::Panel::left("projects_sidebar")
            .exact_size(if self.settings.sidebar_collapsed { 50.0 } else { 220.0 })
            .frame(Frame::NONE.fill(pal.sidebar_bg).inner_margin(Margin::symmetric(4, 0)))
            .show(ui, |ui| self.sidebar(ui, &pal, &mut acts));

        if !self.projects.is_empty() {
            let changed = self.projects.get(self.active).map_or(0, |p| p.git.status.files.len());
            egui::Panel::left("stripe_left")
                .exact_size(38.0)
                .frame(Frame::NONE.fill(pal.panel_bg).inner_margin(Margin::symmetric(4, 6)))
                .show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        if Self::stripe_button(ui, &pal, "🗀", "Project (Alt+1)", self.left == Some(LeftTab::Project)) {
                            self.left = if self.left == Some(LeftTab::Project) { None } else { Some(LeftTab::Project) };
                        }
                        let tip = match changed {
                            0 => "Commit (Alt+0)".to_owned(),
                            n => format!("Commit (Alt+0) · {n} uncommitted files"),
                        };
                        let on = self.left == Some(LeftTab::Commit);
                        if Self::stripe_button_col(ui, &pal, "✔", &tip, on, None, changed) {
                            self.left = if self.left == Some(LeftTab::Commit) { None } else { Some(LeftTab::Commit) };
                        }
                        if Self::stripe_button(ui, &pal, "⎇", "Git log (Alt+9)", self.left == Some(LeftTab::Log)) {
                            self.left = if self.left == Some(LeftTab::Log) { None } else { Some(LeftTab::Log) };
                        }
                        if Self::stripe_button(ui, &pal, "🔍", "Find in files (Ctrl+Shift+F)", self.left == Some(LeftTab::Find)) {
                            self.left = if self.left == Some(LeftTab::Find) { None } else { Some(LeftTab::Find) };
                            if self.left == Some(LeftTab::Find) {
                                if let Some(p) = self.proj() {
                                    p.find.focus = true;
                                }
                            }
                        }
                        let on = self.left == Some(LeftTab::Claude);
                        if Self::stripe_button_col(ui, &pal, "✻", "Claude Code: usage & sessions (Alt+3)", on, Some(claude_ui::CLAUDE), 0) {
                            self.left = if on { None } else { Some(LeftTab::Claude) };
                        }
                    });
                });

            if let Some(tab) = self.left {
                // the log and search lists want more room, so they keep their own width
                let (panel_id, width) = match tab {
                    LeftTab::Log => ("left_tool_log", 460.0),
                    LeftTab::Find => ("left_tool_find", 420.0),
                    _ => ("left_tool", 290.0),
                };
                egui::Panel::left(panel_id)
                    .resizable(true)
                    .default_size(width)
                    .min_size(180.0)
                    .frame(Frame::NONE.fill(pal.panel_bg).inner_margin(Margin::symmetric(6, 6)))
                    .show(ui, |ui| {
                        match tab {
                            LeftTab::Claude => return self.claude_panel(ui, &pal),
                            LeftTab::Find => {
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new("🔍  Find in Files").strong());
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        if ui.small_button("«").on_hover_text("Hide panel").clicked() {
                                            self.left = None;
                                        }
                                    });
                                });
                                ui.add_space(4.0);
                                return self.find_panel(ui, &pal, &mut acts);
                            }
                            _ => {}
                        }
                        let p = &mut self.projects[self.active];
                        match tab {
                            LeftTab::Claude | LeftTab::Find => {}
                            LeftTab::Log => {
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new("⎇  Git Log").strong());
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        if ui.small_button("«").on_hover_text("Hide panel").clicked() {
                                            self.left = None;
                                        }
                                        if ui.small_button("⟳").on_hover_text("Refresh").clicked() {
                                            p.git.refresh_all();
                                        }
                                    });
                                });
                                ui.add_space(4.0);
                                for a in p.git.log_ui(ui, &pal) {
                                    if let GitUiAction::OpenDiff { title, key, text } = a {
                                        acts.push(Action::OpenDiff { title, key, text });
                                    }
                                }
                            }
                            LeftTab::Project => {
                                if let Some(a) = p.tree.ui(ui, &pal, &p.git.status_map, &p.git.dirty_dirs) {
                                    acts.push(match a {
                                        TreeAction::Open(path) => Action::OpenFile(path, None),
                                        TreeAction::NewFile(d) => Action::Prompt(PromptKind::NewFile(d)),
                                        TreeAction::NewFolder(d) => Action::Prompt(PromptKind::NewFolder(d)),
                                        TreeAction::Rename(path) => Action::Prompt(PromptKind::Rename(path)),
                                        TreeAction::Delete(path) => Action::Confirm(Confirm::Delete(path)),
                                        TreeAction::Reveal(path) => Action::Reveal(path),
                                        TreeAction::Terminal(path) => Action::Terminal(path),
                                        TreeAction::CopyPath(s) => Action::Copy(s),
                                        TreeAction::AskClaude(path) => Action::AskClaude(util::rel_path(&p.root, &path)),
                                        TreeAction::HidePanel => {
                                            self.left = None;
                                            return;
                                        }
                                    });
                                }
                            }
                            LeftTab::Commit => {
                                for a in p.git.commit_ui(ui, &pal) {
                                    acts.push(match a {
                                        GitUiAction::OpenDiff { title, key, text } => Action::OpenDiff { title, key, text },
                                        GitUiAction::OpenFile(f) => Action::OpenFile(f, None),
                                        GitUiAction::ConfirmRollback(f) => Action::Confirm(Confirm::Rollback(f)),
                                        GitUiAction::HidePanel => {
                                            self.left = None;
                                            continue;
                                        }
                                    });
                                }
                            }
                        }
                    });
            }
        }

        // diffs requested from the commit window arrive asynchronously
        if let Some(p) = self.projects.get_mut(self.active) {
            for a in p.git.take_ui_actions() {
                if let GitUiAction::OpenDiff { title, key, text } = a {
                    acts.push(Action::OpenDiff { title, key, text });
                }
            }
        }

        egui::CentralPanel::default()
            .frame(Frame::NONE.fill(pal.editor_bg))
            .show(ui, |ui| {
                if self.projects.is_empty() {
                    self.welcome(ui, &pal, &mut acts);
                    return;
                }
                Frame::NONE.fill(pal.panel_bg).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    self.tabs_bar(ui, &pal, &mut acts);
                });
                let hl = &self.hl;
                let cx = EditorCtx { hl, pal: &pal, dark: self.settings.dark, font_size: self.settings.editor_font_size };
                let p = &mut self.projects[self.active];
                match p.active_doc.and_then(|i| p.docs.get_mut(i)) {
                    Some(doc) => {
                        if !doc.is_virtual() {
                            p.last_file = Some(doc.path.clone());
                        }
                        editor::show_doc(ui, doc, &cx);
                    }
                    None => {
                        p.active_doc = None; // a stale index falls back to the Claude tab
                        p.chat_mut().ensure_loaded();
                        p.chat_mut().unseen = false;
                        let last = p.last_session.clone();
                        let active_file = p
                            .last_file
                            .as_ref()
                            .filter(|f| p.docs.iter().any(|d| &d.path == *f))
                            .map(|f| util::rel_path(&p.root, f));
                        let chat = &mut p.chats[p.active_chat];
                        for a in claude_ui::show(ui, chat, &pal, last, active_file) {
                            match a {
                                ChatAction::OpenFile(f) => acts.push(Action::OpenFile(f, None)),
                                ChatAction::SetMode(m) => acts.push(Action::ClaudeMode(m)),
                                ChatAction::SetModel(m) => acts.push(Action::ClaudeModel(m)),
                                ChatAction::OpenSession(id) => acts.push(Action::OpenSession(id)),
                            }
                        }
                    }
                }
            });

        self.modals(&ctx, &pal);
        self.apply(acts, &ctx);
        // tabs opened, closed or switched → save right away, so a crash or kill doesn't lose them
        if self.projects.get(self.active).is_some_and(|p| p.tabs_sig() != self.tabs_sig) {
            self.persist();
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.persist();
    }
}
