//! Persistent settings in %APPDATA%\VibeZen\settings.json

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct ProjectMemo {
    pub open_files: Vec<String>,
    /// `None` = a Claude Code tab was in front
    pub active_file: Option<String>,
    /// session ids of the open conversation tabs, in tab order
    pub chats: Vec<String>,
    pub active_chat: Option<String>,
    pub last_session: Option<String>,
    /// unix seconds of the project's last action (agent task, save, commit…); orders RECENT
    pub last_active: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Settings {
    pub dark: bool,
    pub editor_font_size: f32,
    pub ui_font_size: f32,
    pub open_projects: Vec<String>,
    pub active_project: usize,
    pub recent: Vec<String>,
    pub claude_model: String,
    pub claude_mode: String,
    /// save edited files shortly after typing stops, on focus loss and on leaving a project
    pub auto_save: bool,
    pub notify: bool,
    pub notify_only_unfocused: bool,
    pub notify_sound: bool,
    pub sidebar_collapsed: bool,
    pub memo: HashMap<String, ProjectMemo>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            dark: true,
            editor_font_size: 14.0,
            ui_font_size: 13.5,
            open_projects: vec![],
            active_project: 0,
            recent: vec![],
            claude_model: "default".into(),
            claude_mode: "acceptEdits".into(),
            auto_save: true,
            notify: true,
            notify_only_unfocused: false,
            notify_sound: true,
            sidebar_collapsed: false,
            memo: HashMap::new(),
        }
    }
}

impl Settings {
    fn path() -> PathBuf {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        base.join("VibeZen").join("settings.json")
    }

    pub fn load() -> Self {
        let p = Self::path();
        // first start after the rename: pick up the settings of VibeStorm (saved to the new place)
        let legacy = p.parent().and_then(|d| d.parent()).map(|b| b.join("VibeStorm").join("settings.json"));
        std::fs::read_to_string(&p)
            .or_else(|e| legacy.map_or(Err(e), std::fs::read_to_string))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let p = Self::path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let tmp = p.with_extension("json.tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, &p);
            }
        }
    }

    pub fn push_recent(&mut self, path: &str) {
        self.recent.retain(|p| !p.eq_ignore_ascii_case(path));
        self.recent.insert(0, path.to_owned());
        self.recent.truncate(25);
    }
}
