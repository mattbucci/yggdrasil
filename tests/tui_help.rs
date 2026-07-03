//! Regression for the help overlay state machine + keymap (yggdrasil-132).

use ygg::tui::help::{GLOBAL_KEYS, HelpOverlay, pane_keys};

#[test]
fn help_starts_closed() {
    let h = HelpOverlay::default();
    assert!(!h.open);
}

#[test]
fn toggle_flips_open_state() {
    let mut h = HelpOverlay::default();
    h.toggle();
    assert!(h.open);
    h.toggle();
    assert!(!h.open);
}

#[test]
fn close_is_idempotent() {
    let mut h = HelpOverlay { open: true };
    h.close();
    h.close();
    assert!(!h.open);
}

#[test]
fn global_keymap_lists_quit_and_navigation() {
    let keys: Vec<&str> = GLOBAL_KEYS.iter().map(|k| k.keys).collect();
    assert!(keys.iter().any(|k| k.contains("q")), "missing quit");
    assert!(keys.iter().any(|k| k.contains("Tab")), "missing nav");
    assert!(keys.iter().any(|k| k.contains("?")), "missing self-toggle");
    assert!(
        keys.contains(&"1,2,3,5,9,0,G,N,C"),
        "missing exact switch-view key legend"
    );
}

#[test]
fn each_active_pane_has_a_keymap_section() {
    for v in [
        "Dashboard",
        "Dag",
        "Tasks",
        "Logs",
        "Locks",
        "Runs",
        "RunGrid",
        "Nerdy",
        "Chat",
    ] {
        let keys = pane_keys(v);
        // RunGrid has its own narrow "rows = tasks" pseudo-bindings; every
        // other pane carries at least one real binding.
        assert!(
            !keys.is_empty(),
            "{v} has no pane-specific keys; consider adding at least ↑↓"
        );
    }
}

#[test]
fn unknown_pane_returns_empty_section() {
    assert!(pane_keys("ThisPaneDoesNotExist").is_empty());
}
