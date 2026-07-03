//! Saved views (yggdrasil-131). The user composes a pane + filters
//! they like, presses `:save <name>`, and recalls it later by `:load
//! <name>`. Persisted to `~/.config/ygg/views.toml` so views survive
//! TUI restarts.
//!
//! For the MVP a saved view captures the active pane and the global
//! scope (yggdrasil-134). Per-pane filter state will compose in once
//! `/`-filter (yggdrasil-128) lands; the file format already carries
//! a `filter` map so old views read forward without migration.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// One persisted view. `pane` is the `ActiveView`'s stable label
/// (matching `App::active_view_label`); `scope` is "repo" or "all".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SavedView {
    pub name: String,
    pub pane: String,
    pub scope: String,
    /// Per-pane filter substring keyed by pane label. Forward-compatible
    /// with yggdrasil-128 — empty for views saved before that feature.
    #[serde(default)]
    pub filter: BTreeMap<String, String>,
}

/// On-disk file shape. Keeps a top-level wrapper so we can grow the
/// format (per-user prefs, default view to load on start) without
/// breaking older files.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SavedViewsFile {
    #[serde(default)]
    pub views: Vec<SavedView>,
}

/// Default config path under the user's home directory. Falls back to
/// the cwd if `$HOME` isn't set so a CI environment can still exercise
/// the loader without picking up someone else's views.
pub fn default_path() -> PathBuf {
    if let Ok(p) = std::env::var("YGG_VIEWS_PATH") {
        return PathBuf::from(p);
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".config/ygg/views.toml");
    }
    PathBuf::from("ygg-views.toml")
}

/// Load views from the supplied path. Missing file → empty result;
/// only invalid TOML produces an error so the TUI can boot from a
/// fresh install without explicit setup.
pub fn load_from(path: &std::path::Path) -> Result<SavedViewsFile, String> {
    if !path.exists() {
        return Ok(SavedViewsFile::default());
    }
    let body = std::fs::read_to_string(path).map_err(|e| format!("read {path:?}: {e}"))?;
    toml::from_str(&body).map_err(|e| format!("parse {path:?}: {e}"))
}

/// Save (overwrites) the views file at `path`. Creates parent dirs as
/// needed; emits TOML with a stable, alphabetised key order.
pub fn save_to(path: &std::path::Path, file: &SavedViewsFile) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {parent:?}: {e}"))?;
    }
    let body = toml::to_string_pretty(file).map_err(|e| format!("serialize: {e}"))?;
    std::fs::write(path, body).map_err(|e| format!("write {path:?}: {e}"))
}

impl SavedViewsFile {
    /// Find a view by name (case-sensitive). Returns the first match;
    /// duplicates are tolerated on disk but `upsert` keeps them unique.
    pub fn get(&self, name: &str) -> Option<&SavedView> {
        self.views.iter().find(|v| v.name == name)
    }

    /// Insert-or-replace by name. Keeps the views vector stable in
    /// insertion order so users see saves in the order they made them.
    pub fn upsert(&mut self, view: SavedView) {
        if let Some(slot) = self.views.iter_mut().find(|v| v.name == view.name) {
            *slot = view;
        } else {
            self.views.push(view);
        }
    }

    /// Remove a view by name. No-op when missing.
    pub fn remove(&mut self, name: &str) {
        self.views.retain(|v| v.name != name);
    }
}
