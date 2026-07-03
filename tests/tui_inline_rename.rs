//! Regression for the inline-rename buffer state machine (yggdrasil-155).

use ygg::tui::tasks_view::TasksView;

#[test]
fn fresh_view_is_not_in_rename_mode() {
    let v = TasksView::new();
    assert!(!v.rename_mode());
    assert!(v.rename_buffer().is_none());
}

#[test]
fn rename_begin_with_no_selection_is_noop() {
    let mut v = TasksView::new();
    // No rows loaded → selection has nothing to anchor on.
    v.rows.clear();
    v.rename_begin();
    assert!(!v.rename_mode());
}

#[test]
fn cancel_clears_buffer() {
    let mut v = TasksView::new();
    v.rename = Some((uuid::Uuid::nil().to_string(), "edit".into()));
    v.rename_cancel();
    assert!(!v.rename_mode());
}

#[test]
fn push_pop_round_trip() {
    let mut v = TasksView::new();
    v.rename = Some((uuid::Uuid::nil().to_string(), "ab".into()));
    v.rename_push('c');
    assert_eq!(v.rename_buffer(), Some("abc"));
    v.rename_pop();
    assert_eq!(v.rename_buffer(), Some("ab"));
}

#[test]
fn push_when_not_in_rename_mode_is_noop() {
    let mut v = TasksView::new();
    v.rename_push('x');
    v.rename_pop();
    assert!(!v.rename_mode());
}
