//! Regression for the cascade ripple queue (yggdrasil-166).

use std::sync::{Mutex, MutexGuard};
use ygg::tui::motion::{MAX_RIPPLES, RIPPLE_FRAMES, RippleQueue};

// `RippleQueue::push` consults the process-global `YGG_TUI_NO_MOTION` env var
// (via `motion_disabled()`). `motion_disabled_blocks_push` mutates that global,
// so it races the push()-based tests running on sibling threads in this same
// test binary. Serialize every test in the file through one guard so the env
// var is only ever set while no other test is observing it.
static ENV_GUARD: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    ENV_GUARD.lock().unwrap_or_else(|p| p.into_inner())
}

#[test]
fn empty_queue_has_nothing_active() {
    let _g = serial();
    let q = RippleQueue::default();
    for d in 0..10 {
        assert!(!q.is_active_for_distance(d));
    }
}

#[test]
fn pushed_ripple_fires_one_distance_per_frame() {
    let _g = serial();
    let mut q = RippleQueue::default();
    q.push(42);
    // First paint pass: distance 0 (origin) is hot.
    assert!(q.is_active_for_distance(0));
    assert!(!q.is_active_for_distance(1));
    q.tick_paint();
    assert!(q.is_active_for_distance(1));
    assert!(!q.is_active_for_distance(0));
}

#[test]
fn ripple_decays_after_max_frames() {
    let _g = serial();
    let mut q = RippleQueue::default();
    q.push(1);
    for _ in 0..RIPPLE_FRAMES {
        q.tick_paint();
    }
    // Drained → no live ripples.
    assert!(q.ripples.is_empty());
    for d in 0..10 {
        assert!(!q.is_active_for_distance(d));
    }
}

#[test]
fn duplicate_origin_refreshes_rather_than_stacks() {
    let _g = serial();
    let mut q = RippleQueue::default();
    q.push(7);
    q.tick_paint();
    q.push(7); // same origin
    assert_eq!(q.ripples.len(), 1, "duplicate origin must not stack");
    // Refreshed countdown resets to RIPPLE_FRAMES.
    assert_eq!(q.ripples[0].frames_remaining, RIPPLE_FRAMES);
}

#[test]
fn capacity_bounded_to_max_ripples() {
    let _g = serial();
    let mut q = RippleQueue::default();
    for i in 0..(MAX_RIPPLES as u64 + 5) {
        q.push(i);
    }
    assert_eq!(q.ripples.len(), MAX_RIPPLES);
}

#[test]
fn motion_disabled_blocks_push() {
    let _g = serial();
    unsafe { std::env::set_var("YGG_TUI_NO_MOTION", "1") };
    let mut q = RippleQueue::default();
    q.push(1);
    assert!(q.ripples.is_empty(), "disabled motion must not enqueue");
    unsafe { std::env::remove_var("YGG_TUI_NO_MOTION") };
}
