//! Lazy project file tree with git status colors.

use crate::git::StatusKind;
use crate::theme::{Palette, hex};
use eframe::egui::{self, Align2, Color32, FontId, Sense, Stroke, Ui, pos2, vec2};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
}

pub enum TreeAction {
    Open(PathBuf),
    NewFile(PathBuf),
    NewFolder(PathBuf),
    Rename(PathBuf),
    Delete(PathBuf),
    Reveal(PathBuf),
    Terminal(PathBuf),
    CopyPath(String),
    AskClaude(PathBuf),
}

pub struct FileTree {
    pub root: PathBuf,
    expanded: HashSet<PathBuf>,
    cache: HashMap<PathBuf, Vec<Entry>>,
    pub selected: Option<PathBuf>,
}

const HIDDEN: &[&str] = &[".git"];

impl FileTree {
    pub fn new(root: PathBuf) -> Self {
        let mut expanded = HashSet::new();
        expanded.insert(root.clone());
        Self {
            root,
            expanded,
            cache: HashMap::new(),
            selected: None,
        }
    }

    pub fn refresh(&mut self) {
        self.cache.clear();
    }

    pub fn reveal(&mut self, path: &Path) {
        let mut p = path.parent();
        while let Some(d) = p {
            if !d.starts_with(&self.root) {
                break;
            }
            self.expanded.insert(d.to_path_buf());
            p = d.parent();
        }
        self.selected = Some(path.to_path_buf());
    }

    /// Where "new file" goes: the selected folder, the folder of the selected file, or the root.
    pub fn new_target_dir(&self) -> PathBuf {
        match &self.selected {
            Some(p) if p.is_dir() => p.clone(),
            Some(p) => p.parent().filter(|d| d.starts_with(&self.root)).unwrap_or(&self.root).to_path_buf(),
            None => self.root.clone(),
        }
    }

    fn children(&mut self, dir: &Path) -> &Vec<Entry> {
        if !self.cache.contains_key(dir) {
            let mut v: Vec<Entry> = std::fs::read_dir(dir)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .filter_map(|e| {
                            let name = e.file_name().to_string_lossy().into_owned();
                            if HIDDEN.contains(&name.as_str()) {
                                return None;
                            }
                            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                            Some(Entry {
                                path: e.path(),
                                name,
                                is_dir,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            v.sort_by(|a, b| {
                b.is_dir
                    .cmp(&a.is_dir)
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            });
            self.cache.insert(dir.to_path_buf(), v);
        }
        &self.cache[dir]
    }

    fn flatten(&mut self) -> Vec<(usize, Entry)> {
        let mut out = vec![];
        let root = self.root.clone();
        self.flatten_dir(&root, 0, &mut out);
        out
    }

    fn flatten_dir(&mut self, dir: &Path, depth: usize, out: &mut Vec<(usize, Entry)>) {
        let kids = self.children(dir).clone();
        for e in kids {
            let is_open = e.is_dir && self.expanded.contains(&e.path);
            let path = e.path.clone();
            out.push((depth, e));
            if is_open {
                self.flatten_dir(&path, depth + 1, out);
            }
            if out.len() > 200_000 {
                return;
            }
        }
    }

    pub fn ui(
        &mut self,
        ui: &mut Ui,
        pal: &Palette,
        status: &HashMap<String, StatusKind>,
        dirty_dirs: &HashSet<String>,
    ) -> Option<TreeAction> {
        let mut action = None;
        let rows = self.flatten();
        let row_h = 22.0;
        let font = FontId::proportional(ui.style().text_styles[&egui::TextStyle::Body].size);
        let small = FontId::proportional(9.5);
        let root = self.root.clone();

        // root row: name, path and the new file / folder buttons
        let target = self.new_target_dir();
        let resp = ui.horizontal(|ui| {
            ui.add_space(4.0);
            ui.label(egui::RichText::new(
                root.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            ).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let into = crate::util::rel_path(&root, &target);
                let into = if into.is_empty() { "the project root".to_owned() } else { format!("{into}/") };
                if ui.small_button("📁").on_hover_text(format!("New folder in {into}")).clicked() {
                    action = Some(TreeAction::NewFolder(target.clone()));
                }
                if ui.small_button("📄").on_hover_text(format!("New file in {into}  (Alt+Insert)")).clicked() {
                    action = Some(TreeAction::NewFile(target.clone()));
                }
                ui.add(egui::Label::new(egui::RichText::new(root.display().to_string()).small().color(pal.text_weak)).truncate());
            });
        }).response.interact(Sense::click());
        resp.context_menu(|ui| {
            if ui.button("📄  New File…").clicked() {
                action = Some(TreeAction::NewFile(root.clone()));
                ui.close();
            }
            if ui.button("📁  New Folder…").clicked() {
                action = Some(TreeAction::NewFolder(root.clone()));
                ui.close();
            }
            ui.separator();
            if ui.button("Copy Path").clicked() {
                action = Some(TreeAction::CopyPath(root.display().to_string()));
                ui.close();
            }
            if ui.button("Reveal in Explorer").clicked() {
                action = Some(TreeAction::Reveal(root.clone()));
                ui.close();
            }
            if ui.button("Open Terminal Here").clicked() {
                action = Some(TreeAction::Terminal(root.clone()));
                ui.close();
            }
        });

        egui::ScrollArea::vertical()
            .id_salt("tree_scroll")
            .auto_shrink([false, false])
            .show_rows(ui, row_h, rows.len(), |ui, range| {
                for (depth, e) in &rows[range] {
                    let width = ui.available_width().max(100.0);
                    let (rect, resp) = ui.allocate_exact_size(vec2(width, row_h), Sense::click());
                    let selected = self.selected.as_ref() == Some(&e.path);
                    let p = ui.painter();
                    if selected {
                        p.rect_filled(rect, 4.0, pal.selection);
                    } else if resp.hovered() {
                        p.rect_filled(rect, 4.0, pal.hover);
                    }
                    let x0 = rect.min.x + 6.0 + *depth as f32 * 16.0;
                    let cy = rect.center().y;
                    let rel = crate::util::rel_path(&root, &e.path);
                    let mut color = pal.text;
                    if e.is_dir {
                        let open = self.expanded.contains(&e.path);
                        // chevron
                        let c = pal.text_weak;
                        let s = Stroke::new(1.3, c);
                        if open {
                            p.line_segment([pos2(x0 + 1.0, cy - 2.0), pos2(x0 + 4.5, cy + 2.0)], s);
                            p.line_segment([pos2(x0 + 4.5, cy + 2.0), pos2(x0 + 8.0, cy - 2.0)], s);
                        } else {
                            p.line_segment([pos2(x0 + 2.5, cy - 4.0), pos2(x0 + 6.0, cy)], s);
                            p.line_segment([pos2(x0 + 6.0, cy), pos2(x0 + 2.5, cy + 4.0)], s);
                        }
                        // folder glyph
                        let fr = egui::Rect::from_min_size(pos2(x0 + 13.0, cy - 5.5), vec2(14.0, 11.0));
                        p.rect_filled(fr, 2.0, hex(0x8A9199).gamma_multiply(if open { 1.0 } else { 0.85 }));
                        p.rect_filled(
                            egui::Rect::from_min_size(pos2(x0 + 13.0, cy - 7.0), vec2(6.0, 3.0)),
                            1.0,
                            hex(0x8A9199),
                        );
                        if dirty_dirs.contains(&rel) {
                            color = pal.modified;
                        }
                    } else {
                        let (label, bg) = file_badge(&e.name);
                        let br = egui::Rect::from_min_size(pos2(x0 + 12.0, cy - 7.0), vec2(16.0, 14.0));
                        p.rect_filled(br, 3.0, bg);
                        p.text(br.center(), Align2::CENTER_CENTER, label, small.clone(), Color32::WHITE);
                        if let Some(st) = status.get(&rel) {
                            color = st.color(pal);
                        }
                    }
                    p.text(pos2(x0 + 33.0, cy), Align2::LEFT_CENTER, &e.name, font.clone(), color);

                    if resp.clicked() {
                        self.selected = Some(e.path.clone());
                        if e.is_dir {
                            if !self.expanded.remove(&e.path) {
                                self.expanded.insert(e.path.clone());
                            }
                        } else {
                            action = Some(TreeAction::Open(e.path.clone()));
                        }
                    }
                    let dir_for_new = if e.is_dir {
                        e.path.clone()
                    } else {
                        e.path.parent().unwrap_or(&root).to_path_buf()
                    };
                    resp.context_menu(|ui| {
                        self.selected = Some(e.path.clone());
                        if ui.button("📄  New File…").clicked() {
                            action = Some(TreeAction::NewFile(dir_for_new.clone()));
                            ui.close();
                        }
                        if ui.button("📁  New Folder…").clicked() {
                            action = Some(TreeAction::NewFolder(dir_for_new.clone()));
                            ui.close();
                        }
                        ui.separator();
                        if ui.button("✏  Rename…").clicked() {
                            action = Some(TreeAction::Rename(e.path.clone()));
                            ui.close();
                        }
                        if ui.button("🗑  Delete…").clicked() {
                            action = Some(TreeAction::Delete(e.path.clone()));
                            ui.close();
                        }
                        ui.separator();
                        if ui.button("🤖  Ask Claude about this").clicked() {
                            action = Some(TreeAction::AskClaude(e.path.clone()));
                            ui.close();
                        }
                        if ui.button("Copy Path").clicked() {
                            action = Some(TreeAction::CopyPath(e.path.display().to_string()));
                            ui.close();
                        }
                        if ui.button("Copy Relative Path").clicked() {
                            action = Some(TreeAction::CopyPath(rel.clone()));
                            ui.close();
                        }
                        if ui.button("Reveal in Explorer").clicked() {
                            action = Some(TreeAction::Reveal(e.path.clone()));
                            ui.close();
                        }
                        if ui.button("Open Terminal Here").clicked() {
                            action = Some(TreeAction::Terminal(dir_for_new.clone()));
                            ui.close();
                        }
                    });
                }
            });
        action
    }
}

/// Small colored badge per file type.
pub fn file_badge(name: &str) -> (&'static str, Color32) {
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "php" | "phtml" => ("P", hex(0x7A86B8)),
        "js" | "mjs" | "cjs" => ("JS", hex(0xC9A227)),
        "ts" | "mts" | "cts" => ("TS", hex(0x3178C6)),
        "tsx" | "jsx" => ("⚛", hex(0x2A9FBC)),
        "vue" => ("V", hex(0x41B883)),
        "rs" => ("R", hex(0xC0592E)),
        "py" => ("Py", hex(0x3572A5)),
        "go" => ("Go", hex(0x00ADD8)),
        "java" | "kt" | "kts" => ("J", hex(0xB07219)),
        "c" | "h" => ("C", hex(0x555E8C)),
        "cpp" | "hpp" | "cc" => ("C+", hex(0x9C4D7B)),
        "cs" => ("C#", hex(0x68217A)),
        "html" | "htm" | "twig" => ("<>", hex(0xD8643A)),
        "css" | "scss" | "less" | "sass" => ("#", hex(0x5B6FD1)),
        "json" | "jsonc" | "lock" => ("{}", hex(0x8E8C4A)),
        "md" | "markdown" | "mdx" => ("M", hex(0x5A6878)),
        "yml" | "yaml" | "toml" | "ini" | "env" | "xml" => ("⚙", hex(0x6C7A89)),
        "sql" => ("DB", hex(0xD48C2E)),
        "sh" | "bat" | "cmd" | "ps1" => (">", hex(0x4E8C5A)),
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "ico" | "bmp" => ("▣", hex(0x9E6CC4)),
        "rb" => ("Rb", hex(0xB2302A)),
        "swift" => ("S", hex(0xE85D3A)),
        "dart" => ("D", hex(0x0F8FC4)),
        _ => ("·", hex(0x6E7681)),
    }
}
