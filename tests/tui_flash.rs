//! Regression tests for the cell-level flash decorator (yggdrasil-152).

use ygg::tui::app::{FlashState, OpsStats};

use std::sync::{Mutex, MutexGuard, OnceLock};

/// Serialize the tests in this file. `flash_disabled_via_env_var` mutates the
/// process-global `YGG_TUI_NO_FLASH`, which `FlashState::mark_changes` reads —
/// so while it is set, a sibling test running on another thread would observe
/// flashing disabled and fail spuriously. Every test takes this lock so the
/// env-var test never overlaps the others (pre-existing race, yggdrasil-152).
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn ops(alive: i64, stuck: i64, tasks: i64, sessions: i64) -> OpsStats {
    OpsStats {
        agents_alive: alive,
        agents_stuck: stuck,
        tasks_running: tasks,
        live_sessions: sessions,
        db_ms: 0,
        // Burn-rate fields (yggdrasil-148): not exercised by these
        // tests; default to zero so the struct literal stays complete.
        tokens_per_min: 0.0,
        cost_today_usd: 0.0,
        tokens_today: 0,
        // DB-side signals (yggdrasil-177): same defaults — these
        // tests only flex agents/stuck/tasks/sessions cells.
        pool_used: 0,
        pool_max: 0,
        events_per_min: 0,
    }
}

#[test]
fn unchanged_snapshot_does_not_flash_anything() {
    let _g = serial();
    let mut f = FlashState::default();
    f.mark_changes(&ops(3, 0, 1, 2), &ops(3, 0, 1, 2), 2);
    assert!(!f.is_flashing_alive());
    assert!(!f.is_flashing_stuck());
    assert!(!f.is_flashing_tasks());
    assert!(!f.is_flashing_sessions());
}

#[test]
fn changed_alive_count_flashes_only_alive() {
    let _g = serial();
    let mut f = FlashState::default();
    f.mark_changes(&ops(3, 0, 1, 2), &ops(4, 0, 1, 2), 2);
    assert!(f.is_flashing_alive());
    assert!(!f.is_flashing_stuck());
    assert!(!f.is_flashing_tasks());
    assert!(!f.is_flashing_sessions());
}

#[test]
fn flash_decays_to_zero_after_n_paints() {
    let _g = serial();
    let mut f = FlashState::default();
    f.mark_changes(&ops(0, 0, 0, 0), &ops(1, 0, 0, 0), 2);
    assert!(f.is_flashing_alive());
    f.tick_paint();
    assert!(f.is_flashing_alive());
    f.tick_paint();
    assert!(!f.is_flashing_alive(), "flash should expire after 2 paints");
}

#[test]
fn flash_disabled_via_env_var() {
    let _g = serial();
    // Holds `serial()` across the whole set/remove window so no sibling test
    // observes YGG_TUI_NO_FLASH while it is set.
    unsafe { std::env::set_var("YGG_TUI_NO_FLASH", "1") };
    let mut f = FlashState::default();
    f.mark_changes(&ops(0, 0, 0, 0), &ops(1, 1, 1, 1), 2);
    assert!(!f.is_flashing_alive());
    assert!(!f.is_flashing_stuck());
    assert!(!f.is_flashing_tasks());
    assert!(!f.is_flashing_sessions());
    unsafe { std::env::remove_var("YGG_TUI_NO_FLASH") };
}

#[test]
fn re_marking_a_live_flash_resets_the_window() {
    let _g = serial();
    let mut f = FlashState::default();
    f.mark_changes(&ops(0, 0, 0, 0), &ops(1, 0, 0, 0), 3);
    f.tick_paint(); // 2 left
    f.mark_changes(&ops(1, 0, 0, 0), &ops(2, 0, 0, 0), 3);
    // Re-mark resets to 3, not adds.
    assert_eq!(f.agents_alive, 3);
}

#[test]
fn each_field_is_independent() {
    let _g = serial();
    let mut f = FlashState::default();
    f.mark_changes(&ops(0, 0, 0, 0), &ops(1, 0, 1, 0), 2);
    assert!(f.is_flashing_alive());
    assert!(!f.is_flashing_stuck());
    assert!(f.is_flashing_tasks());
    assert!(!f.is_flashing_sessions());
}
