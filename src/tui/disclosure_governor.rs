//! Disclosure governor (yggdrasil-145). Two-stage gate before any
//! "poke" reaches the user: per-sender action-potential + global
//! cooldown. Holds the TUI back from notification storms when fleets
//! get chatty.
//!
//! Per-sender state borrows agent-ways' "EngagementState":
//!   - absolute refractory (60 s) — hard wall after a fire
//!   - exponentially-decaying threshold elevation
//!   - per-peer magnitude boost (1.0 / 1.75 / 2.5) so active
//!     conversation partners climb above noise without a group infra
//!
//! Global state caps disclosures at 3 per 120 s rolling window.
//! Held-back events stay on the accumulator; they re-fire when the
//! window rolls.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// Hard refractory after a sender fires — no further pokes from that
/// sender for this long. Default 60 s mirrors the agent-ways setting.
pub const ABSOLUTE_REFRACTORY: Duration = Duration::from_secs(60);

/// Half-life for the post-fire threshold elevation. Returns to
/// baseline by ~395 s; multiplier peaks at 2.25× immediately after a
/// fire and decays exponentially.
pub const MULTIPLIER_HALF_LIFE: Duration = Duration::from_secs(395);

/// Peak threshold multiplier right after a fire. Above this, the
/// sender's events are gated until the multiplier decays back below
/// `1.0 + epsilon`.
pub const PEAK_MULTIPLIER: f64 = 2.25;

/// Global cooldown — minimum gap between *any* two disclosures across
/// all senders.
pub const GLOBAL_COOLDOWN: Duration = Duration::from_secs(15);

/// Rolling window for `MAX_DISCLOSURES_PER_WINDOW`.
pub const GLOBAL_WINDOW: Duration = Duration::from_secs(120);

/// Cap on disclosures within `GLOBAL_WINDOW`. Above the cap, events
/// queue on the accumulator and fire when the window rolls.
pub const MAX_DISCLOSURES_PER_WINDOW: usize = 3;

#[derive(Debug, Clone, Default)]
struct SenderState {
    /// `Some(_)` after a fire; `None` once the refractory has elapsed
    /// AND the multiplier has decayed back to ~1.0.
    last_fire: Option<Instant>,
}

#[derive(Debug, Default)]
pub struct DisclosureGovernor {
    senders: HashMap<String, SenderState>,
    /// Recent fire timestamps (across all senders) for the rolling
    /// window check. Trimmed lazily on every `try_admit`.
    fires: VecDeque<Instant>,
}

impl DisclosureGovernor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Try to admit a disclosure from `sender` with magnitude `score`
    /// (any positive number; higher = louder). Returns `true` if the
    /// caller should fire the disclosure now; `false` if gated.
    pub fn try_admit(&mut self, sender: &str, score: f64) -> bool {
        self.try_admit_at(sender, score, Instant::now())
    }

    /// Test-only entry point with an explicit `now` so tests don't
    /// have to thread.sleep through actual seconds.
    pub fn try_admit_at(&mut self, sender: &str, score: f64, now: Instant) -> bool {
        // Trim the global window to entries inside [now - GLOBAL_WINDOW, now].
        while let Some(t) = self.fires.front() {
            if now.duration_since(*t) > GLOBAL_WINDOW {
                self.fires.pop_front();
            } else {
                break;
            }
        }

        // Global rolling-window cap.
        if self.fires.len() >= MAX_DISCLOSURES_PER_WINDOW {
            return false;
        }
        // Global cooldown.
        if let Some(last) = self.fires.back()
            && now.duration_since(*last) < GLOBAL_COOLDOWN
        {
            return false;
        }

        // Per-sender refractory + threshold check.
        let entry = self.senders.entry(sender.to_string()).or_default();
        if let Some(last) = entry.last_fire {
            let since = now.duration_since(last);
            if since < ABSOLUTE_REFRACTORY {
                return false;
            }
            // Exponential decay: multiplier(t) = 1 + (PEAK-1) * 2^(-t/half_life).
            let elapsed_secs = since.as_secs_f64();
            let half_life = MULTIPLIER_HALF_LIFE.as_secs_f64();
            let decay = 2_f64.powf(-elapsed_secs / half_life);
            let multiplier = 1.0 + (PEAK_MULTIPLIER - 1.0) * decay;
            if score < multiplier {
                return false;
            }
        }

        // Admit. Record both per-sender + global timestamps.
        entry.last_fire = Some(now);
        self.fires.push_back(now);
        true
    }

    pub fn fire_count_in_window(&self) -> usize {
        self.fires.len()
    }
}
