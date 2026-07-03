//! Regression for the disclosure governor (yggdrasil-145).

use std::time::{Duration, Instant};
use ygg::tui::disclosure_governor::{
    ABSOLUTE_REFRACTORY, DisclosureGovernor, GLOBAL_COOLDOWN, GLOBAL_WINDOW,
    MAX_DISCLOSURES_PER_WINDOW,
};

#[test]
fn first_disclosure_passes_through() {
    let mut g = DisclosureGovernor::new();
    let now = Instant::now();
    assert!(g.try_admit_at("agent-a", 1.0, now));
}

#[test]
fn refractory_blocks_same_sender_within_window() {
    let mut g = DisclosureGovernor::new();
    let t0 = Instant::now();
    assert!(g.try_admit_at("agent-a", 1.0, t0));
    let t1 = t0 + Duration::from_secs(10);
    // Different sender allowed by refractory but blocked by global cooldown.
    assert!(!g.try_admit_at("agent-a", 1.0, t1));
}

#[test]
fn global_cooldown_blocks_other_senders_too() {
    let mut g = DisclosureGovernor::new();
    let t0 = Instant::now();
    assert!(g.try_admit_at("a", 1.0, t0));
    let t1 = t0 + Duration::from_secs(5);
    assert!(!g.try_admit_at("b", 1.0, t1));
    let t2 = t0 + GLOBAL_COOLDOWN + Duration::from_secs(1);
    assert!(g.try_admit_at("b", 1.0, t2));
}

#[test]
fn rolling_window_caps_total_disclosures() {
    let mut g = DisclosureGovernor::new();
    let t0 = Instant::now();
    let mut t = t0;
    for i in 0..MAX_DISCLOSURES_PER_WINDOW {
        // Different sender + past global cooldown each time.
        let sender = format!("a{i}");
        assert!(
            g.try_admit_at(&sender, 1.0, t),
            "fire {i} should pass while under cap"
        );
        t = t + GLOBAL_COOLDOWN + Duration::from_secs(1);
    }
    // One more inside the window must be gated.
    let sender = "a-overflow".to_string();
    assert!(
        !g.try_admit_at(&sender, 1.0, t),
        "overflow fire must be blocked"
    );
}

#[test]
fn rolling_window_re_admits_after_horizon_elapses() {
    let mut g = DisclosureGovernor::new();
    let t0 = Instant::now();
    g.try_admit_at("a", 1.0, t0);
    // Past the global window — the rolling cap is no longer binding.
    // Sender "a"'s threshold multiplier is still partly elevated
    // (half-life ≈ 395 s) so the new event needs a louder magnitude
    // to clear; that's the whole point of the multiplier.
    let later = t0 + GLOBAL_WINDOW + Duration::from_secs(1);
    assert!(g.try_admit_at("a", 5.0, later));
}

#[test]
fn refractory_blocks_same_sender_below_threshold_after_cooldown() {
    let mut g = DisclosureGovernor::new();
    let t0 = Instant::now();
    g.try_admit_at("a", 1.0, t0);
    // After global cooldown but still within absolute refractory:
    // sender "a" must be blocked even with high score.
    let t1 = t0 + GLOBAL_COOLDOWN + Duration::from_secs(1);
    assert!(t1 - t0 < ABSOLUTE_REFRACTORY);
    assert!(!g.try_admit_at("a", 5.0, t1));
}
