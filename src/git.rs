//! Git integration: status, commit, push/pull/fetch, branches, log graph, diffs.
//! All git commands run on background threads; results come back over a channel.

use crate::theme::{LANES, Palette, paint_check, tool_btn};
use crate::util::hidden_cmd;
use eframe::egui::{self, Align, Align2, Color32, FontId, Frame, Layout, Margin, RichText, Sense, Stroke, Ui, pos2, vec2};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StatusKind {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
    Conflict,
}

impl StatusKind {
    pub fn color(self, p: &Palette) -> Color32 {
        match self {
            StatusKind::Modified | StatusKind::Renamed => p.modified,
            StatusKind::Added => p.added,
            StatusKind::Deleted => p.deleted,
            StatusKind::Untracked => p.untracked,
            StatusKind::Conflict => p.conflict,
        }
    }
    pub fn letter(self) -> &'static str {
        match self {
            StatusKind::Modified => "M",
            StatusKind::Added => "A",
            StatusKind::Deleted => "D",
            StatusKind::Renamed => "R",
            StatusKind::Untracked => "U",
            StatusKind::Conflict => "!",
        }
    }
}

#[derive(Clone, Debug)]
pub struct GitFile {
    /// relative to the project root
    pub path: String,
    pub orig: Option<String>,
    pub kind: StatusKind,
    /// repository folder relative to the project root ("" = the root repository)
    pub repo: String,
}

impl GitFile {
    /// Path inside its own repository.
    pub fn repo_path(&self) -> &str {
        in_repo(&self.path, &self.repo)
    }
    pub fn repo_orig(&self) -> Option<&str> {
        self.orig.as_deref().map(|o| in_repo(o, &self.repo))
    }
}

fn in_repo<'a>(path: &'a str, repo: &str) -> &'a str {
    if repo.is_empty() {
        return path;
    }
    path.strip_prefix(repo).and_then(|p| p.strip_prefix('/')).unwrap_or(path)
}

fn repo_dir(root: &Path, repo: &str) -> PathBuf {
    if repo.is_empty() { root.to_path_buf() } else { root.join(repo) }
}

/// A git repository inside the project: the root one ("") and nested ones.
#[derive(Clone, Default)]
pub struct Repo {
    pub dir: String,
    pub branch: String,
    pub upstream: Option<String>,
    /// commits on HEAD that are not pushed yet, newest first
    pub outgoing: Vec<Commit>,
}

#[derive(Clone, Default)]
pub struct StatusData {
    /// the project root itself is a repository (branch/ahead/behind describe it)
    pub is_repo: bool,
    pub branch: String,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// changes of every repository, paths relative to the project root
    pub files: Vec<GitFile>,
    /// root repository first, then nested ones
    pub repos: Vec<Repo>,
}

#[derive(Clone)]
pub struct Commit {
    pub hash: String,
    pub short: String,
    pub refs: Vec<String>,
    pub subject: String,
    pub author: String,
    pub date: String,
}

#[derive(Clone)]
pub struct LogRow {
    pub graph: String,
    pub commit: Option<Commit>,
}

pub enum GitMsg {
    Status(StatusData),
    Log(Vec<LogRow>),
    Branches(Vec<String>, Vec<String>),
    Op { label: String, result: Result<String, String> },
    Diff { title: String, key: String, text: String },
    Details { hash: String, text: String, files: Vec<(String, String)> },
    CommitMsg(String, Result<String, String>),
}

pub enum GitUiAction {
    OpenDiff { title: String, key: String, text: String },
    OpenFile(PathBuf),
    ConfirmRollback(Vec<GitFile>),
}

pub struct GitState {
    pub root: PathBuf,
    pub status: StatusData,
    pub checked: HashSet<String>,
    pub status_map: HashMap<String, StatusKind>,
    pub dirty_dirs: HashSet<String>,
    pub log: Vec<LogRow>,
    pub local_branches: Vec<String>,
    pub remote_branches: Vec<String>,
    /// commit message per repository ("" = the root one)
    pub commit_msgs: HashMap<String, String>,
    pub amend: bool,
    pub busy: Vec<String>,
    pub last_error: Option<String>,
    pub last_info: Option<String>,
    pub selected_commit: Option<String>,
    pub details: Option<(String, String, Vec<(String, String)>)>,
    /// commit messages being written by Claude
    pub generating_msg: usize,
    /// change selected in the Commit tree (shows its diff)
    pub sel_change: Option<String>,
    pub collapsed_groups: HashSet<String>,
    /// height of each commit-message box (drag the panel's top edge to change)
    pub msg_box_h: f32,
    /// a commit-message box has keyboard focus (for Ctrl+Enter)
    pub msg_focused: bool,
    status_inflight: bool,
    status_again: bool,
    /// repositories whose message is cleared once the running commit succeeds
    commit_msg_pending_clear: Vec<String>,
    pending_ui: Vec<GitUiAction>,
    known_files: HashSet<String>,
    tx: Sender<GitMsg>,
    rx: Receiver<GitMsg>,
    ctx: egui::Context,
}

/// Height of a commit-message box showing two lines of text.
const MSG_BOX_2_LINES: f32 = 50.0;

fn run_git(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = hidden_cmd("git")
        .args(["-c", "core.quotepath=false", "-c", "color.ui=false"])
        .args(args)
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| format!("Cannot run git: {e}"))?;
    if out.status.success() {
        let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
        let err = String::from_utf8_lossy(&out.stderr);
        if s.trim().is_empty() && !err.trim().is_empty() {
            s = err.into_owned(); // push/pull report progress on stderr
        }
        Ok(s)
    } else {
        let mut e = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        if e.is_empty() {
            e = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        }
        Err(e)
    }
}

fn parse_status(s: &str) -> StatusData {
    let mut d = StatusData {
        is_repo: true,
        ..Default::default()
    };
    let mut toks = s.split('\0').peekable();
    while let Some(t) = toks.next() {
        if t.is_empty() {
            continue;
        }
        if let Some(b) = t.strip_prefix("## ") {
            // "main...origin/main [ahead 1, behind 2]" | "No commits yet on main" | "HEAD (no branch)"
            let (head, rest) = match b.find(" [") {
                Some(i) => (&b[..i], &b[i..]),
                None => (b, ""),
            };
            if let Some(n) = head.strip_prefix("No commits yet on ") {
                d.branch = n.to_owned();
            } else if let Some((l, r)) = head.split_once("...") {
                d.branch = l.to_owned();
                d.upstream = Some(r.to_owned());
            } else {
                d.branch = head.to_owned();
            }
            for part in rest.trim_matches(|c| c == ' ' || c == '[' || c == ']').split(", ") {
                if let Some(n) = part.strip_prefix("ahead ") {
                    d.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix("behind ") {
                    d.behind = n.parse().unwrap_or(0);
                }
            }
            continue;
        }
        if t.len() < 4 {
            continue;
        }
        let x = t.as_bytes()[0] as char;
        let y = t.as_bytes()[1] as char;
        let path = t[3..].to_owned();
        let mut orig = None;
        if x == 'R' || x == 'C' {
            orig = toks.next().map(|s| s.to_owned());
        }
        let kind = match (x, y) {
            ('?', '?') => StatusKind::Untracked,
            ('!', '!') => continue,
            ('U', _) | (_, 'U') | ('A', 'A') | ('D', 'D') => StatusKind::Conflict,
            ('R', _) | ('C', _) => StatusKind::Renamed,
            ('A', _) => StatusKind::Added,
            ('D', _) | (_, 'D') => StatusKind::Deleted,
            _ => StatusKind::Modified,
        };
        d.files.push(GitFile { path, orig, kind, repo: String::new() });
    }
    d.files.sort_by(|a, b| a.path.to_lowercase().cmp(&b.path.to_lowercase()));
    d
}

/// Folders never searched for nested repositories.
const SKIP_DIRS: &[&str] = &["node_modules", "vendor", "target", "dist", "build", "bower_components", "__pycache__", "venv"];

/// Nested git repositories (folders with their own `.git`), relative to `root`, a few levels deep.
/// The walk is remembered for a minute: status refreshes come after every agent edit and every
/// return to the window, and on a big folder (a home directory) it takes seconds of disk work.
pub(crate) fn nested_repos(root: &Path) -> Vec<String> {
    fn walk(dir: &Path, rel: &str, depth: usize, cloud: &crate::util::CloudGuard, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            if !e.file_type().is_ok_and(|t| t.is_dir()) {
                continue; // also skips symlinks / junctions
            }
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || SKIP_DIRS.contains(&name.as_str()) || crate::util::SKIP_ALWAYS.contains(&name.as_str()) || cloud.skip(&e) {
                continue;
            }
            let r = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            let p = e.path();
            if p.join(".git").exists() {
                out.push(r.clone());
            }
            if depth > 1 {
                walk(&p, &r, depth - 1, cloud, out);
            }
        }
    }
    static CACHE: std::sync::Mutex<Option<HashMap<PathBuf, (std::time::Instant, Vec<String>)>>> = std::sync::Mutex::new(None);
    if let Some((at, v)) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).get(root) {
        if at.elapsed().as_secs() < 60 {
            return v.clone();
        }
    }
    let mut out = vec![];
    walk(root, "", 4, &crate::util::CloudGuard::new(root), &mut out);
    out.sort();
    CACHE.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(root.to_owned(), (std::time::Instant::now(), out.clone()));
    out
}

/// Commits on HEAD not pushed yet: ahead of the upstream, or (no upstream yet) on no remote branch.
fn outgoing(dir: &Path, d: &StatusData) -> Vec<Commit> {
    const FMT: &str = "--pretty=format:\x1f%H\x1f%h\x1f%D\x1f%s\x1f%an\x1f%ad";
    let r = if d.upstream.is_some() {
        if d.ahead == 0 {
            return vec![];
        }
        run_git(dir, &["log", "-n", "200", "--date=relative", FMT, "@{u}..HEAD"])
    } else {
        // without any remote there is nowhere to push to
        if run_git(dir, &["remote"]).is_ok_and(|s| s.trim().is_empty()) {
            return vec![];
        }
        run_git(dir, &["log", "-n", "200", "--date=relative", FMT, "HEAD", "--not", "--remotes"])
    };
    r.map(|s| parse_log(&s).into_iter().filter_map(|r| r.commit).collect()).unwrap_or_default()
}

/// Status of the root repository plus every nested one, merged into one list.
fn collect_status(root: &Path) -> StatusData {
    const ARGS: &[&str] = &["status", "--porcelain=v1", "-z", "-b", "--untracked-files=all"];
    let nested = nested_repos(root);
    let status = |dir: &Path| {
        run_git(dir, ARGS).ok().map(|s| {
            let d = parse_status(&s);
            let out = outgoing(dir, &d);
            (d, out)
        })
    };
    // nested repositories a few at a time: dozens of `git status` at once swamp the disk
    let ((mut d, root_out), subs) = std::thread::scope(|sc| {
        let rest = sc.spawn(|| {
            let mut subs: Vec<(String, StatusData, Vec<Commit>)> = vec![];
            for chunk in nested.chunks(3) {
                let jobs: Vec<_> = chunk
                    .iter()
                    .map(|dir| sc.spawn(move || status(&root.join(dir)).map(|(d, out)| (dir.clone(), d, out))))
                    .collect();
                subs.extend(jobs.into_iter().filter_map(|j| j.join().ok().flatten()));
            }
            subs
        });
        let d = status(root).unwrap_or_default();
        (d, rest.join().unwrap_or_default())
    });
    if d.is_repo {
        d.repos.push(Repo { dir: String::new(), branch: d.branch.clone(), upstream: d.upstream.clone(), outgoing: root_out });
    }
    for (dir, sd, out) in subs {
        d.repos.push(Repo { dir: dir.clone(), branch: sd.branch, upstream: sd.upstream, outgoing: out });
        for mut f in sd.files {
            f.path = format!("{dir}/{}", f.path);
            f.orig = f.orig.map(|o| format!("{dir}/{o}"));
            f.repo = dir.clone();
            d.files.push(f);
        }
    }
    // a nested repository also shows up in its parent as an untracked folder / submodule
    d.files.retain(|f| !nested.iter().any(|n| n == f.path.trim_end_matches('/')));
    d.files.sort_by(|a, b| a.path.to_lowercase().cmp(&b.path.to_lowercase()));
    d
}

fn parse_log(s: &str) -> Vec<LogRow> {
    s.lines()
        .map(|line| match line.find('\x1f') {
            None => LogRow {
                graph: line.trim_end().to_owned(),
                commit: None,
            },
            Some(i) => {
                let graph = line[..i].trim_end().to_owned();
                let f: Vec<&str> = line[i + 1..].split('\x1f').collect();
                let get = |k: usize| f.get(k).copied().unwrap_or("").to_owned();
                let refs = get(2)
                    .split(", ")
                    .filter(|r| !r.is_empty())
                    .map(|r| r.to_owned())
                    .collect();
                LogRow {
                    graph,
                    commit: Some(Commit {
                        hash: get(0),
                        short: get(1),
                        refs,
                        subject: get(3),
                        author: get(4),
                        date: get(5),
                    }),
                }
            }
        })
        .collect()
}

impl GitState {
    pub fn new(root: PathBuf, ctx: egui::Context) -> Self {
        let (tx, rx) = channel();
        let mut g = Self {
            root,
            status: StatusData::default(),
            checked: HashSet::new(),
            status_map: HashMap::new(),
            dirty_dirs: HashSet::new(),
            log: vec![],
            local_branches: vec![],
            remote_branches: vec![],
            commit_msgs: HashMap::new(),
            amend: false,
            busy: vec![],
            last_error: None,
            last_info: None,
            selected_commit: None,
            details: None,
            generating_msg: 0,
            sel_change: None,
            collapsed_groups: HashSet::new(),
            msg_box_h: MSG_BOX_2_LINES,
            msg_focused: false,
            status_inflight: false,
            status_again: false,
            commit_msg_pending_clear: vec![],
            pending_ui: vec![],
            known_files: HashSet::new(),
            tx,
            rx,
            ctx,
        };
        g.refresh_all();
        g
    }

    fn spawn(&self, f: impl FnOnce(&Path) -> GitMsg + Send + 'static) {
        let tx = self.tx.clone();
        let root = self.root.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let msg = f(&root);
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    pub fn refresh_status(&mut self) {
        if self.status_inflight {
            self.status_again = true;
            return;
        }
        self.status_inflight = true;
        self.spawn(|root| GitMsg::Status(collect_status(root)));
    }

    pub fn refresh_log(&mut self) {
        self.spawn(|root| {
            let r = run_git(
                root,
                &[
                    "log",
                    "--graph",
                    "--date-order",
                    "--all",
                    "-n",
                    "600",
                    "--date=relative",
                    "--pretty=format:\x1f%H\x1f%h\x1f%D\x1f%s\x1f%an\x1f%ad",
                ],
            );
            GitMsg::Log(r.map(|s| parse_log(&s)).unwrap_or_default())
        });
    }

    pub fn refresh_branches(&mut self) {
        self.spawn(|root| {
            let local = run_git(root, &["branch", "--format=%(refname:short)"]).unwrap_or_default();
            let remote = run_git(root, &["branch", "-r", "--format=%(refname:short)"]).unwrap_or_default();
            GitMsg::Branches(
                local.lines().map(|s| s.to_owned()).filter(|s| !s.is_empty()).collect(),
                remote
                    .lines()
                    .map(|s| s.to_owned())
                    .filter(|s| !s.is_empty() && !s.ends_with("/HEAD") && s.contains('/'))
                    .collect(),
            )
        });
    }

    pub fn refresh_all(&mut self) {
        self.refresh_status();
        self.refresh_log();
        self.refresh_branches();
    }

    /// Run an arbitrary git operation in the root repository, then refresh everything.
    pub fn op(&mut self, label: &str, steps: Vec<Vec<String>>) {
        self.op_in(label, steps.into_iter().map(|s| (String::new(), s)).collect());
    }

    /// Like `op`, each step running in the given repository folder ("" = root).
    pub fn op_in(&mut self, label: &str, steps: Vec<(String, Vec<String>)>) {
        self.busy.push(label.to_owned());
        let label = label.to_owned();
        self.spawn(move |root| {
            let mut out = String::new();
            for (repo, step) in steps {
                let args: Vec<&str> = step.iter().map(|s| s.as_str()).collect();
                match run_git(&repo_dir(root, &repo), &args) {
                    Ok(s) => out.push_str(&s),
                    Err(e) => {
                        return GitMsg::Op {
                            label,
                            result: Err(if repo.is_empty() { e } else { format!("[{repo}] {e}") }),
                        };
                    }
                }
            }
            GitMsg::Op {
                label,
                result: Ok(out),
            }
        });
    }

    pub fn fetch(&mut self) {
        self.op("Fetching", vec![vec!["fetch".into(), "--all".into(), "--prune".into()]]);
    }
    pub fn pull(&mut self) {
        self.op("Pulling", vec![vec!["pull".into()]]);
    }
    pub fn push(&mut self) {
        let args = if self.status.upstream.is_some() {
            vec!["push".into()]
        } else {
            vec!["push".into(), "-u".into(), "origin".into(), "HEAD".into()]
        };
        self.op("Pushing", vec![args]);
    }
    pub fn checkout(&mut self, branch: &str, remote: bool) {
        let name = if remote {
            branch.split_once('/').map(|(_, b)| b).unwrap_or(branch)
        } else {
            branch
        };
        self.op(&format!("Checking out {name}"), vec![vec!["checkout".into(), name.into()]]);
    }
    pub fn new_branch(&mut self, name: &str) {
        self.op(&format!("Creating {name}"), vec![vec!["checkout".into(), "-b".into(), name.into()]]);
    }

    /// Repositories that have changes, in display order (root first).
    pub fn changed_repos(&self) -> Vec<Repo> {
        self.status.repos.iter().filter(|r| self.status.files.iter().any(|f| f.repo == r.dir)).cloned().collect()
    }

    /// Display name of a repository: the project folder for the root one.
    pub fn repo_title(&self, dir: &str) -> String {
        if dir.is_empty() {
            self.root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| self.root.display().to_string())
        } else {
            dir.to_owned()
        }
    }

    /// Commit the checked files; every repository gets its own commit with its own message.
    pub fn commit(&mut self, and_push: bool) {
        let selected: Vec<GitFile> = self
            .status
            .files
            .iter()
            .filter(|f| self.checked.contains(&f.path))
            .cloned()
            .collect();
        if selected.is_empty() && !self.amend {
            self.last_error = Some("No files selected for commit".into());
            return;
        }
        let mut repos: Vec<String> =
            self.status.repos.iter().map(|r| r.dir.clone()).filter(|d| selected.iter().any(|f| &f.repo == d)).collect();
        if repos.is_empty() {
            repos.push(String::new()); // amend without files: the root repository
        }
        for dir in &repos {
            if self.commit_msgs.get(dir).is_none_or(|m| m.trim().is_empty()) && !self.amend {
                let title = self.repo_title(dir);
                self.last_error = Some(if repos.len() > 1 { format!("Commit message for {title} is empty") } else { "Commit message is empty".into() });
                return;
            }
        }
        let mut steps: Vec<(String, Vec<String>)> = vec![];
        for (k, dir) in repos.iter().enumerate() {
            let msg = self.commit_msgs.get(dir).map(|m| m.trim().to_owned()).unwrap_or_default();
            let sel: Vec<&GitFile> = selected.iter().filter(|f| &f.repo == dir).collect();
            let total = self.status.files.iter().filter(|f| &f.repo == dir).count();
            // `add -A` would also stage nested repositories as gitlinks
            let has_nested = self.status.repos.iter().any(|r| r.dir != *dir && (dir.is_empty() || r.dir.starts_with(&format!("{dir}/"))));
            let all = sel.len() == total && !has_nested;
            let mut commit: Vec<String> = vec!["commit".into()];
            if self.amend {
                commit.push("--amend".into());
                if msg.is_empty() {
                    commit.push("--no-edit".into());
                }
            }
            if !msg.is_empty() {
                commit.push("-m".into());
                commit.push(msg);
            }
            if all {
                steps.push((dir.clone(), vec!["add".into(), "-A".into()]));
            } else if !sel.is_empty() {
                let mut list = String::new();
                for f in &sel {
                    list.push_str(f.repo_path());
                    list.push('\0');
                    if let Some(o) = f.repo_orig() {
                        list.push_str(o);
                        list.push('\0');
                    }
                }
                let file = std::env::temp_dir().join(format!("vibezen-pathspec-{}-{k}.txt", std::process::id()));
                let _ = std::fs::write(&file, list);
                let spec = format!("--pathspec-from-file={}", file.display());
                steps.push((dir.clone(), vec!["add".into(), "-A".into(), spec.clone(), "--pathspec-file-nul".into()]));
                commit.push(spec);
                commit.push("--pathspec-file-nul".into());
            }
            steps.push((dir.clone(), commit));
            if and_push {
                let upstream = self.status.repos.iter().find(|r| &r.dir == dir).and_then(|r| r.upstream.clone());
                if upstream.is_some() {
                    steps.push((dir.clone(), vec!["push".into()]));
                } else {
                    steps.push((dir.clone(), vec!["push".into(), "-u".into(), "origin".into(), "HEAD".into()]));
                }
            }
        }
        self.op_in(if and_push { "Committing and pushing" } else { "Committing" }, steps);
        self.commit_msg_pending_clear = repos;
    }

    pub fn rollback(&mut self, files: Vec<GitFile>) {
        let mut tracked: Vec<(String, Vec<String>)> = vec![];
        let mut add = |repo: &str, p: &str| match tracked.iter_mut().find(|(r, _)| r == repo) {
            Some((_, l)) => l.push(p.to_owned()),
            None => tracked.push((repo.to_owned(), vec![p.to_owned()])),
        };
        for f in &files {
            if f.kind == StatusKind::Untracked || f.kind == StatusKind::Added && f.orig.is_none() {
                let _ = std::fs::remove_file(self.root.join(&f.path));
                if f.kind == StatusKind::Added {
                    add(&f.repo, f.repo_path());
                }
            } else {
                add(&f.repo, f.repo_path());
                if let Some(o) = f.repo_orig() {
                    add(&f.repo, o);
                }
            }
        }
        if tracked.is_empty() {
            self.refresh_status();
            return;
        }
        let steps = tracked
            .into_iter()
            .map(|(repo, paths)| {
                let mut args: Vec<String> =
                    vec!["restore".into(), "--source=HEAD".into(), "--staged".into(), "--worktree".into(), "--".into()];
                args.extend(paths);
                (repo, args)
            })
            .collect();
        self.op_in("Rolling back", steps);
    }

    pub fn show_file_diff(&self, f: &GitFile) {
        let path = f.path.clone();
        let rel = f.repo_path().to_owned();
        let repo = f.repo.clone();
        let kind = f.kind;
        self.spawn(move |root| {
            let dir = repo_dir(root, &repo);
            let text = if kind == StatusKind::Untracked {
                run_git(&dir, &["diff", "--no-index", "--", "/dev/null", &rel])
                    .or_else(|e| if e.is_empty() { Ok(String::new()) } else { Err(e) })
            } else {
                run_git(&dir, &["diff", "HEAD", "--", &rel])
                    .or_else(|_| run_git(&dir, &["diff", "--cached", "--", &rel]))
            };
            // `git diff --no-index` exits 1 when files differ; its output then arrives as Err.
            let text = match text {
                Ok(t) => t,
                Err(e) => e,
            };
            GitMsg::Diff {
                title: format!("Δ {}", path.rsplit('/').next().unwrap_or(&path)),
                key: format!("diff:{path}"),
                text: if text.trim().is_empty() { "(no textual changes)".into() } else { text },
            }
        });
    }

    pub fn select_commit(&mut self, hash: &str) {
        self.selected_commit = Some(hash.to_owned());
        let h = hash.to_owned();
        self.spawn(move |root| {
            let text = run_git(
                root,
                &["show", "-s", "--format=%H%nAuthor: %an <%ae>%nDate:   %ad%n%n%B", &h],
            )
            .unwrap_or_else(|e| e);
            let files = run_git(root, &["show", "--name-status", "--format=", &h])
                .unwrap_or_default()
                .lines()
                .filter_map(|l| {
                    let mut it = l.split('\t');
                    let st = it.next()?.to_owned();
                    let p = it.last()?.to_owned();
                    Some((st, p))
                })
                .collect();
            GitMsg::Details { hash: h, text, files }
        });
    }

    pub fn show_commit_file_diff(&self, hash: &str, path: &str) {
        let (h, p) = (hash.to_owned(), path.to_owned());
        self.spawn(move |root| {
            let text = run_git(root, &["show", "--format=%h %s%n", &h, "--", &p]).unwrap_or_else(|e| e);
            GitMsg::Diff {
                title: format!("Δ {} @{}", p.rsplit('/').next().unwrap_or(&p), &h[..h.len().min(7)]),
                key: format!("show:{h}:{p}"),
                text,
            }
        });
    }

    /// Whole diff of a commit in the given repository ("" = root).
    pub fn show_commit_diff(&self, repo: &str, hash: &str) {
        let (repo, h) = (repo.to_owned(), hash.to_owned());
        self.spawn(move |root| {
            let text = run_git(&repo_dir(root, &repo), &["show", "--stat", "-p", "--format=%h %s%nAuthor: %an <%ae>%nDate:   %ad%n", &h])
                .unwrap_or_else(|e| e);
            GitMsg::Diff {
                title: format!("Δ @{}", &h[..h.len().min(7)]),
                key: format!("show:{repo}:{h}"),
                text,
            }
        });
    }

    /// Push one repository ("" = root), setting the upstream if it has none.
    pub fn push_repo(&mut self, dir: &str) {
        let upstream = self.status.repos.iter().find(|r| r.dir == dir).and_then(|r| r.upstream.clone());
        let args = if upstream.is_some() {
            vec!["push".into()]
        } else {
            vec!["push".into(), "-u".into(), "origin".into(), "HEAD".into()]
        };
        self.op_in("Pushing", vec![(dir.to_owned(), args)]);
    }

    /// Ask Claude (haiku, one-shot) to write a commit message from the diff,
    /// one per repository with checked changes.
    pub fn generate_message(&mut self) {
        if self.generating_msg > 0 {
            return;
        }
        let mut by_repo: Vec<(String, Vec<String>)> = vec![];
        for f in self.status.files.iter().filter(|f| self.checked.contains(&f.path)) {
            match by_repo.iter_mut().find(|(r, _)| *r == f.repo) {
                Some((_, l)) => l.push(f.repo_path().to_owned()),
                None => by_repo.push((f.repo.clone(), vec![f.repo_path().to_owned()])),
            }
        }
        if by_repo.is_empty() {
            by_repo.push((String::new(), vec![]));
        }
        for (repo, files) in by_repo {
            self.generating_msg += 1;
            self.spawn(move |root| {
                let dir = repo_dir(root, &repo);
                let mut args = vec!["diff", "HEAD", "--"];
                for f in &files {
                    args.push(f);
                }
                let mut diff = run_git(&dir, &args).unwrap_or_default();
                if diff.trim().is_empty() {
                    diff = format!("New/untracked files: {}", files.join(", "));
                }
                if diff.len() > 40_000 {
                    let mut cut = 40_000;
                    while !diff.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    diff.truncate(cut);
                    diff.push_str("\n…(truncated)");
                }
                let prompt = format!(
                    "Write a git commit message for the following changes. Use a concise imperative summary line (max 72 chars), then optionally a blank line and a short bullet list. Output ONLY the commit message, no quotes or code fences.\n\n{diff}"
                );
                GitMsg::CommitMsg(repo, crate::claude::one_shot(&dir, &prompt, "haiku"))
            });
        }
    }

    /// Process background results. Returns true if the working tree may have changed.
    pub fn poll(&mut self) -> bool {
        let mut tree_changed = false;
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                GitMsg::Status(s) => {
                    self.status_inflight = false;
                    // default: newly seen files are checked (like PhpStorm)
                    let paths: HashSet<String> = s.files.iter().map(|f| f.path.clone()).collect();
                    for p in &paths {
                        if !self.known_files.contains(p) {
                            self.checked.insert(p.clone());
                        }
                    }
                    self.checked.retain(|p| paths.contains(p));
                    self.known_files = paths;
                    self.status_map = s.files.iter().map(|f| (f.path.clone(), f.kind)).collect();
                    self.dirty_dirs.clear();
                    for f in &s.files {
                        let mut p = f.path.as_str();
                        while let Some(i) = p.rfind('/') {
                            p = &p[..i];
                            self.dirty_dirs.insert(p.to_owned());
                        }
                    }
                    self.status = s;
                    if self.status_again {
                        self.status_again = false;
                        self.refresh_status();
                    }
                }
                GitMsg::Log(l) => self.log = l,
                GitMsg::Branches(l, r) => {
                    self.local_branches = l;
                    self.remote_branches = r;
                }
                GitMsg::Op { label, result } => {
                    self.busy.retain(|b| b != &label);
                    match result {
                        Ok(out) => {
                            self.last_error = None;
                            let first = out.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
                            self.last_info = Some(format!("{label}: done{}", if first.is_empty() { String::new() } else { format!(" — {first}") }));
                            if label.starts_with("Committing") && !self.commit_msg_pending_clear.is_empty() {
                                for r in &self.commit_msg_pending_clear {
                                    self.commit_msgs.remove(r);
                                }
                                self.amend = false;
                            }
                        }
                        Err(e) => {
                            self.last_error = Some(format!("{label} failed: {e}"));
                        }
                    }
                    self.commit_msg_pending_clear.clear();
                    tree_changed = true;
                    self.refresh_all();
                }
                GitMsg::Diff { title, key, text } => {
                    self.pending_ui.push(GitUiAction::OpenDiff { title, key, text });
                }
                GitMsg::Details { hash, text, files } => {
                    if self.selected_commit.as_deref() == Some(hash.as_str()) {
                        self.details = Some((hash, text, files));
                    }
                }
                GitMsg::CommitMsg(repo, r) => {
                    self.generating_msg = self.generating_msg.saturating_sub(1);
                    match r {
                        Ok(m) => {
                            self.commit_msgs.insert(repo, m.trim().to_owned());
                        }
                        Err(e) => self.last_error = Some(format!("Generating message failed: {e}")),
                    }
                }
            }
        }
        tree_changed
    }

    pub fn take_ui_actions(&mut self) -> Vec<GitUiAction> {
        std::mem::take(&mut self.pending_ui)
    }

    // ------------------------------------------------------------------ UI

    /// The "Commit" tool window (left side).
    /// The left "Commit" tool window, laid out like JetBrains: toolbar, change tree
    /// with tri-state checkboxes, resizable commit-message box, Commit / Commit and Push.
    pub fn commit_ui(&mut self, ui: &mut Ui, pal: &Palette) -> Vec<GitUiAction> {
        let mut actions = vec![];
        if !self.status.is_repo && self.status.repos.is_empty() {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("This folder is not a git repository.").color(pal.text_weak));
                if ui.button("Initialize repository").clicked() {
                    self.op("Initializing", vec![vec!["init".into()]]);
                }
            });
            return actions;
        }
        let checked_files: Vec<GitFile> =
            self.status.files.iter().filter(|f| self.checked.contains(&f.path)).cloned().collect();
        let selected: Option<GitFile> = self
            .sel_change
            .as_ref()
            .and_then(|s| self.status.files.iter().find(|f| &f.path == s))
            .cloned();

        // ---------------- toolbar
        egui::Panel::top(("commit_toolbar", self.root.clone()))
            .frame(Frame::NONE.inner_margin(Margin::symmetric(4, 3)))
            .show_separator_line(true)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 1.0;
                    if tool_btn(ui, pal, "⟳", "Refresh", true) {
                        self.refresh_all();
                    }
                    let rb = if checked_files.is_empty() { selected.clone().into_iter().collect() } else { checked_files.clone() };
                    if tool_btn(ui, pal, "↶", "Rollback… (checked files, or the selected one)", !rb.is_empty()) {
                        actions.push(GitUiAction::ConfirmRollback(rb));
                    }
                    if tool_btn(ui, pal, "⇄", "Show Diff (Ctrl+D)", selected.is_some()) {
                        if let Some(f) = &selected {
                            self.show_file_diff(f);
                        }
                    }
                    ui.add_space(6.0);
                    if tool_btn(ui, pal, "⊞", "Expand All", true) {
                        self.collapsed_groups.clear();
                    }
                    if tool_btn(ui, pal, "⊟", "Collapse All", true) {
                        for r in &self.status.repos {
                            self.collapsed_groups.insert(format!("repo:{}", r.dir));
                            self.collapsed_groups.insert(format!("unv:{}", r.dir));
                        }
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if !self.status.branch.is_empty() {
                            ui.label(RichText::new(format!("⎇ {}", self.status.branch)).small().color(pal.text_weak));
                        }
                    });
                });
            });

        // ---------------- commit message(s) + buttons (resizable from the top edge)
        // One message box per repository with changes. Fixed height (no auto-fit to
        // content, which made the box creep); drag the panel's top edge to change it.
        let mut msg_repos: Vec<(String, String, String)> =
            self.changed_repos().into_iter().map(|r| (self.repo_title(&r.dir), r.dir, r.branch)).collect();
        if msg_repos.is_empty() {
            msg_repos.push((self.repo_title(""), String::new(), self.status.branch.clone()));
        }
        let multi = msg_repos.len() > 1;
        let label_h = if multi { 20.0 } else { 0.0 };
        let per_box = label_h + self.msg_box_h + 6.0;
        // margins + amend row + buttons row
        let fixed = 14.0 + 28.0 + 40.0;
        let max_h = (ui.available_height() - 90.0).max(fixed + per_box);
        let panel_h = (fixed + per_box * msg_repos.len() as f32).min(max_h);
        let mut any_focus = false;
        let panel = egui::Panel::bottom(("commit_box", self.root.clone()))
            .resizable(false)
            .exact_size(panel_h)
            .frame(Frame::NONE.inner_margin(Margin { left: 6, right: 6, top: 8, bottom: 6 }))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.amend, "Amend");
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let busy = self.generating_msg > 0;
                        let gen_label = if busy { "✨ Writing…" } else { "✨" };
                        let tip = if multi {
                            "Generate a commit message for each repository with Claude (Haiku) from the checked changes"
                        } else {
                            "Generate commit message with Claude (Haiku) from the checked changes"
                        };
                        if tool_btn(ui, pal, gen_label, tip, !busy) {
                            self.generate_message();
                        }
                    });
                });
                ui.add_space(2.0);
                let buttons_h = 30.0;
                let gap = 6.0;
                let area_h = (ui.available_height() - buttons_h - gap - 4.0).max(self.msg_box_h);
                egui::ScrollArea::vertical()
                    .id_salt(("commit_msgs_scroll", self.root.clone()))
                    .max_height(area_h)
                    .min_scrolled_height(area_h)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // exact heights, so the scroll bar only shows when the boxes really don't fit
                        ui.spacing_mut().item_spacing.y = 0.0;
                        for (i, (title, dir, branch)) in msg_repos.iter().enumerate() {
                            if i > 0 {
                                ui.add_space(6.0);
                            }
                            if multi {
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(title).small().strong());
                                    if !branch.is_empty() {
                                        ui.label(RichText::new(format!("⎇ {branch}")).small().color(pal.text_weak));
                                    }
                                });
                            }
                            // the first box keeps the id Ctrl+K focuses
                            let msg_id = if i == 0 {
                                egui::Id::new(("commit_msg", self.root.clone()))
                            } else {
                                egui::Id::new(("commit_msg", self.root.clone(), dir.clone()))
                            };
                            let focused = ui.memory(|m| m.has_focus(msg_id));
                            any_focus |= focused;
                            let (box_rect, box_resp) =
                                ui.allocate_exact_size(vec2(ui.available_width(), self.msg_box_h), Sense::click());
                            ui.painter().rect(
                                box_rect,
                                6.0,
                                pal.input_bg,
                                Stroke::new(1.0, if focused { pal.accent } else { pal.border }),
                                egui::StrokeKind::Inside,
                            );
                            if box_resp.clicked() {
                                ui.memory_mut(|m| m.request_focus(msg_id));
                            }
                            // the padding lives here, not in the TextEdit, so two lines fit the 2-line box
                            let inner = box_rect.shrink2(vec2(10.0, 6.0));
                            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(inner));
                            let msg = self.commit_msgs.entry(dir.clone()).or_default();
                            egui::ScrollArea::vertical()
                                .id_salt(("commit_msg_scroll", self.root.clone(), dir.clone()))
                                .max_height(inner.height())
                                .auto_shrink([false, false])
                                .show(&mut child, |ui| {
                                    ui.add(
                                        egui::TextEdit::multiline(msg)
                                            .id(msg_id)
                                            .frame(Frame::NONE)
                                            .margin(Margin::ZERO)
                                            .hint_text(if multi { format!("Commit Message for {title}") } else { "Commit Message".into() })
                                            .desired_width(f32::INFINITY)
                                            // the default 4 rows overflow a 2-line box and always show a scroll bar
                                            .desired_rows(1)
                                            .min_size(vec2(0.0, inner.height())),
                                    );
                                });
                        }
                    });
                ui.add_space(gap);
                ui.horizontal(|ui| {
                    let n = self.checked.len();
                    let busy = !self.busy.is_empty();
                    let can = !busy;
                    let tip = if multi { format!("Commit {n} file(s), one commit per repository  (Ctrl+Enter)") } else { format!("Commit {n} file(s)  (Ctrl+Enter)") };
                    if ui
                        .add_enabled(
                            can,
                            egui::Button::new(RichText::new("  Commit  ").color(Color32::WHITE)).fill(pal.accent).corner_radius(4),
                        )
                        .on_hover_text(tip)
                        .clicked()
                    {
                        self.commit(false);
                    }
                    if ui.add_enabled(can, egui::Button::new("Commit and Push…").corner_radius(4)).clicked() {
                        self.commit(true);
                    }
                    if busy {
                        ui.label(RichText::new(format!("{}…", self.busy.join(", "))).small().color(pal.accent));
                    } else if n > 0 {
                        ui.label(RichText::new(format!("{n} selected")).small().color(pal.text_weak));
                    }
                });
            });
        self.msg_focused = any_focus;
        // drag the top edge of the commit box to resize the message boxes
        let pr = panel.response.rect;
        let handle = egui::Rect::from_min_max(pos2(pr.min.x, pr.min.y - 3.0), pos2(pr.max.x, pr.min.y + 3.0));
        let h = ui.interact(handle, egui::Id::new(("commit_box_resize", self.root.clone())), Sense::drag());
        if h.hovered() || h.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
            ui.painter().line_segment([pos2(pr.min.x, pr.min.y), pos2(pr.max.x, pr.min.y)], Stroke::new(2.0, pal.accent));
        }
        if h.dragged() {
            let n = msg_repos.len() as f32;
            self.msg_box_h = (self.msg_box_h - h.drag_delta().y / n).clamp(MSG_BOX_2_LINES, 600.0);
        }

        // ---------------- unpushed commits, right above the commit box
        let outgoing: Vec<Repo> = self.status.repos.iter().filter(|r| !r.outgoing.is_empty()).cloned().collect();
        if !outgoing.is_empty() {
            let multi = outgoing.len() > 1;
            let row_h = 22.0;
            let head_h = 24.0;
            let mut toggle: Option<String> = None;
            let mut diff: Option<(String, String)> = None;
            let mut push: Option<String> = None;
            let content_h: f32 = outgoing
                .iter()
                .map(|r| head_h + if self.collapsed_groups.contains(&format!("out:{}", r.dir)) { 0.0 } else { row_h * r.outgoing.len() as f32 })
                .sum();
            // up to ~7 commits visible, the rest scrolls
            let list_h = content_h.min(head_h * outgoing.len() as f32 + row_h * 7.0);
            egui::Panel::bottom(("commit_outgoing", self.root.clone()))
                .resizable(false)
                .exact_size(list_h + 7.0)
                .frame(Frame::NONE.inner_margin(Margin { left: 2, right: 2, top: 3, bottom: 4 }))
                .show(ui, |ui| {
                    egui::ScrollArea::vertical().id_salt(("outgoing", self.root.clone())).auto_shrink([false, false]).show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        for r in &outgoing {
                            let key = format!("out:{}", r.dir);
                            let open = !self.collapsed_groups.contains(&key);
                            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), head_h), Sense::click());
                            let btn = egui::Rect::from_min_max(pos2(rect.max.x - 28.0, rect.min.y), rect.max);
                            let p = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
                            if resp.hovered() {
                                p.rect_filled(rect, 3.0, pal.hover);
                            }
                            let p = p.with_clip_rect(p.clip_rect().intersect(egui::Rect::from_min_max(rect.min, pos2(btn.min.x - 2.0, rect.max.y))));
                            let x = rect.min.x + 4.0;
                            p.text(pos2(x + 5.0, rect.center().y), Align2::CENTER_CENTER, if open { "⌄" } else { "›" }, FontId::proportional(13.0), pal.text_weak);
                            let title = if multi { self.repo_title(&r.dir) } else { "Unpushed".into() };
                            let g = p.layout_no_wrap(title, FontId::proportional(13.0), pal.text);
                            let gw = g.size().x;
                            p.galley(pos2(x + 14.0, rect.center().y - g.size().y / 2.0), g, pal.text);
                            let n = r.outgoing.len();
                            let to = r.upstream.clone().unwrap_or_else(|| "origin (new branch)".into());
                            let info = format!("{n} {} → {to}", if n == 1 { "commit" } else { "commits" });
                            p.text(pos2(x + 14.0 + gw + 8.0, rect.center().y), Align2::LEFT_CENTER, info, FontId::proportional(12.0), pal.text_weak);
                            if resp.clicked() {
                                toggle = Some(key);
                            }
                            let can_push = self.busy.is_empty();
                            let tip = if multi { format!("Push {}", self.repo_title(&r.dir)) } else { "Push".into() };
                            let pushed = ui
                                .scope_builder(egui::UiBuilder::new().max_rect(btn).layout(Layout::right_to_left(Align::Center)), |ui| {
                                    tool_btn(ui, pal, "↑", &tip, can_push)
                                })
                                .inner;
                            if pushed {
                                push = Some(r.dir.clone());
                            }
                            if !open {
                                continue;
                            }
                            for c in &r.outgoing {
                                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
                                let p = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
                                if resp.hovered() {
                                    p.rect_filled(rect, 3.0, pal.hover);
                                }
                                let mid = rect.center().y;
                                let mut x = rect.min.x + 22.0;
                                let hg = p.layout_no_wrap(c.short.clone(), FontId::monospace(11.5), pal.text_weak);
                                let hw = hg.size().x;
                                p.galley(pos2(x, mid - hg.size().y / 2.0), hg, pal.text_weak);
                                x += hw + 8.0;
                                let mut subj_max = rect.max.x - 6.0;
                                if rect.width() >= 280.0 {
                                    let dg = p.layout_no_wrap(c.date.clone(), FontId::proportional(11.5), pal.text_weak);
                                    subj_max -= dg.size().x + 10.0;
                                    p.galley(pos2(rect.max.x - 6.0 - dg.size().x, mid - dg.size().y / 2.0), dg, pal.text_weak);
                                }
                                let sg = p.layout_no_wrap(c.subject.clone(), FontId::proportional(13.0), pal.text);
                                let clip = egui::Rect::from_min_max(pos2(x, rect.min.y), pos2(subj_max.max(x), rect.max.y));
                                p.with_clip_rect(p.clip_rect().intersect(clip)).galley(pos2(x, mid - sg.size().y / 2.0), sg, pal.text);
                                if resp.clicked() {
                                    diff = Some((r.dir.clone(), c.hash.clone()));
                                }
                                resp.on_hover_text(format!("{}  {}\n{}  ·  {}\nclick: show diff", c.short, c.subject, c.author, c.date))
                                    .context_menu(|ui| {
                                        if ui.button("⇄  Show Diff").clicked() {
                                            diff = Some((r.dir.clone(), c.hash.clone()));
                                            ui.close();
                                        }
                                        if ui.button("Copy Revision Number").clicked() {
                                            ui.ctx().copy_text(c.hash.clone());
                                            ui.close();
                                        }
                                    });
                            }
                        }
                    });
                });
            if let Some(k) = toggle {
                if !self.collapsed_groups.remove(&k) {
                    self.collapsed_groups.insert(k);
                }
            }
            if let Some((repo, hash)) = diff {
                self.show_commit_diff(&repo, &hash);
            }
            if let Some(dir) = push {
                self.push_repo(&dir);
            }
        }

        // ---------------- change tree: one group per repository, named after it
        egui::CentralPanel::default().frame(Frame::NONE.inner_margin(Margin::symmetric(2, 2))).show(ui, |ui| {
            if self.status.files.is_empty() {
                ui.add_space(14.0);
                ui.vertical_centered(|ui| ui.label(RichText::new("No local changes").color(pal.text_weak)));
                return;
            }
            enum Row {
                Group { key: String, title: String, info: String, files: Vec<GitFile>, indent: f32 },
                File(GitFile, f32),
            }
            let mut rows: Vec<Row> = vec![];
            for r in self.status.repos.clone() {
                let files: Vec<GitFile> = self.status.files.iter().filter(|f| f.repo == r.dir).cloned().collect();
                if files.is_empty() {
                    continue;
                }
                let (tracked, untracked): (Vec<GitFile>, Vec<GitFile>) =
                    files.iter().cloned().partition(|f| f.kind != StatusKind::Untracked);
                let key = format!("repo:{}", r.dir);
                let open = !self.collapsed_groups.contains(&key);
                let n = files.len();
                let info = format!("{}{n} {}", if r.branch.is_empty() { String::new() } else { format!("⎇ {} · ", r.branch) }, if n == 1 { "file" } else { "files" });
                rows.push(Row::Group { key, title: self.repo_title(&r.dir), info, files, indent: 0.0 });
                if !open {
                    continue;
                }
                rows.extend(tracked.into_iter().map(|f| Row::File(f, 1.0)));
                if !untracked.is_empty() {
                    let key = format!("unv:{}", r.dir);
                    let open = !self.collapsed_groups.contains(&key);
                    let n = untracked.len();
                    let info = format!("{n} {}", if n == 1 { "file" } else { "files" });
                    let files = untracked.clone();
                    rows.push(Row::Group { key, title: "Unversioned Files".into(), info, files, indent: 1.0 });
                    if open {
                        rows.extend(untracked.into_iter().map(|f| Row::File(f, 2.0)));
                    }
                }
            }
            let row_h = 22.0;
            let step = 18.0;
            let mut toggle_files: Vec<(String, bool)> = vec![];
            let mut toggle_group: Option<String> = None;
            let mut select: Option<GitFile> = None;
            let mut open: Option<GitFile> = None;
            egui::ScrollArea::vertical().id_salt(("changes", self.root.clone())).auto_shrink([false, false]).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for row in &rows {
                    match row {
                        Row::Group { key, title, info, files, indent } => {
                            let n_checked = files.iter().filter(|f| self.checked.contains(&f.path)).count();
                            let state = if n_checked == 0 {
                                Some(false)
                            } else if n_checked == files.len() {
                                Some(true)
                            } else {
                                None
                            };
                            let open_group = !self.collapsed_groups.contains(key);
                            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
                            let p = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
                            if resp.hovered() {
                                p.rect_filled(rect, 3.0, pal.hover);
                            }
                            let mut x = rect.min.x + 4.0 + indent * step;
                            p.text(pos2(x + 5.0, rect.center().y), Align2::CENTER_CENTER, if open_group { "⌄" } else { "›" }, FontId::proportional(13.0), pal.text_weak);
                            x += 14.0;
                            let cb = egui::Rect::from_center_size(pos2(x + 7.0, rect.center().y), vec2(13.0, 13.0));
                            paint_check(&p, cb, state, pal);
                            x += 20.0;
                            let g = p.layout_no_wrap(title.clone(), FontId::proportional(13.0), pal.text);
                            let gw = g.size().x;
                            p.galley(pos2(x, rect.center().y - g.size().y / 2.0), g, pal.text);
                            p.text(pos2(x + gw + 8.0, rect.center().y), Align2::LEFT_CENTER, info, FontId::proportional(12.0), pal.text_weak);
                            if resp.clicked() {
                                let ptr = resp.interact_pointer_pos().unwrap_or_default();
                                if cb.expand(4.0).contains(ptr) {
                                    let on = state != Some(true);
                                    for f in files.iter() {
                                        toggle_files.push((f.path.clone(), on));
                                    }
                                } else {
                                    toggle_group = Some(key.clone());
                                }
                            }
                        }
                        Row::File(f, indent) => {
                            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
                            let p = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
                            let is_sel = self.sel_change.as_deref() == Some(f.path.as_str());
                            if is_sel {
                                p.rect_filled(rect, 3.0, pal.selection);
                            } else if resp.hovered() {
                                p.rect_filled(rect, 3.0, pal.hover);
                            }
                            let mut x = rect.min.x + 22.0 + (indent - 1.0) * step;
                            let c = self.checked.contains(&f.path);
                            let cb = egui::Rect::from_center_size(pos2(x + 7.0, rect.center().y), vec2(13.0, 13.0));
                            paint_check(&p, cb, Some(c), pal);
                            x += 20.0;
                            let rp = f.repo_path();
                            let (dir, name) = match rp.rfind('/') {
                                Some(i) => (&rp[..i], &rp[i + 1..]),
                                None => ("", rp),
                            };
                            let (badge, bcol) = crate::tree::file_badge(name);
                            let bg = p.layout_no_wrap(badge.to_owned(), FontId::proportional(9.5), bcol);
                            let bw = bg.size().x.max(12.0);
                            p.galley(pos2(x + (bw - bg.size().x) / 2.0, rect.center().y - bg.size().y / 2.0), bg, bcol);
                            x += bw + 6.0;
                            let col = f.kind.color(pal);
                            let ng = p.layout_no_wrap(name.to_owned(), FontId::proportional(13.0), col);
                            let nw = ng.size().x;
                            p.galley(pos2(x, rect.center().y - ng.size().y / 2.0), ng, col);
                            if !dir.is_empty() {
                                p.text(pos2(x + nw + 8.0, rect.center().y), Align2::LEFT_CENTER, dir, FontId::proportional(12.0), pal.text_weak);
                            }
                            if resp.clicked() {
                                let ptr = resp.interact_pointer_pos().unwrap_or_default();
                                if cb.expand(4.0).contains(ptr) {
                                    toggle_files.push((f.path.clone(), !c));
                                } else {
                                    select = Some(f.clone());
                                }
                            }
                            if resp.double_clicked() {
                                open = Some(f.clone());
                            }
                            resp.on_hover_text(format!("{}  ·  {}\nclick: diff · double-click: open", f.path, f.kind.letter()))
                                .context_menu(|ui| {
                                    if ui.button("⇄  Show Diff").clicked() {
                                        select = Some(f.clone());
                                        ui.close();
                                    }
                                    if ui.button("Open File").clicked() {
                                        open = Some(f.clone());
                                        ui.close();
                                    }
                                    ui.separator();
                                    if ui.button("↶  Rollback…").clicked() {
                                        actions.push(GitUiAction::ConfirmRollback(vec![f.clone()]));
                                        ui.close();
                                    }
                                });
                        }
                    }
                }
            });
            for (p, on) in toggle_files {
                if on {
                    self.checked.insert(p);
                } else {
                    self.checked.remove(&p);
                }
            }
            if let Some(k) = toggle_group {
                if !self.collapsed_groups.remove(&k) {
                    self.collapsed_groups.insert(k);
                }
            }
            if let Some(f) = select {
                self.sel_change = Some(f.path.clone());
                self.show_file_diff(&f);
            }
            if let Some(f) = open {
                actions.push(GitUiAction::OpenFile(self.root.join(&f.path)));
            }
        });
        actions
    }

    /// The bottom "Git" tool window: log graph + commit details.
    pub fn log_ui(&mut self, ui: &mut Ui, pal: &Palette) -> Vec<GitUiAction> {
        let mut actions = vec![];
        if !self.status.is_repo {
            ui.label(RichText::new("Not a git repository").color(pal.text_weak));
            return actions;
        }
        let full = ui.available_rect_before_wrap();
        // a narrow tool window stacks the details under the list instead of beside it
        let stacked = full.width() < 760.0;
        let (left, right) = if stacked {
            let split = full.min.y + (full.height() * 0.6).max(120.0).min(full.height() - 80.0);
            (
                egui::Rect::from_min_max(full.min, pos2(full.max.x, split)),
                egui::Rect::from_min_max(pos2(full.min.x, split + 6.0), full.max),
            )
        } else {
            let details_w = (full.width() * 0.36).clamp(240.0, 520.0);
            (
                egui::Rect::from_min_max(full.min, pos2(full.max.x - details_w, full.max.y)),
                egui::Rect::from_min_max(pos2(full.max.x - details_w + 6.0, full.min.y), full.max),
            )
        };

        let mut clicked: Option<String> = None;
        let mut checkout: Option<String> = None;
        ui.scope_builder(egui::UiBuilder::new().max_rect(left), |ui| {
            let row_h = 22.0;
            let cell = 8.0;
            let font = FontId::proportional(13.0);
            let small = FontId::proportional(11.5);
            egui::ScrollArea::vertical()
                .id_salt("git_log")
                .auto_shrink([false, false])
                .show_rows(ui, row_h, self.log.len(), |ui, range| {
                    for row in &self.log[range] {
                        let w = ui.available_width();
                        let (rect, resp) = ui.allocate_exact_size(vec2(w, row_h), Sense::click());
                        let p = ui.painter();
                        let sel = row
                            .commit
                            .as_ref()
                            .map(|c| self.selected_commit.as_deref() == Some(c.hash.as_str()))
                            .unwrap_or(false);
                        if sel {
                            p.rect_filled(rect, 3.0, pal.selection);
                        } else if resp.hovered() && row.commit.is_some() {
                            p.rect_filled(rect, 3.0, pal.hover);
                        }
                        // graph
                        let (top, bot) = (rect.min.y, rect.max.y);
                        let mid = rect.center().y;
                        for (ci, ch) in row.graph.chars().enumerate() {
                            let x = rect.min.x + 6.0 + ci as f32 * cell + cell * 0.5;
                            let lane = LANES[(ci / 2) % LANES.len()];
                            let s = Stroke::new(1.6, lane);
                            match ch {
                                '*' => {
                                    p.line_segment([pos2(x, top), pos2(x, bot)], s);
                                    p.circle_filled(pos2(x, mid), 4.2, lane);
                                    p.circle_filled(pos2(x, mid), 2.0, pal.panel_bg);
                                }
                                '|' => { p.line_segment([pos2(x, top), pos2(x, bot)], s); }
                                '/' => { p.line_segment([pos2(x + cell, top), pos2(x - cell, bot)], Stroke::new(1.6, LANES[((ci + 1) / 2) % LANES.len()])); }
                                '\\' => { p.line_segment([pos2(x - cell, top), pos2(x + cell, bot)], Stroke::new(1.6, LANES[(ci.saturating_sub(1) / 2) % LANES.len()])); }
                                '_' => { p.line_segment([pos2(x - cell * 0.5, bot), pos2(x + cell * 0.5, bot)], s); }
                                '-' | '.' => { p.line_segment([pos2(x - cell * 0.5, mid), pos2(x + cell * 0.5, mid)], s); }
                                _ => {}
                            }
                        }
                        let Some(c) = &row.commit else { continue };
                        let gw = row.graph.chars().count() as f32 * cell + 14.0;
                        let mut x = rect.min.x + gw.max(30.0);
                        for r in &c.refs {
                            let (txt, col) = if let Some(b) = r.strip_prefix("HEAD -> ") {
                                (b.to_owned(), pal.added)
                            } else if let Some(t) = r.strip_prefix("tag: ") {
                                (t.to_owned(), pal.warn)
                            } else if r.contains('/') {
                                (r.clone(), LANES[3])
                            } else {
                                (r.clone(), pal.modified)
                            };
                            let g = p.layout_no_wrap(txt, small.clone(), col);
                            let br = egui::Rect::from_min_size(pos2(x, mid - 8.0), vec2(g.size().x + 10.0, 16.0));
                            p.rect_filled(br, 4.0, col.gamma_multiply(0.18));
                            p.rect_stroke(br, 4.0, Stroke::new(1.0, col.gamma_multiply(0.6)), egui::StrokeKind::Inside);
                            p.galley(pos2(x + 5.0, mid - g.size().y / 2.0), g, col);
                            x += br.width() + 4.0;
                        }
                        let right_x = rect.max.x - 8.0;
                        // author and date columns only when the row has room for them
                        let columns = w >= 560.0;
                        let date_x = right_x - 110.0;
                        let author_x = date_x - 130.0;
                        let subj_max = if columns { author_x - x - 10.0 } else { right_x - x }.max(20.0);
                        let subj = p.layout(c.subject.clone(), font.clone(), pal.text, f32::INFINITY);
                        let clip = egui::Rect::from_min_max(pos2(x, rect.min.y), pos2(x + subj_max, rect.max.y));
                        p.with_clip_rect(clip).galley(pos2(x + 2.0, mid - subj.size().y / 2.0), subj, pal.text);
                        if columns {
                            let date_g = p.layout_no_wrap(c.date.clone(), small.clone(), pal.text_weak);
                            let author_g = p.layout_no_wrap(c.author.clone(), small.clone(), pal.text_weak);
                            p.with_clip_rect(egui::Rect::from_min_max(pos2(author_x, top), pos2(date_x - 6.0, bot)))
                                .galley(pos2(author_x, mid - author_g.size().y / 2.0), author_g, pal.text_weak);
                            p.galley(pos2(date_x, mid - date_g.size().y / 2.0), date_g, pal.text_weak);
                        }
                        if resp.clicked() {
                            clicked = Some(c.hash.clone());
                        }
                        resp.on_hover_text(format!("{}  {}\n{}  ·  {}", c.short, c.subject, c.author, c.date)).context_menu(|ui| {
                            if ui.button("Copy Revision Number").clicked() {
                                ui.ctx().copy_text(c.hash.clone());
                                ui.close();
                            }
                            if ui.button("Checkout Revision").clicked() {
                                checkout = Some(c.hash.clone());
                                ui.close();
                            }
                        });
                    }
                });
        });
        if let Some(h) = clicked {
            self.select_commit(&h);
        }
        if let Some(h) = checkout {
            self.op("Checking out", vec![vec!["checkout".into(), h]]);
        }

        // details pane
        let divider = if stacked {
            [pos2(right.min.x, right.min.y - 3.0), pos2(right.max.x, right.min.y - 3.0)]
        } else {
            [pos2(right.min.x - 3.0, right.min.y), pos2(right.min.x - 3.0, right.max.y)]
        };
        ui.painter().line_segment(divider, Stroke::new(1.0, pal.border));
        ui.scope_builder(egui::UiBuilder::new().max_rect(right), |ui| {
            egui::ScrollArea::vertical().id_salt("git_details").auto_shrink([false, false]).show(ui, |ui| {
                match &self.details {
                    None => {
                        ui.label(RichText::new("Select a commit to see details").color(pal.text_weak));
                    }
                    Some((hash, text, files)) => {
                        let hash = hash.clone();
                        for (st, path) in files.clone() {
                            ui.horizontal(|ui| {
                                let kind = match st.chars().next() {
                                    Some('A') => StatusKind::Added,
                                    Some('D') => StatusKind::Deleted,
                                    Some('R') => StatusKind::Renamed,
                                    _ => StatusKind::Modified,
                                };
                                ui.label(RichText::new(kind.letter()).monospace().color(kind.color(pal)));
                                if ui
                                    .add(egui::Label::new(RichText::new(&path).color(kind.color(pal))).sense(Sense::click()).truncate())
                                    .on_hover_text("Show diff")
                                    .clicked()
                                {
                                    self.show_commit_file_diff(&hash, &path);
                                }
                            });
                        }
                        ui.separator();
                        ui.label(RichText::new(text.clone()).monospace().size(12.0));
                    }
                }
            });
        });
        let _ = ui.allocate_rect(full, Sense::hover());
        actions.append(&mut self.take_ui_actions());
        actions
    }
}

#[cfg(test)]
mod multi_repo_tests {
    use super::*;

    #[test]
    fn nested_repo_changes_are_listed_under_their_repo() {
        let root = std::env::temp_dir().join(format!("vibezen-multirepo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let sub = root.join("libs").join("core");
        std::fs::create_dir_all(&sub).unwrap();
        run_git(&root, &["init", "-q"]).unwrap();
        run_git(&sub, &["init", "-q"]).unwrap();
        std::fs::write(root.join("a.txt"), "a").unwrap();
        std::fs::write(sub.join("b.txt"), "b").unwrap();

        let d = collect_status(&root);
        let _ = std::fs::remove_dir_all(&root);
        assert!(d.is_repo);
        assert_eq!(d.repos.iter().map(|r| r.dir.as_str()).collect::<Vec<_>>(), ["", "libs/core"]);
        let files: Vec<(&str, &str, &str)> = d.files.iter().map(|f| (f.path.as_str(), f.repo.as_str(), f.repo_path())).collect();
        // the nested repo itself is not listed as an untracked folder of the root repo
        assert_eq!(files, [("a.txt", "", "a.txt"), ("libs/core/b.txt", "libs/core", "b.txt")]);
    }

    #[test]
    fn unpushed_commits_are_listed() {
        let base = std::env::temp_dir().join(format!("vibezen-outgoing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (remote, root) = (base.join("remote.git"), base.join("work"));
        std::fs::create_dir_all(&remote).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        run_git(&remote, &["init", "-q", "--bare"]).unwrap();
        run_git(&root, &["init", "-q", "-b", "main"]).unwrap();
        run_git(&root, &["remote", "add", "origin", &remote.to_string_lossy()]).unwrap();
        let commit = |msg: &str| {
            std::fs::write(root.join("f.txt"), msg).unwrap();
            run_git(&root, &["add", "-A"]).unwrap();
            run_git(&root, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", msg]).unwrap();
        };
        commit("one");
        // a branch that was never pushed: everything not on a remote
        let subjects = |d: &StatusData| d.repos[0].outgoing.iter().map(|c| c.subject.clone()).collect::<Vec<_>>();
        assert_eq!(subjects(&collect_status(&root)), ["one"]);
        run_git(&root, &["push", "-q", "-u", "origin", "main"]).unwrap();
        assert!(subjects(&collect_status(&root)).is_empty());
        commit("two");
        commit("three");
        let d = collect_status(&root);
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(subjects(&d), ["three", "two"]);
    }
}
