//! Per-pane filter state (yggdrasil-128). Each pane carries a
//! `PaneFilter` that owns a `tui-input::Input` for the pattern + a
//! mode flag; pressing `/` enters edit mode, typing builds the
//! pattern, Enter commits, Esc clears.
//!
//! The pane-level integration (handing keys to `Input` while in edit
//! mode, applying `matches` to its rows) is per-pane work that lands
//! incrementally. This module ships:
//!
//! - the state machine (`enter`, `cancel`, `commit`, `clear`, `mode`),
//! - the `matches(haystack)` predicate panes call per row,
//! - the `compose` helper that joins the pattern with optional pane
//!   scope tags so future composability ("status:open priority:0") can
//!   layer in.

use tui_input::Input;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FilterMode {
    /// Not filtering — every row passes.
    #[default]
    Off,
    /// User pressed `/` and is typing; rows already filter live so
    /// they can preview what survives.
    Editing,
    /// User pressed Enter — pattern is committed; further keys flow
    /// to the pane until `/` re-enters edit mode.
    Active,
}

#[derive(Debug, Default, Clone)]
pub struct PaneFilter {
    pub mode: FilterMode,
    pub input: Input,
}

impl PaneFilter {
    /// Begin a new filter. Pre-fills the Input with the prior committed
    /// pattern so re-pressing `/` lets the user edit, not retype.
    pub fn enter(&mut self) {
        self.mode = FilterMode::Editing;
    }

    /// Esc while editing → drop pattern entirely. Active filters get
    /// cleared the same way; the second `Esc` after a committed
    /// filter returns the pane to no-filter.
    pub fn cancel(&mut self) {
        self.input = Input::default();
        self.mode = FilterMode::Off;
    }

    /// Commit the current pattern. Empty patterns flip back to Off so
    /// pressing Enter on an empty input doesn't pin a pane to "show
    /// nothing matches the empty string."
    pub fn commit(&mut self) {
        if self.input.value().is_empty() {
            self.mode = FilterMode::Off;
        } else {
            self.mode = FilterMode::Active;
        }
    }

    /// Predicate the pane evaluates per row. While editing, the pattern
    /// is live; while Active, the committed pattern stays in force.
    /// Off → everything passes.
    pub fn matches(&self, haystack: &str) -> bool {
        match self.mode {
            FilterMode::Off => true,
            FilterMode::Editing | FilterMode::Active => {
                let needle = self.input.value();
                if needle.is_empty() {
                    true
                } else {
                    haystack.to_lowercase().contains(&needle.to_lowercase())
                }
            }
        }
    }

    /// Stable label panes can show in the title bar / help row when a
    /// filter is active. Returns None when Off.
    pub fn label(&self) -> Option<String> {
        match self.mode {
            FilterMode::Off => None,
            FilterMode::Editing => Some(format!("/{}", self.input.value())),
            FilterMode::Active => Some(format!("filter: {}", self.input.value())),
        }
    }

    pub fn is_editing(&self) -> bool {
        matches!(self.mode, FilterMode::Editing)
    }
}
