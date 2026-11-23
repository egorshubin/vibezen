//! The Claude Code tool window, styled after the Claude Code VS Code extension:
//! conversation header with history, a timeline of messages and tool calls with
//! inline diffs, and a rounded input box holding mode / model / attach / send.

use crate::claude::{self, Chat, Item, MODELS, MODES, PermState};
use crate::theme::{Palette, hex, tool_btn};
use eframe::egui::{
    self, Align, Align2, Color32, FontId, Frame, Id, Key, Layout, Margin, Modifiers, RichText, Sense, Stroke, Ui, pos2,
    vec2,
};
use serde_json::Value;
use std::path::PathBuf;

/// Claude brand orange
pub const CLAUDE: Color32 = hex(0xD97757);

pub enum ChatAction {
    OpenFile(PathBuf),
    /// new default permission mode for every project (saved in settings)
    SetMode(String),
    /// new default model for every project (saved in settings)
    SetModel(String),
    /// start a fresh conversation in its own tab
    NewSession,
    /// show a stored conversation (in its tab if one is open, else in a new one)
    OpenSession(String),
}

enum Act {
    Allow(usize, bool, Option<&'static str>),
    Deny(usize),
    Pick(usize, String, String, bool),
    Submit(usize, bool),
    Open(PathBuf),
}

fn spark(t: f64) -> &'static str {
    const F: [&str; 6] = ["·", "✢", "✳", "✶", "✻", "✽"];
    F[((t * 7.0) as usize) % F.len()]
}

fn mode_label(mode: &str) -> String {
    let icon = match mode {
        "acceptEdits" => "⏵⏵",
        "plan" => "⏸",
        "bypassPermissions" => "⚠",
        "auto" => "⏵",
        _ => "✋",
    };
    let name = MODES.iter().find(|m| m.0 == mode).map(|m| m.1).unwrap_or(mode);
    format!("{icon} {name}")
}

/// A small rounded "chip" button used in the input footer.
fn chip(ui: &mut Ui, pal: &Palette, text: &str, tip: &str) -> egui::Response {
    let font = FontId::proportional(12.0);
    let g = ui.painter().layout_no_wrap(text.to_owned(), font, pal.text_weak);
    let size = vec2(g.size().x + 14.0, 22.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let col = if resp.hovered() { pal.text } else { pal.text_weak };
    if resp.hovered() {
        ui.painter().rect_filled(rect, 11.0, pal.hover);
    }
    ui.painter().galley_with_override_text_color(rect.center() - g.size() / 2.0, g, col);
    resp.on_hover_text(tip)
}

/// An attached document: icon and name in a rounded frame.
fn file_chip(ui: &mut Ui, pal: &Palette, name: &str, removable: bool) -> egui::Response {
    let font = FontId::proportional(12.0);
    let g = ui.painter().layout_no_wrap(format!("📄 {}", crate::util::truncate(name, 40)), font, pal.text);
    let size = vec2(g.size().x + 16.0, if removable { 48.0 } else { 24.0 });
    let (rect, resp) = ui.allocate_exact_size(size, if removable { Sense::click() } else { Sense::hover() });
    let fill = if removable && resp.hovered() { pal.hover } else { pal.button_bg };
    ui.painter().rect(rect, 6.0, fill, Stroke::new(1.0, pal.border), egui::StrokeKind::Inside);
    ui.painter().galley(rect.center() - g.size() / 2.0, g, pal.text);
    resp
}

/// Commands matching what is typed after the `/`: name prefix first, then anywhere in the name.
fn command_matches(cmds: &[claude::SlashCommand], q: &str) -> Vec<(String, String)> {
    let q = q.to_lowercase();
    let mut v: Vec<(bool, &claude::SlashCommand)> = cmds
        .iter()
        .filter_map(|c| {
            let n = c.name.to_lowercase();
            if n.starts_with(&q) {
                Some((false, c))
            } else if n.contains(&q) {
                Some((true, c))
            } else {
                None
            }
        })
        .collect();
    v.sort_by_key(|m| m.0);
    v.into_iter().map(|(_, c)| (c.name.clone(), c.desc.clone())).collect()
}

/// The input box text: a known `/command` at the start and `@file` mentions are coloured.
fn input_job(ui: &Ui, pal: &Palette, text: &str, commands: &std::collections::HashSet<String>) -> egui::text::LayoutJob {
    let font = egui::FontSelection::Default.resolve(ui.style());
    let base = ui.visuals().override_text_color.unwrap_or_else(|| ui.visuals().widgets.inactive.text_color());
    let mut job = egui::text::LayoutJob::default();
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        let len = match rest.find(|c: char| c.is_whitespace() != rest.starts_with(char::is_whitespace)) {
            Some(n) => n,
            None => rest.len(),
        };
        let word = &rest[..len];
        let col = if i == 0 && word.strip_prefix('/').is_some_and(|c| commands.contains(c)) {
            CLAUDE
        } else if word.len() > 1 && word.starts_with('@') {
            pal.link
        } else {
            base
        };
        job.append(word, 0.0, egui::TextFormat::simple(font.clone(), col));
        i += len;
    }
    job
}

fn code_block(ui: &mut Ui, pal: &Palette, label: &str, text: &str, max_lines: usize) {
    let mut s: String = text.lines().take(max_lines).collect::<Vec<_>>().join("\n");
    let total = text.lines().count();
    if total > max_lines {
        s.push_str(&format!("\n… {} more lines", total - max_lines));
    }
    ui.horizontal_top(|ui| {
        ui.add_sized(vec2(30.0, 16.0), egui::Label::new(RichText::new(label).small().color(pal.text_weak)));
        Frame::NONE
            .fill(ui.visuals().code_bg_color)
            .corner_radius(4)
            .inner_margin(Margin::symmetric(6, 4))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.add(egui::Label::new(RichText::new(s).monospace().size(11.5).color(pal.text)).wrap().selectable(true));
            });
    });
}

/// Claude's markdown. egui_commonmark lays out every styled run as its own label and
/// the wrapping row centres them, which lifts monospace `code` above the text line;
/// bottom alignment puts Segoe UI and the monospace font on the same baseline.
fn markdown(ui: &mut Ui, cache: &mut egui_commonmark::CommonMarkCache, text: &str) {
    ui.scope(|ui| {
        ui.style_mut().override_text_valign = Some(Align::BOTTOM);
        egui_commonmark::CommonMarkViewer::new().show(ui, cache, text);
    });
}

/// Y of the first text line's baseline within a galley.
fn first_baseline(g: &egui::Galley) -> f32 {
    g.rows
        .first()
        .and_then(|r| r.row.glyphs.first().map(|gl| r.pos.y + gl.pos.y))
        .unwrap_or(g.size().y * 0.8)
}

/// Line diff by trimming the common head/tail (good enough for Edit tool hunks).
fn diff_lines<'a>(old: &'a str, new: &'a str) -> Vec<(char, &'a str)> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let mut pre = 0;
    while pre < a.len() && pre < b.len() && a[pre] == b[pre] {
        pre += 1;
    }
    let mut suf = 0;
    while suf < a.len() - pre && suf < b.len() - pre && a[a.len() - 1 - suf] == b[b.len() - 1 - suf] {
        suf += 1;
    }
    let mut out = vec![];
    for l in &a[pre.saturating_sub(2)..pre] {
        out.push((' ', *l));
    }
    for l in &a[pre..a.len() - suf] {
        out.push(('-', *l));
    }
    for l in &b[pre..b.len() - suf] {
        out.push(('+', *l));
    }
    for l in a[a.len() - suf..].iter().take(2) {
        out.push((' ', *l));
    }
    out
}

fn diff_view(ui: &mut Ui, pal: &Palette, file: &str, hunks: &[(&str, &str)]) {
    Frame::NONE
        .stroke(Stroke::new(1.0, pal.border))
        .corner_radius(6)
        .inner_margin(Margin::same(0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            Frame::NONE.inner_margin(Margin::symmetric(8, 4)).show(ui, |ui| {
                ui.label(RichText::new(file).small().monospace().color(pal.text_weak));
            });
            let font = FontId::monospace(11.5);
            let row_h = 16.0;
            let mut shown = 0;
            for (i, (old, new)) in hunks.iter().enumerate() {
                if i > 0 {
                    ui.add_space(3.0);
                }
                for (k, line) in diff_lines(old, new) {
                    if shown >= 60 {
                        break;
                    }
                    shown += 1;
                    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::hover());
                    let p = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
                    let (bg, sign_col) = match k {
                        '-' => (Some(pal.diff_del_bg), pal.deleted),
                        '+' => (Some(pal.diff_add_bg), pal.added),
                        _ => (None, pal.text_weak),
                    };
                    if let Some(bg) = bg {
                        p.rect_filled(rect, 0.0, bg);
                    }
                    p.text(pos2(rect.min.x + 6.0, rect.center().y), Align2::LEFT_CENTER, k.to_string(), font.clone(), sign_col);
                    p.text(pos2(rect.min.x + 20.0, rect.center().y), Align2::LEFT_CENTER, line, font.clone(), pal.text);
                }
            }
            if shown >= 60 {
                ui.label(RichText::new("   …").small().color(pal.text_weak));
            }
            ui.add_space(3.0);
        });
}

/// Expanded body of a tool call.
fn tool_body(ui: &mut Ui, pal: &Palette, name: &str, input: &Value, result: &Option<(String, bool)>, cwd: &std::path::Path) {
    let file = || {
        let p = input["file_path"].as_str().unwrap_or("");
        PathBuf::from(p).strip_prefix(cwd).map(|r| r.to_string_lossy().replace('\\', "/")).unwrap_or(p.to_owned())
    };
    match name {
        "Edit" => diff_view(
            ui,
            pal,
            &file(),
            &[(input["old_string"].as_str().unwrap_or(""), input["new_string"].as_str().unwrap_or(""))],
        ),
        "MultiEdit" => {
            let hunks: Vec<(&str, &str)> = input["edits"]
                .as_array()
                .map(|a| a.iter().map(|e| (e["old_string"].as_str().unwrap_or(""), e["new_string"].as_str().unwrap_or(""))).collect())
                .unwrap_or_default();
            diff_view(ui, pal, &file(), &hunks);
        }
        "Write" => diff_view(ui, pal, &file(), &[("", input["content"].as_str().unwrap_or(""))]),
        "Bash" | "PowerShell" => code_block(ui, pal, "IN", input["command"].as_str().unwrap_or(""), 30),
        "Read" | "Glob" | "Grep" | "TodoWrite" => {}
        _ => code_block(ui, pal, "IN", &serde_json::to_string_pretty(input).unwrap_or_default(), 30),
    }
    if let Some((r, is_err)) = result {
        if !matches!(name, "Edit" | "MultiEdit" | "Write" | "TodoWrite") || *is_err {
            code_block(ui, pal, if *is_err { "ERR" } else { "OUT" }, r, if name == "Read" { 12 } else { 30 });
        }
    }
}

pub fn show(
    ui: &mut Ui,
    chat: &mut Chat,
    pal: &Palette,
    last_session: Option<String>,
    active_file: Option<String>,
) -> Vec<ChatAction> {
    let mut out = vec![];
    let mut acts: Vec<Act> = vec![];
    let now = ui.input(|i| i.time);
    let cwd = chat.cwd.clone();
    let uid = chat.uid;

    // ---------------- header: conversation title + history + new
    egui::Panel::top("claude_header")
        .frame(Frame::NONE.inner_margin(Margin::symmetric(8, 5)))
        .show_separator_line(true)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("✻").color(CLAUDE).size(16.0));
                let title = chat.title().unwrap_or_else(|| "New conversation".into());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 1.0;
                    if tool_btn(ui, pal, "✚", "New session", true) {
                        out.push(ChatAction::NewSession);
                    }
                    let hist = ui.menu_button(RichText::new("🕘").size(13.0), |ui| {
                        ui.set_min_width(320.0);
                        let stale = chat.history_at.is_none_or(|t| t.elapsed().as_secs() > 5);
                        if stale {
                            chat.history = claude::list_sessions(&cwd);
                            chat.history_at = Some(std::time::Instant::now());
                        }
                        ui.label(RichText::new("Past conversations").small().strong().color(pal.text_weak));
                        if chat.history.is_empty() {
                            ui.label(RichText::new("No saved conversations for this folder").color(pal.text_weak));
                        }
                        let mut pick = None;
                        egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                            for s in &chat.history {
                                let current = chat.holds_session(&s.id);
                                let label = format!("{}\n{}", s.title, crate::util::fmt_ago(s.modified));
                                if ui.add(egui::Button::selectable(current, label).wrap()).clicked() {
                                    pick = Some(s.id.clone());
                                    ui.close();
                                }
                            }
                        });
                        ui.separator();
                        if ui
                            .add_enabled(last_session.is_some(), egui::Button::new("⟲  Resume the last conversation"))
                            .clicked()
                        {
                            pick = last_session.clone();
                            ui.close();
                        }
                        if let Some(id) = pick {
                            out.push(ChatAction::OpenSession(id));
                        }
                    });
                    hist.response.on_hover_text("Past conversations");
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.add(egui::Label::new(RichText::new(title).strong()).truncate());
                    });
                });
            });
            if !chat.todos.is_empty() && chat.todos.iter().any(|t| t.status != "completed") {
                ui.add_space(2.0);
                for t in &chat.todos {
                    let (icon, col) = match t.status.as_str() {
                        "completed" => ("☑", pal.success),
                        "in_progress" => ("◐", CLAUDE),
                        _ => ("☐", pal.text_weak),
                    };
                    ui.horizontal(|ui| {
                        ui.add_space(4.0);
                        ui.label(RichText::new(icon).color(col));
                        let mut rt = RichText::new(&t.content).small();
                        if t.status == "completed" {
                            rt = rt.strikethrough().color(pal.text_weak);
                        } else if t.status == "in_progress" {
                            rt = rt.color(pal.text);
                        }
                        ui.add(egui::Label::new(rt).wrap());
                    });
                }
            }
        });

    // ---------------- input box
    egui::Panel::bottom("claude_input")
        .frame(Frame::NONE.inner_margin(Margin { left: 8, right: 8, top: 4, bottom: 8 }))
        .show_separator_line(false)
        .show(ui, |ui| {
            let input_id = chat.input_id();
            if std::mem::take(&mut chat.focus_input) {
                ui.memory_mut(|m| m.request_focus(input_id));
            }
            let focused = ui.memory(|m| m.has_focus(input_id));
            // slash commands: the popup lists matches while the first word is typed
            let query = chat.input.strip_prefix('/').filter(|q| !q.contains(char::is_whitespace)).map(str::to_owned);
            let hidden = chat.cmd_hidden_for.as_deref() == Some(chat.input.as_str());
            let matches = match &query {
                Some(q) if focused && !hidden => command_matches(chat.commands(), q),
                _ => vec![],
            };
            let known: std::collections::HashSet<String> =
                if chat.input.starts_with('/') { chat.commands().iter().map(|c| c.name.clone()).collect() } else { Default::default() };
            let q_id = Id::new(("cmd_query", uid));
            if ui.data(|d| d.get_temp::<Option<String>>(q_id)) != Some(query.clone()) {
                ui.data_mut(|d| d.insert_temp(q_id, query.clone()));
                chat.cmd_sel = 0;
                // a popup's first frame only measures it: draw it for real right away
                ui.ctx().request_repaint();
            }
            chat.cmd_sel = chat.cmd_sel.min(matches.len().saturating_sub(1));
            // (name, then send) of the command to put into the input
            let mut complete: Option<(String, bool)> = None;
            let mut sel_moved = false;
            if !matches.is_empty() {
                ui.input_mut(|i| {
                    if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                        chat.cmd_sel = (chat.cmd_sel + 1) % matches.len();
                        sel_moved = true;
                    }
                    if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                        chat.cmd_sel = (chat.cmd_sel + matches.len() - 1) % matches.len();
                        sel_moved = true;
                    }
                    if i.consume_key(Modifiers::NONE, Key::Tab) {
                        complete = Some((matches[chat.cmd_sel].0.clone(), false));
                    } else if !i.modifiers.shift && i.consume_key(Modifiers::NONE, Key::Enter) {
                        complete = Some((matches[chat.cmd_sel].0.clone(), true));
                    }
                    if i.consume_key(Modifiers::NONE, Key::Escape) {
                        chat.cmd_hidden_for = Some(chat.input.clone());
                    }
                });
            }
            // Enter sends; Shift+Enter falls through to the text box as a new line
            let mut send = focused && !ui.input(|i| i.modifiers.shift) && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
            if focused && chat.busy && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
                chat.interrupt();
            }
            if focused {
                let pasted: Vec<std::sync::Arc<egui::ColorImage>> = ui.input(|i| {
                    i.events
                        .iter()
                        .filter_map(|e| match e {
                            egui::Event::PasteImage(img) => Some(img.clone()),
                            _ => None,
                        })
                        .collect()
                });
                for img in pasted {
                    let png = claude::color_image_to_png(&img);
                    match claude::prepare_image(ui.ctx(), "pasted.png", png) {
                        Ok(a) => chat.attachments.push(a),
                        Err(e) => chat.items.push(Item::Error(e)),
                    }
                }
            }
            if chat.busy {
                let secs = chat.busy_since.map(|t| t.elapsed().as_secs()).unwrap_or(0);
                ui.horizontal(|ui| {
                    ui.add_space(4.0);
                    ui.label(RichText::new(spark(now)).color(CLAUDE).size(14.0));
                    ui.label(RichText::new("Working…").color(CLAUDE));
                    ui.label(
                        RichText::new(format!("{}  ·  esc to interrupt", crate::util::fmt_duration(secs)))
                            .small()
                            .color(pal.text_weak),
                    );
                });
                ui.add_space(2.0);
            }
            let input_box = Frame::NONE
                .fill(pal.input_bg)
                .stroke(Stroke::new(1.0, if focused { CLAUDE.gamma_multiply(0.8) } else { pal.border }))
                .corner_radius(10)
                .inner_margin(Margin { left: 10, right: 8, top: 8, bottom: 6 })
                .show(ui, |ui| {
                    if !chat.attachments.is_empty() {
                        let mut remove = None;
                        ui.horizontal_wrapped(|ui| {
                            for (i, a) in chat.attachments.iter().enumerate() {
                                let r = match &a.thumb {
                                    Some(t) => ui.add(egui::Image::from_texture((t.id(), vec2(48.0, 48.0))).corner_radius(6).sense(Sense::click())),
                                    None => file_chip(ui, pal, &a.name, true),
                                };
                                if r.on_hover_text(format!("{}  ·  click to remove", a.name)).clicked() {
                                    remove = Some(i);
                                }
                            }
                        });
                        if let Some(i) = remove {
                            chat.attachments.remove(i);
                        }
                        ui.add_space(4.0);
                    }
                    let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, wrap: f32| {
                        let mut job = input_job(ui, pal, buf.as_str(), &known);
                        job.wrap.max_width = wrap;
                        ui.fonts_mut(|f| f.layout_job(job))
                    };
                    egui::ScrollArea::vertical().id_salt(("claude_input_scroll", uid)).max_height(220.0).show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut chat.input)
                                .id(input_id)
                                .frame(Frame::NONE)
                                .hint_text("Ask Claude to edit, explain or fix…  (/ for commands)")
                                .desired_rows(2)
                                .desired_width(f32::INFINITY)
                                // keeps Tab in the box while it completes a command
                                .lock_focus(!matches.is_empty())
                                .layouter(&mut layouter),
                        );
                    });
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        // permission mode
                        let r = chip(ui, pal, &mode_label(&chat.mode), "Permission mode");
                        egui::Popup::menu(&r).show(|ui| {
                            for (id, label, tip) in MODES {
                                let cur = chat.mode == *id;
                                if ui.add(egui::Button::selectable(cur, mode_label(id))).on_hover_text(format!("{label}: {tip}")).clicked() {
                                    chat.set_mode(id);
                                    out.push(ChatAction::SetMode(id.to_string()));
                                }
                            }
                        });
                        // model
                        let mname = MODELS.iter().find(|m| m.0 == chat.model).map(|m| m.1).unwrap_or(&chat.model).to_owned();
                        let r = chip(ui, pal, &format!("{mname} ▾"), "Model");
                        egui::Popup::menu(&r).show(|ui| {
                            for (id, label) in MODELS {
                                if ui.add(egui::Button::selectable(chat.model == *id, *label)).clicked() {
                                    chat.set_model(id);
                                    out.push(ChatAction::SetModel(id.to_string()));
                                }
                            }
                        });
                        if let Some(f) = &active_file {
                            let name = f.rsplit('/').next().unwrap_or(f);
                            if chip(ui, pal, &format!("@ {name}"), &format!("Mention {f} in the message")).clicked() {
                                if !chat.input.is_empty() && !chat.input.ends_with(' ') {
                                    chat.input.push(' ');
                                }
                                chat.input.push_str(&format!("@{f} "));
                                chat.focus_input = true;
                            }
                        }
                        if chip(ui, pal, "📎", "Attach images, PDF, Word, Excel or text files (or paste images with Ctrl+V)").clicked() {
                            crate::util::tall_file_dialog();
                            let all: Vec<&str> = crate::attach::IMAGE_EXTS.iter().chain(crate::attach::DOC_EXTS).copied().collect();
                            if let Some(files) = rfd::FileDialog::new()
                                .add_filter("Images and documents", &all)
                                .add_filter("Images", crate::attach::IMAGE_EXTS)
                                .add_filter("Documents", crate::attach::DOC_EXTS)
                                .add_filter("All files", &["*"])
                                .pick_files()
                            {
                                for f in files {
                                    match crate::attach::load(ui.ctx(), &f) {
                                        Ok(a) => chat.attachments.push(a),
                                        Err(e) => chat.items.push(Item::Error(e)),
                                    }
                                }
                            }
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            // round send / stop button
                            let (rect, resp) = ui.allocate_exact_size(vec2(26.0, 26.0), Sense::click());
                            let can = !chat.input.trim().is_empty() || !chat.attachments.is_empty();
                            let p = ui.painter();
                            if chat.busy && !can {
                                p.circle_filled(rect.center(), 13.0, if resp.hovered() { pal.error } else { pal.button_bg });
                                p.rect_filled(egui::Rect::from_center_size(rect.center(), vec2(9.0, 9.0)), 1.5, pal.text);
                                if resp.on_hover_text("Stop (Esc)").clicked() {
                                    chat.interrupt();
                                }
                            } else {
                                let fill = if can { if resp.hovered() { CLAUDE.gamma_multiply(1.15) } else { CLAUDE } } else { pal.button_bg };
                                p.circle_filled(rect.center(), 13.0, fill);
                                p.text(rect.center(), Align2::CENTER_CENTER, "↑", FontId::proportional(15.0), Color32::WHITE);
                                if can && resp.on_hover_text("Send (Enter) · Shift+Enter for a new line").clicked() {
                                    send = true;
                                }
                            }
                            if let Some(s) = &chat.stats {
                                ui.add_space(6.0);
                                ui.label(RichText::new(s).small().color(pal.text_weak));
                            }
                        });
                    });
                });
            if !matches.is_empty() {
                let r = input_box.response.rect;
                egui::Area::new(Id::new(("cmd_popup", uid)))
                    .order(egui::Order::Foreground)
                    .fixed_pos(r.left_top() - vec2(0.0, 4.0))
                    .pivot(Align2::LEFT_BOTTOM)
                    .show(ui.ctx(), |ui| {
                        Frame::popup(ui.style()).show(ui, |ui| {
                            ui.set_width(r.width() - 14.0);
                            ui.spacing_mut().item_spacing.y = 0.0;
                            let font = FontId::proportional(13.0);
                            // descriptions line up after the longest name (within reason)
                            let col = matches
                                .iter()
                                .map(|(n, _)| ui.painter().layout_no_wrap(format!("/{n}"), font.clone(), pal.text).size().x)
                                .fold(0.0, f32::max)
                                .min(r.width() * 0.45);
                            egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                                for (i, (name, desc)) in matches.iter().enumerate() {
                                    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::click());
                                    let p = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
                                    if i == chat.cmd_sel {
                                        p.rect_filled(rect, 4.0, pal.selection);
                                        if sel_moved {
                                            resp.scroll_to_me(None);
                                        }
                                    } else if resp.hovered() {
                                        p.rect_filled(rect, 4.0, pal.hover);
                                    }
                                    let y = rect.center().y;
                                    p.text(pos2(rect.min.x + 8.0, y), Align2::LEFT_CENTER, format!("/{name}"), font.clone(), CLAUDE);
                                    p.text(pos2(rect.min.x + 22.0 + col, y), Align2::LEFT_CENTER, desc, FontId::proportional(12.0), pal.text_weak);
                                    // on press: the text box gives up focus before a click would complete
                                    if resp.hovered() && ui.input(|i| i.pointer.primary_pressed()) {
                                        complete = Some((name.clone(), false));
                                    }
                                }
                            });
                        });
                    });
            }
            if let Some((name, then_send)) = complete {
                chat.input = format!("/{name} ");
                if then_send {
                    send = true;
                } else {
                    let mut st = egui::text_edit::TextEditState::load(ui.ctx(), input_id).unwrap_or_default();
                    let end = egui::text::CCursor::new(chat.input.chars().count());
                    st.cursor.set_char_range(Some(egui::text::CCursorRange::one(end)));
                    st.store(ui.ctx(), input_id);
                    chat.focus_input = true;
                }
            }
            if send {
                chat.send();
                ui.memory_mut(|m| m.request_focus(input_id));
            }
        });

    // ---------------- conversation
    egui::CentralPanel::default()
        .frame(Frame::NONE.inner_margin(Margin::symmetric(10, 6)))
        .show(ui, |ui| {
            let Chat { items, md_cache, live_text, busy, .. } = chat;
            let to_end_id = Id::new(("claude_to_end", uid));
            let mut area = egui::ScrollArea::vertical()
                .id_salt(("claude_msgs", uid))
                .stick_to_bottom(true)
                .auto_shrink([false, false]);
            // "↓" clicked last frame: jump to the end; reaching it re-enables stick_to_bottom
            if let Some(end) = ui.data_mut(|d| d.remove_temp::<f32>(to_end_id)) {
                area = area.vertical_scroll_offset(end);
            }
            let scroll = area.show(ui, |ui| {
                ui.set_width(ui.available_width());
                if items.is_empty() && !*busy {
                    ui.add_space(40.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new("✻").size(36.0).color(CLAUDE));
                        ui.add_space(4.0);
                        ui.label(RichText::new("What should we build?").size(16.0).strong());
                        ui.add_space(2.0);
                        ui.label(RichText::new(cwd.display().to_string()).small().color(pal.text_weak));
                        ui.add_space(10.0);
                        ui.label(
                            RichText::new(
                                "Each project has its own Claude agent running in parallel.\nYou get a notification when a task is done or needs you.\n🕘 opens past conversations.",
                            )
                            .small()
                            .color(pal.text_weak),
                        );
                    });
                }
                for (idx, item) in items.iter_mut().enumerate() {
                    match item {
                        Item::User { text, thumbs, files } => {
                            ui.add_space(8.0);
                            Frame::NONE
                                .fill(pal.user_bubble)
                                .stroke(Stroke::new(1.0, pal.border))
                                .corner_radius(8)
                                .inner_margin(Margin::symmetric(10, 8))
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    if !thumbs.is_empty() {
                                        ui.horizontal_wrapped(|ui| {
                                            for t in thumbs.iter() {
                                                ui.add(egui::Image::from_texture((t.id(), vec2(72.0, 72.0))).corner_radius(4));
                                            }
                                        });
                                    }
                                    if !files.is_empty() {
                                        ui.horizontal_wrapped(|ui| {
                                            for f in files.iter() {
                                                file_chip(ui, pal, f, false);
                                            }
                                        });
                                    }
                                    if !text.is_empty() {
                                        ui.add(egui::Label::new(RichText::new(text.as_str())).wrap().selectable(true));
                                    }
                                });
                            ui.add_space(6.0);
                        }
                        Item::Assistant(text) => {
                            ui.horizontal_top(|ui| {
                                ui.label(RichText::new("●").size(10.0).color(pal.text));
                                ui.vertical(|ui| {
                                    markdown(ui, md_cache, text);
                                });
                            });
                            ui.add_space(4.0);
                        }
                        Item::Thinking(text) => {
                            egui::CollapsingHeader::new(RichText::new("✻ Thinking").small().italics().color(pal.text_weak))
                                .id_salt(("think", idx))
                                .show(ui, |ui| {
                                    ui.add(egui::Label::new(RichText::new(text.as_str()).small().italics().color(pal.text_weak)).wrap());
                                });
                        }
                        Item::Tool { name, input, result, sub, .. } => {
                            let col = match result {
                                None => CLAUDE.gamma_multiply(0.55 + 0.45 * ((now * 4.0).sin() as f32 * 0.5 + 0.5)),
                                Some((_, true)) => pal.error,
                                Some((_, false)) => pal.success,
                            };
                            let summary = claude::tool_summary(name, input, &cwd);
                            let open_id = Id::new(("tool_open", uid, idx));
                            // edits start expanded (inline diff), like in VS Code
                            let default_open = matches!(name.as_str(), "Edit" | "MultiEdit" | "Write");
                            let mut open = ui.data(|d| d.get_temp::<bool>(open_id)).unwrap_or(default_open);
                            let indent = if *sub { 18.0 } else { 0.0 };
                            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::click());
                            let p = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
                            if resp.hovered() {
                                p.rect_filled(rect, 4.0, pal.hover);
                            }
                            let x = rect.min.x + 4.0 + indent;
                            p.circle_filled(pos2(x + 4.0, rect.center().y), 3.5, col);
                            // the name is centred; the command sits on its baseline (centring
                            // each font's own box would lift the monospace one)
                            let ppp = ui.ctx().pixels_per_point();
                            let snap = |v: f32| (v * ppp).round() / ppp;
                            let ng = p.layout_no_wrap(name.clone(), FontId::proportional(13.0), pal.text);
                            let sg = p.layout_no_wrap(summary.clone(), FontId::monospace(11.5), pal.text_weak);
                            let (nw, name_top) = (ng.size().x, snap(rect.center().y - ng.size().y / 2.0));
                            let base = name_top + first_baseline(&ng);
                            p.galley(pos2(x + 14.0, name_top), ng, pal.text);
                            p.galley(pos2(x + 22.0 + nw, snap(base - first_baseline(&sg))), sg, pal.text_weak);
                            let file = claude::tool_file_path(name, input);
                            let hover = if file.is_some() { "Click: details · double-click: open file" } else { "Click: details" };
                            let resp = resp.on_hover_text(hover);
                            if resp.clicked() {
                                open = !open;
                                ui.data_mut(|d| d.insert_temp(open_id, open));
                            }
                            if resp.double_clicked() {
                                if let Some(f) = file {
                                    acts.push(Act::Open(f));
                                }
                            }
                            if open {
                                ui.horizontal(|ui| {
                                    ui.add_space(14.0 + indent);
                                    ui.vertical(|ui| tool_body(ui, pal, name, input, result, &cwd));
                                });
                                ui.add_space(4.0);
                            }
                        }
                        Item::Permission { tool, input, state, .. } => {
                            let pending = *state == PermState::Pending;
                            ui.add_space(4.0);
                            Frame::NONE
                                .fill(pal.card)
                                .stroke(Stroke::new(1.0, if pending { CLAUDE } else { pal.border }))
                                .corner_radius(8)
                                .inner_margin(Margin::same(10))
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    let plan = tool == "ExitPlanMode";
                                    let title = if plan {
                                        "Here is Claude's plan. Ready to code?".to_owned()
                                    } else {
                                        match tool.as_str() {
                                            "Edit" | "MultiEdit" | "Write" => format!("Make this edit to {}?", claude::tool_summary(tool, input, &cwd)),
                                            "Bash" | "PowerShell" => "Run this command?".to_owned(),
                                            _ => format!("Allow {tool}?"),
                                        }
                                    };
                                    ui.label(RichText::new(title).strong());
                                    ui.add_space(4.0);
                                    if plan {
                                        markdown(ui, md_cache, input["plan"].as_str().unwrap_or(""));
                                    } else {
                                        tool_body(ui, pal, tool, input, &None, &cwd);
                                    }
                                    ui.add_space(4.0);
                                    match state {
                                        PermState::Pending => {
                                            let opts: Vec<(&str, Act)> = if plan {
                                                vec![
                                                    ("Yes, and auto-accept edits", Act::Allow(idx, false, Some("acceptEdits"))),
                                                    ("Yes, and manually approve edits", Act::Allow(idx, false, Some("manual"))),
                                                    ("No, keep planning", Act::Deny(idx)),
                                                ]
                                            } else {
                                                vec![
                                                    ("Yes", Act::Allow(idx, false, None)),
                                                    ("Yes, and don't ask again this session", Act::Allow(idx, true, None)),
                                                    ("No, tell Claude what to do differently", Act::Deny(idx)),
                                                ]
                                            };
                                            for (n, (label, act)) in opts.into_iter().enumerate() {
                                                let txt = RichText::new(format!("{}  {label}", n + 1));
                                                let b = egui::Button::new(if n == 0 { txt.color(Color32::WHITE) } else { txt })
                                                    .fill(if n == 0 { CLAUDE } else { pal.button_bg })
                                                    .min_size(vec2(ui.available_width(), 26.0))
                                                    .corner_radius(6);
                                                if ui.add(b).clicked() {
                                                    acts.push(act);
                                                }
                                            }
                                        }
                                        PermState::Allowed => {
                                            ui.label(RichText::new("✔ Allowed").small().color(pal.success));
                                        }
                                        PermState::Denied => {
                                            ui.label(RichText::new("✖ Denied").small().color(pal.error));
                                        }
                                    }
                                });
                            ui.add_space(4.0);
                        }
                        Item::Question { input, answers, state, .. } => {
                            let pending = *state == PermState::Pending;
                            Frame::NONE
                                .fill(pal.card)
                                .stroke(Stroke::new(1.0, if pending { CLAUDE } else { pal.border }))
                                .corner_radius(8)
                                .inner_margin(Margin::same(10))
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    if let Some(qs) = input["questions"].as_array() {
                                        for q in qs {
                                            let qt = q["question"].as_str().unwrap_or("").to_owned();
                                            let multi = q["multiSelect"].as_bool().unwrap_or(false);
                                            ui.add(egui::Label::new(RichText::new(&qt).strong()).wrap());
                                            if let Some(opts) = q["options"].as_array() {
                                                for o in opts {
                                                    let label = o["label"].as_str().unwrap_or("").to_owned();
                                                    let chosen = answers.get(&qt).map(|a| a.split(", ").any(|x| x == label)).unwrap_or(false);
                                                    let desc = o["description"].as_str().unwrap_or("");
                                                    let r = ui.add_enabled(
                                                        pending,
                                                        egui::Button::selectable(chosen, format!("{}  {label}", if chosen { "●" } else { "○" }))
                                                            .min_size(vec2(ui.available_width(), 24.0)),
                                                    );
                                                    if r.on_hover_text(desc).clicked() {
                                                        acts.push(Act::Pick(idx, qt.clone(), label.clone(), multi));
                                                    }
                                                }
                                            }
                                            ui.add_space(4.0);
                                        }
                                    }
                                    if pending {
                                        ui.horizontal(|ui| {
                                            if ui.add(egui::Button::new(RichText::new("Submit").color(Color32::WHITE)).fill(CLAUDE)).clicked() {
                                                acts.push(Act::Submit(idx, true));
                                            }
                                            if ui.button("Skip").clicked() {
                                                acts.push(Act::Submit(idx, false));
                                            }
                                        });
                                    }
                                });
                        }
                        Item::Result { text, is_error } => {
                            if *is_error {
                                ui.add(egui::Label::new(RichText::new(format!("✖ {text}")).small().color(pal.error)).wrap());
                            } else {
                                ui.label(RichText::new(format!("✔ {text}")).small().color(pal.text_weak));
                            }
                            ui.add_space(6.0);
                        }
                        Item::System(t) => {
                            ui.label(RichText::new(t.as_str()).small().italics().color(pal.text_weak));
                        }
                        Item::Error(t) => {
                            ui.add(egui::Label::new(RichText::new(t.as_str()).small().color(pal.error)).wrap());
                        }
                    }
                }
                if *busy && !live_text.is_empty() {
                    ui.horizontal_top(|ui| {
                        ui.label(RichText::new("●").size(10.0).color(pal.text));
                        ui.vertical(|ui| {
                            markdown(ui, md_cache, live_text);
                        });
                    });
                }
                ui.add_space(8.0);
            });

            // scrolled up → round "↓" button bottom-right; a click jumps to the end and the
            // view sticks to the bottom again
            let max = scroll.content_size.y - scroll.inner_rect.height();
            if max - scroll.state.offset().y > 120.0 {
                let r = egui::Rect::from_center_size(scroll.inner_rect.right_bottom() - vec2(34.0, 22.0), vec2(30.0, 30.0));
                let resp = ui.interact(r, to_end_id, Sense::click()).on_hover_text("Scroll to the end");
                let p = ui.painter();
                p.circle(r.center(), 15.0, if resp.hovered() { pal.hover } else { pal.card }, Stroke::new(1.0, pal.border));
                crate::theme::paint_glyph(p, r.center(), "↓", egui::FontId::proportional(15.0), pal.text);
                if resp.clicked() {
                    ui.data_mut(|d| d.insert_temp(to_end_id, max));
                    ui.ctx().request_repaint();
                }
            }
        });

    for a in acts {
        match a {
            Act::Allow(i, always, mode) => chat.respond_permission(i, true, always, mode),
            Act::Deny(i) => {
                chat.respond_permission(i, false, false, None);
                chat.focus_input = true;
            }
            Act::Pick(i, q, label, multi) => {
                if let Some(ans) = chat.question_answers_mut(i) {
                    if multi {
                        let mut cur: Vec<String> = ans
                            .get(&q)
                            .map(|s| s.split(", ").filter(|x| !x.is_empty()).map(|x| x.to_owned()).collect())
                            .unwrap_or_default();
                        if let Some(p) = cur.iter().position(|x| *x == label) {
                            cur.remove(p);
                        } else {
                            cur.push(label);
                        }
                        ans.insert(q, cur.join(", "));
                    } else {
                        ans.insert(q, label);
                    }
                }
            }
            Act::Submit(i, ok) => chat.respond_question(i, ok),
            Act::Open(p) => out.push(ChatAction::OpenFile(p)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A known command at the start and `@` mentions get colour; the rest stays plain.
    #[test]
    fn input_highlighting() {
        let ctx = egui::Context::default();
        let pal = crate::theme::palette(true);
        let known: std::collections::HashSet<String> = ["compact".to_owned()].into();
        let mut runs = vec![];
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            for text in ["/compact  keep @src/a.rs\nnow", "/compac x", "say /compact"] {
                let job = input_job(ui, &pal, text, &known);
                assert_eq!(job.text, text);
                runs.push(job.sections.iter().map(|s| (&text[s.byte_range.start.0..s.byte_range.end.0], s.format.color)).collect::<Vec<_>>());
            }
        });
        let coloured = |r: &Vec<(&str, Color32)>, c: Color32| r.iter().filter(|s| s.1 == c).map(|s| s.0.to_owned()).collect::<Vec<_>>();
        assert_eq!(coloured(&runs[0], CLAUDE), ["/compact"]);
        assert_eq!(coloured(&runs[0], pal.link), ["@src/a.rs"]);
        assert!(coloured(&runs[1], CLAUDE).is_empty() && coloured(&runs[2], CLAUDE).is_empty());

        let cmds: Vec<claude::SlashCommand> = ["add-dir", "compact", "security-review", "plug:add"]
            .iter()
            .map(|n| claude::SlashCommand { name: n.to_string(), desc: String::new() })
            .collect();
        let names = |q| command_matches(&cmds, q).into_iter().map(|m| m.0).collect::<Vec<_>>();
        assert_eq!(names("add"), ["add-dir", "plug:add"]);
        assert_eq!(names("REV"), ["security-review"]);
        assert_eq!(names("").len(), 4);
    }

    /// Scrolled up in a long conversation, the "↓" button appears; a click brings back the end.
    #[test]
    fn scroll_to_end_button() {
        let ctx = egui::Context::default();
        crate::app::setup_fonts(&ctx);
        crate::theme::apply(&ctx, true, 14.0);
        let pal = crate::theme::palette(true);
        let mut chat = Chat::new(std::env::temp_dir(), ctx.clone(), "default".into(), "manual".into(), None);
        chat.items = (0..80).map(|n| Item::Assistant(format!("reply {n}"))).collect();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(600.0, 700.0));
        let mut input = egui::RawInput::default();
        input.screen_rect = Some(screen);
        let mut time = 0.0;
        // (text, centre) of every text shape on screen
        let mut frame = |chat: &mut Chat, events: Vec<egui::Event>| {
            let mut i = input.clone();
            i.events = events;
            time += 0.5;
            i.time = Some(time);
            let out = ctx.run_ui(i, |ui| {
                show(ui, chat, &pal, None, None);
            });
            out.shapes
                .iter()
                .filter_map(|cs| match &cs.shape {
                    egui::Shape::Text(t) if cs.clip_rect.intersects(t.visual_bounding_rect()) && screen.intersects(t.visual_bounding_rect()) => {
                        Some((t.galley.text().to_owned(), t.visual_bounding_rect().center()))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let find = |texts: &[(String, egui::Pos2)], s: &str| texts.iter().find(|t| t.0 == s).map(|t| t.1);
        for _ in 0..3 {
            frame(&mut chat, vec![]);
        }
        let f = frame(&mut chat, vec![]);
        assert!(find(&f, "reply 79").is_some() && find(&f, "↓").is_none(), "starts at the end");

        let wheel = egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: vec2(0.0, 5000.0),
            phase: egui::TouchPhase::Move,
            source: egui::MouseWheelSource::Unknown,
            modifiers: Default::default(),
        };
        frame(&mut chat, vec![egui::Event::PointerMoved(pos2(300.0, 200.0)), wheel]);
        for _ in 0..6 {
            frame(&mut chat, vec![]);
        }
        let f = frame(&mut chat, vec![]);
        assert!(find(&f, "reply 79").is_none(), "scrolled up");
        let btn = find(&f, "↓").expect("button shown when scrolled up");

        let click = |pressed| egui::Event::PointerButton { pos: btn, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
        frame(&mut chat, vec![egui::Event::PointerMoved(btn), click(true)]);
        frame(&mut chat, vec![click(false)]);
        for _ in 0..3 {
            frame(&mut chat, vec![]);
        }
        // moved away so the hover tooltip doesn't count
        let f = frame(&mut chat, vec![egui::Event::PointerMoved(pos2(300.0, 200.0))]);
        assert!(find(&f, "reply 79").is_some() && find(&f, "↓").is_none(), "back at the end: {:?}", f.iter().map(|t| &t.0).collect::<Vec<_>>());

        // still stuck to the bottom: a new reply is shown without scrolling
        chat.items.push(Item::Assistant("reply 80".into()));
        frame(&mut chat, vec![]);
        let f = frame(&mut chat, vec![]);
        assert!(find(&f, "reply 80").is_some());
    }

    /// Typing `/` opens the command list at once; each further letter narrows it.
    #[test]
    fn slash_opens_command_popup() {
        let ctx = egui::Context::default();
        crate::app::setup_fonts(&ctx);
        crate::theme::apply(&ctx, true, 14.0);
        let pal = crate::theme::palette(true);
        let mut chat = Chat::new(std::env::temp_dir(), ctx.clone(), "default".into(), "manual".into(), None);
        chat.focus_input = true;
        let mut input = egui::RawInput::default();
        input.screen_rect = Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(600.0, 700.0)));
        let mut time = 0.0;
        let mut frame = |events: Vec<egui::Event>| {
            let mut i = input.clone();
            i.events = events;
            // popups fade in: let the clock run
            time += 0.5;
            i.time = Some(time);
            let out = ctx.run_ui(i, |ui| {
                show(ui, &mut chat, &pal, None, None);
            });
            let mut texts = vec![];
            for cs in &out.shapes {
                if let egui::Shape::Text(t) = &cs.shape {
                    texts.push(t.galley.text().to_owned());
                }
            }
            texts
        };
        frame(vec![]);
        frame(vec![]);
        let shown = |texts: &[String]| texts.iter().filter(|t| t.starts_with('/') && t.len() > 1).cloned().collect::<Vec<_>>();
        frame(vec![egui::Event::Text("/".into())]);
        frame(vec![]); // the popup's first frame is an invisible sizing pass
        let f = frame(vec![]);
        let all = shown(&f);
        assert!(all.contains(&"/compact".to_owned()) && all.contains(&"/init".to_owned()), "after '/': {all:?}");
        frame(vec![egui::Event::Text("co".into())]);
        frame(vec![]);
        let some = shown(&frame(vec![]));
        assert!(some.contains(&"/compact".to_owned()) && !some.contains(&"/init".to_owned()), "after '/co': {some:?}");
        assert!(some.len() < all.len());
    }

    /// List numbers and bullets sit on their item's first text line.
    #[test]
    fn list_markers_on_text_baseline() {
        for ppp in [1.0f32, 1.25, 1.5, 2.0] {
            let ctx = egui::Context::default();
            crate::app::setup_fonts(&ctx);
            crate::theme::apply(&ctx, true, 14.0);
            let mut cache = egui_commonmark::CommonMarkCache::default();
            let mut input = egui::RawInput::default();
            input.screen_rect = Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(420.0, 400.0)));
            input.viewports.entry(egui::ViewportId::ROOT).or_default().native_pixels_per_point = Some(ppp);
            let md = "To fix it:\n\n1. In Chrome, check the extension.\n2. In Claude Code, see `/mcp` or `/chrome` here.\n\n- Bullet one\n- Bullet `two`\n\nVerified:\n\n- Wrapped item that is long enough to run onto a second and maybe a third line of text here.\n- Another wrapped item with `code` in it, also long enough to wrap onto the next line.\n";
            let mut out = None;
            for _ in 0..3 {
                let mut o = ctx.run_ui(input.clone(), |ui| markdown(ui, &mut cache, md));
                o.textures_delta.clear();
                out = Some(o);
            }
            // (first word, x, baseline) of every text run
            let mut runs = vec![];
            for cs in &out.unwrap().shapes {
                if let egui::Shape::Text(t) = &cs.shape {
                    let r = &t.galley.rows[0];
                    let s: String = r.row.glyphs.iter().map(|g| g.chr).collect();
                    if let Some(g) = r.row.glyphs.iter().find(|g| !g.chr.is_whitespace()) {
                        runs.push((s.trim().split(' ').next().unwrap_or("").to_owned(), t.pos.x, t.pos.y + r.pos.y + g.pos.y));
                    }
                }
            }
            for m in ["1.", "2.", "•"] {
                let marks: Vec<_> = runs.iter().filter(|r| r.0 == m).collect();
                assert!(!marks.is_empty(), "ppp {ppp}: no {m} in {runs:?}");
                if m == "•" {
                    assert_eq!(marks.len(), 4, "ppp {ppp}: {runs:?}");
                }
                for (_, x, b) in marks {
                    // the item's text: the next run on the right, on the same baseline
                    assert!(
                        runs.iter().any(|r| r.1 > *x && !["1.", "2.", "•"].contains(&r.0.as_str()) && (r.2 - b).abs() < 0.01),
                        "ppp {ppp}: {m} at baseline {b} has no text beside it: {runs:?}"
                    );
                }
            }
        }
    }

    /// Inline `code` must sit exactly on the body text's baseline, its chip around it.
    #[test]
    fn inline_code_on_text_baseline() {
        for ppp in [1.0f32, 1.25, 1.5, 2.0] {
            let ctx = egui::Context::default();
            crate::app::setup_fonts(&ctx);
            crate::theme::apply(&ctx, true, 14.0);
            let mut cache = egui_commonmark::CommonMarkCache::default();
            let mut input = egui::RawInput::default();
            input.screen_rect = Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(420.0, 400.0)));
            input.viewports.entry(egui::ViewportId::ROOT).or_default().native_pixels_per_point = Some(ppp);
            let md = "Код в `tall_file_dialog()` в `src/util.rs`, для этого включил в `Cargo.toml` две фичи windows-sys, и ещё немного текста.";
            let mut out = None;
            for _ in 0..3 {
                let mut o = ctx.run_ui(input.clone(), |ui| markdown(ui, &mut cache, md));
                o.textures_delta.clear();
                out = Some(o);
            }
            // (is code, baseline) of every glyph; chips by their corner radius
            let (mut glyphs, mut chips) = (vec![], vec![]);
            for cs in &out.unwrap().shapes {
                match &cs.shape {
                    egui::Shape::Text(t) => {
                        for r in &t.galley.rows {
                            for g in r.row.glyphs.iter().filter(|g| !g.chr.is_whitespace()) {
                                // at 14 pt Segoe UI's ascent is 15.1, Consolas' 10.4
                                let code = g.font_ascent < 12.5;
                                glyphs.push((code, t.pos.y + r.pos.y + g.pos.y));
                            }
                        }
                    }
                    egui::Shape::Rect(r) if r.corner_radius.nw == 4 => chips.push(r.rect),
                    _ => {}
                }
            }
            let text: Vec<f32> = glyphs.iter().filter(|g| !g.0).map(|g| g.1).collect();
            let code: Vec<f32> = glyphs.iter().filter(|g| g.0).map(|g| g.1).collect();
            assert!(!code.is_empty() && chips.len() == 3, "ppp {ppp}: {} code glyphs, {} chips", code.len(), chips.len());
            for b in &code {
                assert!(text.iter().any(|t| (t - b).abs() < 0.01), "ppp {ppp}: code baseline {b} not in text baselines {text:?}");
                let chip = chips.iter().find(|c| c.y_range().contains(*b)).expect("code glyph without chip");
                let (above, below) = (b - chip.min.y, chip.max.y - b);
                assert!(above > below && below > 3.0, "ppp {ppp}: chip {above} above / {below} below the baseline");
            }
        }
    }

    /// Table cells wrap inside their column instead of running into the next one, a table
    /// that fits keeps its cells on one line, and the layout is the same every frame.
    #[test]
    fn table_cells_wrap_in_their_columns() {
        let short = "Limits:

| Сервис | Лимит |
|---|---|
| MySQL (буфер 512 МБ) | 1 ГБ |
| Сайт (FrankenPHP) | 768 МБ |
| Очередь (ИИ-задачи) | 384 МБ |
| Итого максимум | ~2,9 ГБ |
";
        let wide = "| Тип | Примеры | Вывод |
|---|---|---|
| Общий поиск | tender search 500 / $10, tender alerts 50 / $9, tender database 50 / $37, uk tenders 500 / $8 | Мало: такие фразы почти никто не ищет, это видно по частотности |
| Софт | procurement software 5k / $63 | Высокие ставки, но это не ваша аудитория |
";
        for (md, must_fit) in [(short, true), (wide, false)] {
            let ctx = egui::Context::default();
            crate::app::setup_fonts(&ctx);
            crate::theme::apply(&ctx, true, 14.0);
            let mut cache = egui_commonmark::CommonMarkCache::default();
            let mut input = egui::RawInput::default();
            let width = 420.0;
            input.screen_rect = Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(width, 600.0)));
            let mut frames = vec![];
            for _ in 0..4 {
                let mut o = ctx.run_ui(input.clone(), |ui| markdown(ui, &mut cache, md));
                o.textures_delta.clear();
                // (text, rect, rows) of every non-blank text run; the rect leaves out the
                // leading space a label continuing a wrapped line starts with
                let runs: Vec<(String, egui::Rect, usize)> = o
                    .shapes
                    .iter()
                    .filter_map(|cs| match &cs.shape {
                        egui::Shape::Text(t) if !t.galley.text().trim().is_empty() => {
                            let rect = t
                                .galley
                                .rows
                                .iter()
                                .fold(egui::Rect::NOTHING, |r, row| r.union(row.rect_without_leading_space()));
                            Some((t.galley.text().to_owned(), rect.translate(t.pos.to_vec2()), t.galley.rows.len()))
                        }
                        _ => None,
                    })
                    .collect();
                frames.push(runs);
            }
            let runs = frames.last().unwrap();
            assert_eq!(&frames[2], runs, "layout changes between frames");
            for word in ["Сервис", "Лимит", "MySQL", "768 МБ", "2,9 ГБ", "Тип", "Вывод"] {
                if md.contains(word) {
                    assert!(runs.iter().any(|r| r.0.contains(word)), "{word:?} not drawn: {runs:#?}");
                }
            }
            for (i, a) in runs.iter().enumerate() {
                assert!(a.1.max.x <= width + 0.5, "{:?} runs past the right edge: {:?}", a.0, a.1);
                if must_fit && md.contains(a.0.trim()) && a.0.contains(' ') {
                    assert_eq!(a.2, 1, "{:?} wrapped although the table fits", a.0);
                }
                for b in &runs[i + 1..] {
                    assert!(!a.1.shrink(0.5).intersects(b.1.shrink(0.5)), "{:?} {:?} overlaps {:?} {:?}", a.0, a.1, b.0, b.1);
                }
            }
            if !must_fit {
                assert!(runs.iter().any(|r| r.0.starts_with("tender search") && r.2 > 1), "long cell didn't wrap: {runs:#?}");
            }
        }
    }
}
