//! Regression for the narrow-terminal collapse (yggdrasil-156).
//! Verifies the threshold and that the collapsed chrome actually fits in
//! an 80-column terminal — the worst realistic narrow case (notebooks,
//! sidebar panes, half-screen tmux splits).

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ygg::tui::app::{App, NARROW_TERMINAL_THRESHOLD};

#[test]
fn narrow_threshold_is_100_columns() {
    // Documenting the magic number so changes here force a test failure
    // and a reviewer eyeballs the implications.
    assert_eq!(NARROW_TERMINAL_THRESHOLD, 100);
}

fn render_at(width: u16) -> Buffer {
    let mut term = Terminal::new(TestBackend::new(width, 30)).unwrap();
    let mut app = App::new("test-agent".to_string());
    term.draw(|f| app.draw_for_test(f)).unwrap();
    term.backend().buffer().clone()
}

fn first_row(buf: &Buffer) -> String {
    let width = buf.area().width;
    (0..width)
        .map(|x| buf[(x, 0)].symbol().chars().next().unwrap_or(' '))
        .collect()
}

#[test]
fn wide_terminal_renders_full_tab_labels() {
    let buf = render_at(160);
    let row = first_row(&buf);
    assert!(
        row.contains("Dashboard") && row.contains("DAG") && row.contains("Tasks"),
        "wide row should carry full labels: {row:?}"
    );
}

#[test]
fn narrow_terminal_renders_compact_tab_labels_only() {
    let buf = render_at(80);
    let row = first_row(&buf);
    assert!(
        !row.contains("Dashboard"),
        "narrow row should NOT carry the word 'Dashboard': {row:?}"
    );
    // Every activator key is still present so the user can switch.
    // (Keys mirror the compact tab registry in src/tui/app.rs.)
    for key in ["1", "2", "3", "5", "9", "0", "G", "N"] {
        assert!(
            row.contains(key),
            "narrow row missing activator {key}: {row:?}"
        );
    }
}

#[test]
fn narrow_terminal_drops_orchestration_panel() {
    let buf = render_at(80);
    let mut all = String::new();
    for y in 0..buf.area().height {
        for x in 0..buf.area().width {
            all.push(buf[(x, y)].symbol().chars().next().unwrap_or(' '));
        }
        all.push('\n');
    }
    // The orchestration panel's title is "orchestration"; it must not
    // render when we collapsed the strip to a single column.
    assert!(
        !all.contains("orchestration"),
        "narrow render must drop the ops panel: \n{all}"
    );
}

#[test]
fn wide_terminal_keeps_orchestration_panel() {
    let buf = render_at(160);
    let mut all = String::new();
    for y in 0..buf.area().height {
        for x in 0..buf.area().width {
            all.push(buf[(x, y)].symbol().chars().next().unwrap_or(' '));
        }
        all.push('\n');
    }
    assert!(
        all.contains("orchestration"),
        "wide render must keep the ops panel: \n{all}"
    );
}

#[test]
fn boundary_width_99_collapses_width_100_does_not() {
    let narrow = render_at(99);
    let wide = render_at(100);
    let narrow_row = first_row(&narrow);
    let wide_row = first_row(&wide);
    assert!(
        !narrow_row.contains("Dashboard"),
        "width 99 must collapse: {narrow_row:?}"
    );
    assert!(
        wide_row.contains("Dashboard"),
        "width 100 must stay wide: {wide_row:?}"
    );
}
