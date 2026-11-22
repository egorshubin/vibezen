//! Project file index, fuzzy "Go to File" and "Find in Files".

use eframe::egui;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const SKIP_DIRS: &[&str] = &[
    ".git", "node_modules", "target", ".idea", ".vs", ".next", ".nuxt", "dist", "build", "__pycache__",
    ".venv", "venv", ".gradle", ".cache", "coverage", ".turbo",
];

pub struct FileIndex {
    pub files: Arc<Mutex<Vec<String>>>,
    pub ready: Arc<Mutex<bool>>,
}

impl FileIndex {
    pub fn build(root: PathBuf, ctx: egui::Context) -> Self {
        let files = Arc::new(Mutex::new(vec![]));
        let ready = Arc::new(Mutex::new(false));
        let (f2, r2) = (files.clone(), ready.clone());
        std::thread::spawn(move || {
            let mut out = list_files(&root);
            out.sort();
            *f2.lock().unwrap() = out;
            *r2.lock().unwrap() = true;
            ctx.request_repaint();
        });
        Self { files, ready }
    }
}

/// Every file of the project, relative to `root`.
fn list_files(root: &Path) -> Vec<String> {
    let mut out = vec![];
    walk(root, root, &crate::util::CloudGuard::new(root), &mut out);
    out
}

fn walk(root: &Path, dir: &Path, cloud: &crate::util::CloudGuard, out: &mut Vec<String>) {
    if out.len() > 150_000 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            if SKIP_DIRS.contains(&name.as_str()) || crate::util::SKIP_ALWAYS.contains(&name.as_str()) || cloud.skip(&e) {
                continue;
            }
            walk(root, &e.path(), cloud, out);
        } else if ft.is_file() {
            out.push(crate::util::rel_path(root, &e.path()));
        }
    }
}

/// Simple fuzzy subsequence score; higher is better. None = no match.
pub fn fuzzy_score(path: &str, query: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let p = path.to_lowercase();
    let q = query.to_lowercase();
    let fname_start = p.rfind('/').map(|i| i + 1).unwrap_or(0);
    // exact substring in file name is best
    if let Some(i) = p[fname_start..].find(&q) {
        return Some(10_000 - i as i64 * 10 - p.len() as i64);
    }
    if let Some(i) = p.find(&q) {
        return Some(5_000 - i as i64 - p.len() as i64);
    }
    let pc: Vec<char> = p.chars().collect();
    let mut score = 0i64;
    let mut pi = 0usize;
    let mut last = 0usize;
    for qc in q.chars() {
        if qc == ' ' {
            continue;
        }
        let mut found = false;
        while pi < pc.len() {
            if pc[pi] == qc {
                score += if pi == last + 1 { 15 } else { 1 };
                if pi == 0 || matches!(pc[pi - 1], '/' | '_' | '-' | '.') {
                    score += 10;
                }
                last = pi;
                pi += 1;
                found = true;
                break;
            }
            pi += 1;
        }
        if !found {
            return None;
        }
    }
    Some(score - p.len() as i64 / 4)
}

pub fn quick_matches(files: &[String], query: &str, limit: usize) -> Vec<String> {
    let mut scored: Vec<(i64, &String)> = files
        .iter()
        .filter_map(|f| fuzzy_score(f, query).map(|s| (s, f)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.into_iter().take(limit).map(|(_, f)| f.clone()).collect()
}

// ------------------------------------------------------------ find in files

/// One matching line.
#[derive(Clone)]
pub struct Hit {
    pub path: String,
    pub line: usize,
    /// 1-based char column of the first match
    pub col: usize,
    /// match length in chars (to select it in the editor)
    pub len: usize,
    /// display text (trimmed / shortened)
    pub text: String,
    /// byte ranges of matches inside `text`
    pub ranges: Vec<(usize, usize)>,
}

/// Flattened result list: a file header followed by its hits.
pub enum Row {
    File { path: String, count: usize },
    Hit(usize),
}

enum Msg {
    Hits(Vec<Hit>),
    Done { truncated: bool, files: usize },
}

const MAX_HITS: usize = 5000;

pub struct FindInFiles {
    pub query: String,
    pub case: bool,
    pub word: bool,
    pub regex: bool,
    /// comma separated: `*.rs, *.ts, src/`
    pub mask: String,
    pub results: Vec<Hit>,
    pub rows: Vec<Row>,
    pub running: bool,
    pub searched: Option<String>,
    pub error: Option<String>,
    pub truncated: bool,
    pub scanned: usize,
    pub focus: bool,
    /// selected row (keyboard navigation)
    pub sel: Option<usize>,
    pub scroll_to_sel: bool,
    /// search-as-you-type debounce
    pub dirty_at: Option<std::time::Instant>,
    sig: String,
    generation: Arc<AtomicU64>,
    rx: Option<Receiver<Msg>>,
}

impl Default for FindInFiles {
    fn default() -> Self {
        Self {
            query: String::new(),
            case: false,
            word: false,
            regex: false,
            mask: String::new(),
            results: vec![],
            rows: vec![],
            running: false,
            searched: None,
            error: None,
            truncated: false,
            scanned: 0,
            focus: false,
            sel: None,
            scroll_to_sel: false,
            dirty_at: None,
            sig: String::new(),
            generation: Arc::new(AtomicU64::new(0)),
            rx: None,
        }
    }
}

pub fn build_regex(query: &str, case: bool, word: bool, regex: bool) -> Result<regex::Regex, String> {
    let body = if regex { query.to_owned() } else { regex::escape(query) };
    let pat = if word { format!(r"\b(?:{body})\b") } else { body };
    regex::RegexBuilder::new(&pat)
        .case_insensitive(!case)
        .size_limit(20 << 20)
        .build()
        .map_err(|e| e.to_string().lines().last().unwrap_or("invalid regex").trim().to_owned())
}

fn mask_matches(masks: &[String], path: &str) -> bool {
    if masks.is_empty() {
        return true;
    }
    let lp = path.to_lowercase().replace('\\', "/");
    let name = lp.rsplit('/').next().unwrap_or(&lp).to_owned();
    masks.iter().any(|m| {
        if let Some(ext) = m.strip_prefix("*.") {
            name.ends_with(&format!(".{ext}"))
        } else if let Some(rest) = m.strip_prefix('*') {
            name.ends_with(rest)
        } else {
            lp.contains(m.as_str())
        }
    })
}

fn char_floor(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Shorten a long line around its first match and collect highlight ranges.
fn display_line(line: &str, re: &regex::Regex, first: usize) -> (String, Vec<(usize, usize)>) {
    let indent = line.len() - line.trim_start().len();
    let mut start = indent;
    let mut prefix = "";
    if first > start + 70 {
        start = char_floor(line, first - 40);
        prefix = "…";
    }
    let end = char_floor(line, start + 240);
    let body = line[start..end].trim_end();
    let off = prefix.len();
    let ranges = re
        .find_iter(body)
        .filter(|m| m.start() < m.end())
        .take(30)
        .map(|m| (m.start() + off, m.end() + off))
        .collect();
    (format!("{prefix}{body}"), ranges)
}

impl FindInFiles {
    fn params(&self) -> String {
        format!("{}\u{1f}{}{}{}\u{1f}{}", self.query, self.case, self.word, self.regex, self.mask)
    }

    /// True when the visible results belong to the current query/options.
    pub fn up_to_date(&self) -> bool {
        self.sig == self.params()
    }

    pub fn cancel(&mut self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.rx = None;
        self.running = false;
    }

    /// Start a background search. The file list is re-walked so new files are found.
    pub fn start(&mut self, root: PathBuf, ctx: egui::Context) {
        self.cancel();
        self.dirty_at = None;
        self.sig = self.params();
        self.results.clear();
        self.rows.clear();
        self.sel = None;
        self.error = None;
        self.truncated = false;
        self.scanned = 0;
        if self.query.is_empty() {
            self.searched = None;
            return;
        }
        let re = match build_regex(&self.query, self.case, self.word, self.regex) {
            Ok(r) => r,
            Err(e) => {
                self.error = Some(e);
                self.searched = None;
                return;
            }
        };
        let masks: Vec<String> = self
            .mask
            .split([',', ';', ' '])
            .map(|m| m.trim().to_lowercase().replace('\\', "/"))
            .filter(|m| !m.is_empty())
            .collect();
        let (tx, rx) = channel();
        self.rx = Some(rx);
        self.running = true;
        self.searched = Some(self.query.clone());
        let my_gen = self.generation.load(Ordering::SeqCst);
        let generation = self.generation.clone();
        std::thread::spawn(move || {
            let alive = || generation.load(Ordering::SeqCst) == my_gen;
            let mut files = list_files(&root);
            files.sort();
            let mut total = 0usize;
            let mut batch: Vec<Hit> = vec![];
            let mut last_flush = std::time::Instant::now();
            let mut truncated = false;
            let mut scanned = 0usize;
            'outer: for f in files.iter().filter(|f| mask_matches(&masks, f)) {
                if !alive() {
                    return;
                }
                scanned += 1;
                let p = root.join(f);
                let Ok(meta) = std::fs::metadata(&p) else { continue };
                // reading an online-only file would download it from the cloud
                if meta.len() > 3_000_000 || crate::util::is_online_only(&meta) {
                    continue;
                }
                let Ok(bytes) = std::fs::read(&p) else { continue };
                if bytes[..bytes.len().min(8000)].contains(&0) {
                    continue; // binary
                }
                let text = String::from_utf8_lossy(&bytes);
                if !re.is_match(&text) {
                    continue;
                }
                for (i, line) in text.lines().enumerate() {
                    let Some(m) = re.find_iter(line).find(|m| m.start() < m.end()) else { continue };
                    let (shown, ranges) = display_line(line, &re, m.start());
                    batch.push(Hit {
                        path: f.clone(),
                        line: i + 1,
                        col: line[..m.start()].chars().count() + 1,
                        len: m.as_str().chars().count(),
                        text: shown,
                        ranges,
                    });
                    total += 1;
                    if total >= MAX_HITS {
                        truncated = true;
                        break 'outer;
                    }
                }
                // stream results so the list fills while searching
                if !batch.is_empty() && last_flush.elapsed().as_millis() > 60 {
                    let _ = tx.send(Msg::Hits(std::mem::take(&mut batch)));
                    ctx.request_repaint();
                    last_flush = std::time::Instant::now();
                }
            }
            if !alive() {
                return;
            }
            if !batch.is_empty() {
                let _ = tx.send(Msg::Hits(batch));
            }
            let _ = tx.send(Msg::Done { truncated, files: scanned });
            ctx.request_repaint();
        });
    }

    pub fn poll(&mut self) {
        let Some(rx) = &self.rx else { return };
        let mut done = false;
        let mut incoming = vec![];
        for msg in rx.try_iter() {
            match msg {
                Msg::Hits(hits) => incoming.extend(hits),
                Msg::Done { truncated, files } => {
                    self.truncated = truncated;
                    self.scanned = files;
                    done = true;
                }
            }
        }
        for h in incoming {
            let idx = self.results.len();
            let same_file = matches!(self.rows.iter().rev().find(|r| matches!(r, Row::File { .. })),
                Some(Row::File { path, .. }) if *path == h.path);
            if same_file {
                if let Some(Row::File { count, .. }) = self.rows.iter_mut().rev().find(|r| matches!(r, Row::File { .. })) {
                    *count += 1;
                }
            } else {
                self.rows.push(Row::File { path: h.path.clone(), count: 1 });
            }
            self.rows.push(Row::Hit(idx));
            self.results.push(h);
        }
        if done {
            self.running = false;
            self.rx = None;
        }
    }

    pub fn file_count(&self) -> usize {
        self.rows.iter().filter(|r| matches!(r, Row::File { .. })).count()
    }

    /// Move keyboard selection to the next/previous hit row.
    pub fn step(&mut self, dir: isize) {
        let n = self.rows.len() as isize;
        if n == 0 {
            return;
        }
        let mut i = self.sel.map(|s| s as isize).unwrap_or(if dir > 0 { -1 } else { n });
        loop {
            i += dir;
            if i < 0 || i >= n {
                return;
            }
            if matches!(self.rows[i as usize], Row::Hit(_)) {
                self.sel = Some(i as usize);
                self.scroll_to_sel = true;
                return;
            }
        }
    }

    /// The selected hit, or the first one.
    pub fn selected_hit(&self) -> Option<&Hit> {
        match self.sel.and_then(|s| self.rows.get(s)) {
            Some(Row::Hit(i)) => self.results.get(*i),
            _ => self.rows.iter().find_map(|r| if let Row::Hit(i) = r { self.results.get(*i) } else { None }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "walks this machine's home folder; run with --ignored"]
    fn home_folder_walk_skips_app_data_and_cloud() {
        // VS_TEST_CWD picks the project folder (default: the home folder)
        let root = PathBuf::from(std::env::var("VS_TEST_CWD").or_else(|_| std::env::var("USERPROFILE")).unwrap());
        let t = std::time::Instant::now();
        let files = list_files(&root);
        eprintln!("files: {} in {:?}", files.len(), t.elapsed());
        assert!(!files.iter().any(|f| f.starts_with("AppData/") || f.starts_with("OneDrive/")));
        let t = std::time::Instant::now();
        let repos = crate::git::nested_repos(&root);
        eprintln!("nested repos: {repos:?} in {:?}", t.elapsed());
    }

    #[test]
    fn regex_options() {
        let r = build_regex("foo.bar", false, false, false).unwrap();
        assert!(r.is_match("x FOO.BAR y"));
        assert!(!r.is_match("fooxbar"));
        let r = build_regex("foo", true, true, false).unwrap();
        assert!(r.is_match("a foo b") && !r.is_match("foobar") && !r.is_match("Foo"));
        let r = build_regex(r"fn\s+\w+", false, false, true).unwrap();
        assert!(r.is_match("pub fn main()"));
        assert!(build_regex("(", false, false, true).is_err());
        let r = build_regex("привет", false, false, false).unwrap();
        assert!(r.is_match("ПРИВЕТ мир"));
    }

    #[test]
    fn masks() {
        let m = vec!["*.rs".to_owned(), "docs/".to_owned()];
        assert!(mask_matches(&m, "src/app.rs"));
        assert!(mask_matches(&m, "docs/a.md"));
        assert!(!mask_matches(&m, "src/a.ts"));
        assert!(mask_matches(&[], "anything"));
    }

    #[test]
    fn display() {
        let r = build_regex("needle", false, false, false).unwrap();
        let line = format!("    {}needle тест needle", "ы".repeat(100));
        let first = line.find("needle").unwrap();
        let (t, ranges) = display_line(&line, &r, first);
        assert!(t.starts_with('…'));
        assert_eq!(ranges.len(), 2);
        for (a, b) in ranges {
            assert_eq!(&t[a..b], "needle");
        }
        let (t, ranges) = display_line("  let needle = 1;", &r, 6);
        assert_eq!(t, "let needle = 1;");
        assert_eq!(&t[ranges[0].0..ranges[0].1], "needle");
    }
}
