//! Open documents and the code editor widget.

use crate::highlight::{self, Highlighting, LineCache};
use crate::theme::Palette;
use eframe::egui::{
    self, Align, Align2, FontId, Frame, Galley, Id, Key, Rect, RichText, Sense,
    Shape, TextEdit, Ui, UiBuilder, pos2,
    text::{CCursor, CCursorRange},
    text_edit::TextEditState,
    vec2,
};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

const IMAGE_EXT: &[&str] = &["png", "jpg", "jpeg", "gif", "bmp", "webp", "ico"];
const MARKDOWN_EXT: &[&str] = &["md", "markdown", "mdx"];
/// Readable line length for rendered markdown.
const PREVIEW_MAX_W: f32 = 860.0;

#[derive(PartialEq)]
pub enum DocKind {
    Text,
    Diff,
    Image,
    Binary(String),
}

pub struct FindState {
    pub query: String,
    pub case: bool,
    /// match ranges in char indices
    pub matches: Vec<(usize, usize)>,
    pub current: usize,
    computed: Option<(u64, String, bool)>,
    pub focus: bool,
}

struct Memo {
    text: String,
    size: f32,
    dark: bool,
    galley: Arc<Galley>,
}

pub struct Doc {
    pub path: PathBuf,
    pub title: String,
    pub kind: DocKind,
    pub text: String,
    pub dirty: bool,
    /// last unsaved edit, for auto-save
    pub edited_at: Option<Instant>,
    saved_hash: u64,
    crlf: bool,
    bom: bool,
    pub mtime: Option<SystemTime>,
    pub changed_on_disk: bool,
    pub syntax: Option<String>,
    hl: LineCache,
    memo: Option<Memo>,
    pub version: u64,
    /// (start, end) char range to select & reveal next frame
    pub goto: Option<(usize, usize)>,
    scroll_to: Option<usize>,
    pub cursor: (usize, usize),
    cursor_ci: usize,
    goto_keep_focus: bool,
    undo_set: bool,
    pub id: Id,
    pub find: Option<FindState>,
    image: Option<Result<egui::TextureHandle, String>>,
    /// Markdown files open rendered; `preview` switches between rendered and source.
    pub markdown: bool,
    pub preview: bool,
    md_cache: egui_commonmark::CommonMarkCache,
}

fn hash_str(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn mtime_of(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

impl Doc {
    pub fn open(path: &Path, hl: &Highlighting) -> Doc {
        let title = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let mut doc = Doc {
            path: path.to_path_buf(),
            title,
            kind: DocKind::Text,
            text: String::new(),
            dirty: false,
            edited_at: None,
            saved_hash: 0,
            crlf: false,
            bom: false,
            mtime: mtime_of(path),
            changed_on_disk: false,
            syntax: None,
            hl: LineCache::default(),
            memo: None,
            version: 0,
            goto: None,
            scroll_to: None,
            cursor: (1, 1),
            cursor_ci: 0,
            goto_keep_focus: false,
            undo_set: false,
            id: Id::new(("doc", path.to_path_buf())),
            find: None,
            image: None,
            markdown: false,
            preview: false,
            md_cache: Default::default(),
        };
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if IMAGE_EXT.contains(&ext.as_str()) {
            doc.kind = DocKind::Image;
            return doc;
        }
        doc.markdown = MARKDOWN_EXT.contains(&ext.as_str());
        doc.preview = doc.markdown;
        doc.load_text(hl);
        doc
    }

    pub fn virtual_doc(title: &str, key: &str, text: String) -> Doc {
        Doc {
            path: PathBuf::from(format!("vibezen://{key}")),
            title: title.to_owned(),
            kind: DocKind::Diff,
            saved_hash: 0,
            text,
            dirty: false,
            edited_at: None,
            crlf: false,
            bom: false,
            mtime: None,
            changed_on_disk: false,
            syntax: None,
            hl: LineCache::default(),
            memo: None,
            version: 0,
            goto: None,
            scroll_to: None,
            cursor: (1, 1),
            cursor_ci: 0,
            goto_keep_focus: false,
            undo_set: true,
            id: Id::new(("vdoc", key.to_owned())),
            find: None,
            image: None,
            markdown: false,
            preview: false,
            md_cache: Default::default(),
        }
    }

    pub fn is_virtual(&self) -> bool {
        self.kind == DocKind::Diff
    }

    fn load_text(&mut self, hl: &Highlighting) {
        match std::fs::read(&self.path) {
            Err(e) => self.kind = DocKind::Binary(format!("Cannot open file: {e}")),
            Ok(bytes) => {
                if bytes.len() > 64 * 1024 * 1024 {
                    self.kind = DocKind::Binary("File is too large to open (> 64 MB).".into());
                    return;
                }
                if bytes[..bytes.len().min(8000)].contains(&0) {
                    self.kind = DocKind::Binary("Binary file — not shown.".into());
                    return;
                }
                let mut text = match String::from_utf8(bytes) {
                    Ok(s) => s,
                    Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
                };
                self.bom = text.starts_with('\u{feff}');
                if self.bom {
                    text.remove(0);
                }
                self.crlf = text.contains("\r\n");
                if self.crlf {
                    text = text.replace("\r\n", "\n");
                }
                let first = text.lines().next().unwrap_or("");
                self.syntax = if text.len() < 8 * 1024 * 1024 {
                    hl.syntax_for(&self.path, first)
                } else {
                    None
                };
                self.saved_hash = hash_str(&text);
                self.text = text;
                self.kind = DocKind::Text;
                self.dirty = false;
                self.edited_at = None;
                self.changed_on_disk = false;
                self.version += 1;
                self.mtime = mtime_of(&self.path);
            }
        }
    }

    pub fn save(&mut self) -> Result<(), String> {
        if self.kind != DocKind::Text || !self.dirty {
            return Ok(());
        }
        let mut out = if self.crlf {
            self.text.replace('\n', "\r\n")
        } else {
            self.text.clone()
        };
        if self.bom {
            out.insert(0, '\u{feff}');
        }
        std::fs::write(&self.path, out).map_err(|e| format!("Saving {} failed: {e}", self.title))?;
        self.saved_hash = hash_str(&self.text);
        self.dirty = false;
        self.edited_at = None;
        self.changed_on_disk = false;
        self.mtime = mtime_of(&self.path);
        Ok(())
    }

    /// Save unless the file was changed or removed on disk under our edits —
    /// an agent's write is never overwritten silently. `Ok(true)` = written.
    pub fn auto_save(&mut self, hl: &Highlighting) -> Result<bool, String> {
        if self.kind != DocKind::Text || !self.dirty {
            return Ok(false);
        }
        self.check_disk(hl);
        if self.changed_on_disk || !self.path.is_file() {
            return Ok(false);
        }
        self.save().map(|_| true)
    }

    /// Reload from disk if the file changed and we have no unsaved edits.
    pub fn check_disk(&mut self, hl: &Highlighting) -> bool {
        if self.is_virtual() {
            return false;
        }
        let m = mtime_of(&self.path);
        if m.is_none() || m == self.mtime {
            return false;
        }
        match self.kind {
            DocKind::Image => {
                self.image = None;
                self.mtime = m;
                true
            }
            _ if self.dirty => {
                self.changed_on_disk = true;
                false
            }
            _ => {
                self.load_text(hl);
                true
            }
        }
    }

    pub fn reload(&mut self, hl: &Highlighting) {
        self.load_text(hl);
    }

    pub fn line_ending(&self) -> &'static str {
        if self.crlf { "CRLF" } else { "LF" }
    }

    /// Jump to 1-based line (and optional 1-based column).
    pub fn goto_line(&mut self, line: usize, col: usize) {
        let mut idx = 0usize;
        for (i, l) in self.text.split('\n').enumerate() {
            if i + 1 == line {
                idx += l.chars().count().min(col.saturating_sub(1));
                break;
            }
            idx += l.chars().count() + 1;
        }
        self.goto = Some((idx, idx));
    }

    /// Jump to 1-based line/column and select `len` chars (a search match).
    pub fn select_at(&mut self, line: usize, col: usize, len: usize) {
        self.goto_line(line, col);
        if let Some((a, _)) = self.goto {
            self.goto = Some((a, a + len));
        }
    }

    /// Current single-line selection in the editor (used to prefill searches).
    pub fn selected_text(&self, ctx: &egui::Context) -> Option<String> {
        let st = TextEditState::load(ctx, self.id)?;
        let r = st.cursor.char_range()?;
        let (a, b) = (r.primary.index.0, r.secondary.index.0);
        let (a, b) = (a.min(b), a.max(b));
        if a == b || b - a > 300 {
            return None;
        }
        let s: String = self.text.chars().skip(a).take(b - a).collect();
        (!s.contains('\n') && !s.trim().is_empty()).then_some(s)
    }

    pub fn open_find(&mut self) {
        // find works on the source text
        self.preview = false;
        match &mut self.find {
            Some(f) => f.focus = true,
            None => {
                self.find = Some(FindState {
                    query: String::new(),
                    case: false,
                    matches: vec![],
                    current: 0,
                    computed: None,
                    focus: true,
                })
            }
        }
    }
}

pub struct EditorCtx<'a> {
    pub hl: &'a Highlighting,
    pub pal: &'a Palette,
    pub dark: bool,
    pub font_size: f32,
}

pub fn show_doc(ui: &mut Ui, doc: &mut Doc, cx: &EditorCtx) {
    match &doc.kind {
        DocKind::Image => show_image(ui, doc, cx),
        DocKind::Binary(msg) => {
            let msg = msg.clone();
            ui.centered_and_justified(|ui| ui.label(RichText::new(msg).color(cx.pal.text_weak)));
        }
        DocKind::Text if doc.markdown => {
            // jumping to a line (search result, go to line) needs the source view
            if doc.goto.is_some() {
                doc.preview = false;
            }
            markdown_bar(ui, doc, cx);
            if doc.preview {
                show_markdown(ui, doc, cx);
            } else {
                show_text(ui, doc, cx);
            }
        }
        DocKind::Text | DocKind::Diff => show_text(ui, doc, cx),
    }
}

/// Preview / Source switch above a markdown document.
fn markdown_bar(ui: &mut Ui, doc: &mut Doc, cx: &EditorCtx) {
    Frame::NONE
        .fill(cx.pal.panel_bg)
        .inner_margin(egui::Margin::symmetric(8, 3))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                if ui.selectable_label(doc.preview, "👁 Preview").clicked() && !doc.preview {
                    doc.preview = true;
                    doc.find = None;
                }
                if ui.selectable_label(!doc.preview, "✎ Source").clicked() && doc.preview {
                    doc.preview = false;
                }
            });
        });
}

fn show_markdown(ui: &mut Ui, doc: &mut Doc, cx: &EditorCtx) {
    let avail = ui.available_rect_before_wrap();
    ui.painter().rect_filled(avail, 0.0, cx.pal.editor_bg);
    egui::ScrollArea::vertical()
        .id_salt(doc.id.with("preview"))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let w = ui.available_width();
            let col = (w - 48.0).min(PREVIEW_MAX_W).max(200.0);
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                ui.add_space(((w - col) / 2.0).max(0.0));
                ui.vertical(|ui| {
                    ui.set_width(col);
                    // the UI theme's Heading is only body + 3; documents want a real h1…h6 scale
                    let style = ui.style_mut();
                    let body = style.text_styles.get(&egui::TextStyle::Body).map_or(14.0, |f| f.size);
                    style.text_styles.insert(egui::TextStyle::Heading, FontId::proportional(body * 2.0));
                    style.override_text_valign = Some(Align::BOTTOM);
                    egui_commonmark::CommonMarkViewer::new().show(ui, &mut doc.md_cache, &doc.text);
                });
            });
            ui.add_space(32.0);
        });
}

fn show_image(ui: &mut Ui, doc: &mut Doc, cx: &EditorCtx) {
    if doc.image.is_none() {
        let res = std::fs::read(&doc.path)
            .map_err(|e| e.to_string())
            .and_then(|b| image::load_from_memory(&b).map_err(|e| e.to_string()))
            .map(|img| {
                let rgba = img.to_rgba8();
                let size = [rgba.width() as usize, rgba.height() as usize];
                let ci = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
                ui.ctx().load_texture(doc.path.to_string_lossy(), ci, egui::TextureOptions::LINEAR)
            });
        doc.image = Some(res);
    }
    match doc.image.as_ref().unwrap() {
        Ok(tex) => {
            let size = tex.size_vec2();
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new(format!("{} × {} px", size.x as u32, size.y as u32))
                        .color(cx.pal.text_weak)
                        .small(),
                );
                egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                    let max = ui.available_size();
                    let scale = (max.x / size.x).min(max.y / size.y).min(1.0);
                    ui.add(egui::Image::from_texture((tex.id(), size * scale)));
                });
            });
        }
        Err(e) => {
            ui.label(RichText::new(format!("Cannot display image: {e}")).color(cx.pal.error));
        }
    }
}

fn find_bar(ui: &mut Ui, doc: &mut Doc, cx: &EditorCtx) {
    let mut close = false;
    let mut step: i32 = 0;
    {
        let f = doc.find.as_mut().unwrap();
        Frame::NONE
            .fill(cx.pal.panel_bg)
            .inner_margin(egui::Margin::symmetric(8, 4))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("🔍");
                    let id = doc.id.with("find_input");
                    let enter = ui.memory(|m| m.has_focus(id))
                        && ui.input(|i| i.key_pressed(Key::Enter));
                    let shift = ui.input(|i| i.modifiers.shift);
                    let r = ui.add(
                        TextEdit::singleline(&mut f.query)
                            .id(id)
                            .hint_text("Find in file")
                            .desired_width(260.0),
                    );
                    if f.focus {
                        r.request_focus();
                        f.focus = false;
                    }
                    if enter || r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        step = if shift { -1 } else { 1 };
                        r.request_focus();
                    }
                    ui.checkbox(&mut f.case, "Aa").on_hover_text("Match case");
                    let label = if f.query.is_empty() {
                        String::new()
                    } else if f.matches.is_empty() {
                        "0 results".into()
                    } else {
                        format!("{}/{}", f.current + 1, f.matches.len())
                    };
                    ui.label(RichText::new(label).color(cx.pal.text_weak));
                    if ui.small_button("↑").on_hover_text("Previous (Shift+Enter)").clicked() {
                        step = -1;
                    }
                    if ui.small_button("↓").on_hover_text("Next (Enter)").clicked() {
                        step = 1;
                    }
                    if ui.small_button("✕").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        close = true;
                    }
                });
            });
    }
    if close {
        doc.find = None;
        doc.goto = Some((doc.cursor_ci, doc.cursor_ci));
        return;
    }
    // recompute matches when needed
    let f = doc.find.as_mut().unwrap();
    let key = (doc.version, f.query.clone(), f.case);
    if f.computed.as_ref() != Some(&key) {
        f.matches = find_matches(&doc.text, &f.query, f.case);
        f.current = f.current.min(f.matches.len().saturating_sub(1));
        f.computed = Some(key);
        if step == 0 && !f.matches.is_empty() {
            step = 2; // reveal current without moving
        }
    }
    if !f.matches.is_empty() && step != 0 {
        let n = f.matches.len();
        if step == 1 {
            f.current = (f.current + 1) % n;
        } else if step == -1 {
            f.current = (f.current + n - 1) % n;
        }
        let (a, b) = f.matches[f.current];
        doc.goto = Some((a, b));
        // keep focus in the find box
        doc.goto_keep_focus = true;
    }
}

fn find_matches(text: &str, q: &str, case: bool) -> Vec<(usize, usize)> {
    if q.is_empty() {
        return vec![];
    }
    let (hay, needle) = if case {
        (std::borrow::Cow::Borrowed(text), q.to_owned())
    } else {
        (std::borrow::Cow::Owned(text.to_ascii_lowercase()), q.to_ascii_lowercase())
    };
    let qlen = needle.chars().count();
    let mut out = vec![];
    // convert byte offsets to char offsets in one pass
    let mut last_b = 0usize;
    let mut last_c = 0usize;
    for (b, _) in hay.match_indices(needle.as_str()) {
        last_c += hay[last_b..b].chars().count();
        last_b = b;
        out.push((last_c, last_c + qlen));
        if out.len() >= 20_000 {
            break;
        }
    }
    out
}

fn line_col(galley: &Galley, ci: usize) -> (usize, usize) {
    let mut rem = ci;
    let n = galley.rows.len();
    for (i, row) in galley.rows.iter().enumerate() {
        let c = row.char_count_including_newline().0;
        if rem < c || i + 1 == n {
            return (i + 1, rem + 1);
        }
        rem -= c;
    }
    (1, 1)
}

fn show_text(ui: &mut Ui, doc: &mut Doc, cx: &EditorCtx) {
    let ctx = ui.ctx().clone();
    let pal = cx.pal;

    if doc.changed_on_disk {
        Frame::NONE
            .fill(pal.warn.gamma_multiply(0.25))
            .inner_margin(egui::Margin::symmetric(8, 4))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("File changed on disk, but you have unsaved changes.");
                    if ui.button("Load disk version").clicked() {
                        doc.reload(cx.hl);
                    }
                    if ui.button("Keep mine").clicked() {
                        doc.changed_on_disk = false;
                        doc.mtime = mtime_of(&doc.path);
                    }
                });
            });
    }
    if doc.find.is_some() {
        find_bar(ui, doc, cx);
    }

    let font = FontId::monospace(cx.font_size);
    let (row_h, char_w) = ui.fonts_mut(|f| (f.row_height(&font), f.glyph_width(&font, '0')));
    let line_count = doc.memo.as_ref().map(|m| m.galley.rows.len()).unwrap_or(1).max(1);
    let digits = line_count.to_string().len().max(3);
    let gutter_w = digits as f32 * char_w + 24.0;

    let avail = ui.available_rect_before_wrap();
    ui.painter().rect_filled(avail, 0.0, pal.editor_bg);
    let gutter = Rect::from_min_size(avail.min, vec2(gutter_w, avail.height()));
    let text_rect = Rect::from_min_max(pos2(gutter.max.x, avail.min.y), avail.max);

    let keep_focus = std::mem::take(&mut doc.goto_keep_focus);
    if let Some((a, b)) = doc.goto.take() {
        let mut st = TextEditState::load(&ctx, doc.id).unwrap_or_default();
        st.cursor
            .set_char_range(Some(CCursorRange::two(CCursor::new(a), CCursor::new(b))));
        st.store(&ctx, doc.id);
        if !keep_focus {
            ctx.memory_mut(|m| m.request_focus(doc.id));
        }
        doc.scroll_to = Some(a);
    }

    let read_only = doc.kind == DocKind::Diff;
    let doc_id = doc.id;
    let mut result: Option<(Arc<Galley>, egui::Pos2, Option<CCursorRange>, bool)> = None;

    ui.scope_builder(UiBuilder::new().max_rect(text_rect), |ui| {
        egui::ScrollArea::both()
            .id_salt(doc_id.with("scroll"))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let bg_idx = ui.painter().add(Shape::Noop);
                let Doc {
                    text,
                    hl: cache,
                    memo,
                    syntax,
                    kind,
                    find,
                    scroll_to,
                    ..
                } = doc;
                let is_diff = *kind == DocKind::Diff;
                let font2 = font.clone();
                let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, _w: f32| -> Arc<Galley> {
                    let s = buf.as_str();
                    if let Some(m) = memo.as_ref() {
                        if m.size == cx.font_size && m.dark == cx.dark && m.text == s {
                            return m.galley.clone();
                        }
                    }
                    let job = if is_diff {
                        highlight::diff_job(s, font2.clone(), pal)
                    } else if let Some(sr) = syntax.as_ref().and_then(|n| cx.hl.ss.find_syntax_by_name(n)) {
                        cache.update(cx.hl, sr, cx.dark, s);
                        cache.layout_job(s, font2.clone(), pal.text)
                    } else {
                        highlight::plain_job(s, font2.clone(), pal.text)
                    };
                    let galley = ui.fonts_mut(|f| f.layout_job(job));
                    *memo = Some(Memo {
                        text: s.to_owned(),
                        size: cx.font_size,
                        dark: cx.dark,
                        galley: galley.clone(),
                    });
                    galley
                };
                let min = ui.available_size();
                let mut ro: &str = "";
                let buffer: &mut dyn egui::TextBuffer = if read_only {
                    ro = text.as_str();
                    &mut ro
                } else {
                    text
                };
                let output = TextEdit::multiline(buffer)
                    .id(doc_id)
                    .font(font.clone())
                    .code_editor()
                    .lock_focus(true)
                    .frame(Frame::NONE)
                    .desired_width(f32::INFINITY)
                    .min_size(min)
                    .layouter(&mut layouter)
                    .show(ui);

                let changed = output.response.changed();
                let galley = output.galley.clone();
                let gpos = output.galley_pos;

                // background shapes: current line + find matches
                let mut shapes = vec![];
                let clip = ui.clip_rect();
                if let Some(cr) = output.cursor_range {
                    let r = galley.pos_from_cursor(cr.primary).translate(gpos.to_vec2());
                    shapes.push(Shape::rect_filled(
                        Rect::from_x_y_ranges(clip.x_range(), r.y_range()),
                        0.0,
                        pal.current_line,
                    ));
                }
                if let Some(f) = find.as_ref() {
                    let top = clip.min.y - gpos.y;
                    let bottom = clip.max.y - gpos.y;
                    for &(a, b) in &f.matches {
                        let ra = galley.pos_from_cursor(CCursor::new(a));
                        if ra.max.y < top {
                            continue;
                        }
                        if ra.min.y > bottom {
                            break;
                        }
                        let rb = galley.pos_from_cursor(CCursor::new(b));
                        let rect = Rect::from_min_max(ra.min, pos2(rb.max.x.max(ra.min.x + 2.0), ra.max.y))
                            .translate(gpos.to_vec2());
                        shapes.push(Shape::rect_filled(rect, 2.0, pal.find_match));
                    }
                }
                ui.painter().set(bg_idx, Shape::Vec(shapes));

                if let Some(ci) = scroll_to.take() {
                    let r = galley.pos_from_cursor(CCursor::new(ci)).translate(gpos.to_vec2());
                    ui.scroll_to_rect(r.expand(row_h * 2.0), Some(Align::Center));
                }
                result = Some((galley, gpos, output.cursor_range, changed));
            });
    });

    let Some((galley, gpos, cursor_range, changed)) = result else { return };

    if changed && !read_only {
        doc.version += 1;
        doc.dirty = hash_str(&doc.text) != doc.saved_hash;
        doc.edited_at = doc.dirty.then(Instant::now);
    }
    if !doc.undo_set && !read_only {
        if let Some(mut st) = TextEditState::load(&ctx, doc_id) {
            let big = doc.text.len() > 300_000;
            st.set_undoer(egui::util::undoer::Undoer::with_settings(egui::util::undoer::Settings {
                max_undos: if big { 15 } else { 200 },
                ..Default::default()
            }));
            st.store(&ctx, doc_id);
            doc.undo_set = true;
        }
    }
    let cur_line = cursor_range.map(|cr| {
        doc.cursor_ci = cr.primary.index.0;
        let lc = line_col(&galley, cr.primary.index.0);
        doc.cursor = lc;
        lc.0
    });

    // gutter with line numbers (fixed, not scrolled horizontally)
    let painter = ui.painter_at(gutter);
    painter.rect_filled(gutter, 0.0, pal.editor_bg);
    let rows = &galley.rows;
    if !rows.is_empty() {
        let first = (((gutter.min.y - gpos.y) / row_h).floor().max(0.0) as usize).min(rows.len() - 1);
        let last = (first + (gutter.height() / row_h).ceil() as usize + 2).min(rows.len());
        let num_font = FontId::monospace(cx.font_size - 1.0);
        for (i, row) in rows.iter().enumerate().take(last).skip(first) {
            let y = gpos.y + row.pos.y;
            let color = if Some(i + 1) == cur_line { pal.gutter_active } else { pal.gutter_text };
            painter.text(
                pos2(gutter.max.x - 12.0, y + row_h * 0.5),
                Align2::RIGHT_CENTER,
                (i + 1).to_string(),
                num_font.clone(),
                color,
            );
        }
    }
    painter.line_segment(
        [pos2(gutter.max.x - 4.0, gutter.min.y), pos2(gutter.max.x - 4.0, gutter.max.y)],
        egui::Stroke::new(1.0, pal.border.gamma_multiply(0.5)),
    );
    let _ = ui.allocate_rect(avail, Sense::hover());
}

