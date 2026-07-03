//! Motion vocabulary for the TUI (yggdrasil-165 + yggdrasil-169).
//!
//! Four named primitives, each with a fixed semantic role; mixing them
//! arbitrarily produces noise, so future panes are expected to pick the
//! one that matches the event class:
//!
//! | Primitive | Frames | Use for                          |
//! |-----------|--------|----------------------------------|
//! | flash     | 1      | discrete state transition         |
//! | pulse     | 3      | milestone / completion            |
//! | ripple    | N      | cascade across rows (yggdrasil-166) |
//! | slide     | 2      | row arrival / departure           |
//!
//! Plus two **continuous** primitives that don't fit the discrete
//! taxonomy but solve the "frozen screen" feel without stealing data:
//!
//! - **breathing**: sine-wave opacity (intensity) for the dot indicator
//!   on active agents. Idle agents stay solid so motion = signal.
//! - **sparkline tail**: a 1-cell-tall braille glyph that scrolls left
//!   on each refresh tick — visible heartbeat of the pane.
//!
//! The Stardew/Vampire-Survivors anti-patterns (color-cycling borders,
//! decorative spinners, animations longer than 500ms) live in the test
//! file as documented "do not regress" guards.

use ratatui::style::{Color, Modifier, Style};

/// Length of the discrete-flash window in paint passes. One pass at the
/// existing 500ms tick = ~1s of inverted highlight; the eye catches it
/// without it feeling laggy.
pub const FLASH_FRAMES: u8 = 1;

/// Length of the milestone-pulse window. Three frames step through a
/// short bold→dim fade.
pub const PULSE_FRAMES: u8 = 3;

/// Length of one slide-in animation (row insert / detach).
pub const SLIDE_FRAMES: u8 = 2;

/// Hard ceiling on any single animation. The engagement-UX research
/// (research synthesis 2026-04-26) called out >500ms as the boundary
/// where stale animations look "broken" to users tabbing in and out.
/// At a 2 Hz refresh that's one full second; no primitive should exceed
/// it. Tests guard the FLASH/PULSE/SLIDE/RIPPLE constants against this.
pub const MAX_ANIMATION_FRAMES: u8 = 4;

/// Number of cascade ripple frames — used by yggdrasil-166. Each frame
/// the ripple advances one row outward from the originating row, then
/// fades at the edge.
pub const RIPPLE_FRAMES: u8 = 4;

/// Apply the flash modifier (REVERSED) to `base` if `frames_remaining`
/// is non-zero. The shared decorator across every flash callsite so a
/// future swap to e.g. `Modifier::SLOW_BLINK` lands in one place.
pub fn flash_style(base: Style, frames_remaining: u8) -> Style {
    if frames_remaining > 0 {
        base.add_modifier(Modifier::REVERSED)
    } else {
        base
    }
}

/// Pulse: BOLD on the rising edge, normal on the trailing edge.
/// `frame` counts down from `PULSE_FRAMES` to 0; we BOLD the first half
/// and dim the second so the eye sees a soft "ping" rather than a hard
/// flash.
pub fn pulse_style(base: Style, frames_remaining: u8) -> Style {
    if frames_remaining == 0 {
        return base;
    }
    if frames_remaining > PULSE_FRAMES / 2 {
        base.add_modifier(Modifier::BOLD)
    } else {
        base.add_modifier(Modifier::DIM)
    }
}

/// Slide: render the cell with REVERSED on the first frame, then BOLD
/// on the second, so a newly-arriving row reads as "popping in" without
/// staying highlighted.
pub fn slide_style(base: Style, frames_remaining: u8) -> Style {
    match frames_remaining {
        2 => base.add_modifier(Modifier::REVERSED),
        1 => base.add_modifier(Modifier::BOLD),
        _ => base,
    }
}

/// Ripple: row at `distance` from origin gets the highlight when the
/// ripple is `distance + 1` frames into its lifetime. Returns whether
/// the cell at `(row_index, distance_from_origin)` should currently be
/// highlighted given the ripple's countdown timer `frames_remaining`.
pub fn ripple_active_at(distance: usize, frames_remaining: u8) -> bool {
    let lived_for = RIPPLE_FRAMES.saturating_sub(frames_remaining);
    lived_for as usize == distance
}

/// Compute a [0.0, 1.0] "breath" intensity for `tick`. Twelve ticks is
/// one full inhale-exhale cycle (≈6 s at the 500ms refresh tick). The
/// curve is a half-cosine so the intensity peaks softly and dwells at
/// both extremes — feels organic compared to a triangle wave.
pub fn breath_intensity(tick: u64) -> f64 {
    const PERIOD: u64 = 12;
    let phase = (tick % PERIOD) as f64 / PERIOD as f64;
    // Half-cosine: 0.5 - 0.5*cos(2*pi*phase) gives a smooth 0..1..0.
    0.5 - 0.5 * (2.0 * std::f64::consts::PI * phase).cos()
}

/// Map a breath intensity (0.0–1.0) to a foreground color: dim
/// `Color::DarkGray` at trough, full `Color::Green` at crest. Used on
/// active-agent state indicators in the dashboard.
pub fn breath_color(intensity: f64) -> Color {
    if intensity > 0.66 {
        Color::Green
    } else if intensity > 0.33 {
        Color::LightGreen
    } else {
        Color::DarkGray
    }
}

/// Master kill-switch shared by every motion primitive. The same env
/// var used by yggdrasil-152's flash decorator so users have one knob
/// for "give me a quiet TUI" rather than five.
pub fn motion_disabled() -> bool {
    matches!(
        std::env::var("YGG_TUI_NO_MOTION").ok().as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// Live cascade-ripple state (yggdrasil-166). When a "signature event"
/// fires (today: a task closes), one of these gets pushed onto a small
/// queue. `tick_paint` decrements the countdown; rendered cells consult
/// `is_active_for_distance` to decide whether to paint REVERSED.
///
/// The queue is bounded — at most `MAX_RIPPLES` simultaneous ripples
/// run; any more get dropped from the front when a new one arrives.
/// The cap of 4 plays nicely with the tick budget without smearing
/// into strobing noise.
#[derive(Debug, Clone, Default)]
pub struct RippleQueue {
    pub ripples: Vec<Ripple>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ripple {
    /// Stable identifier for "what triggered this" — usually a task_ref
    /// hash so a new event on the same task refreshes rather than
    /// stacking.
    pub origin_key: u64,
    pub frames_remaining: u8,
}

/// Soft cap on simultaneous ripples. Prevents a `task close` storm
/// from turning the whole table into a strobe; older ripples drop off
/// the front when a new one is pushed past the cap.
pub const MAX_RIPPLES: usize = 4;

impl RippleQueue {
    /// Push a new ripple keyed by `origin_key`. If a ripple with the
    /// same key is already running, refresh its countdown rather than
    /// stacking — same logical event, same animation.
    pub fn push(&mut self, origin_key: u64) {
        if motion_disabled() {
            return;
        }
        if let Some(existing) = self.ripples.iter_mut().find(|r| r.origin_key == origin_key) {
            existing.frames_remaining = RIPPLE_FRAMES;
            return;
        }
        self.ripples.push(Ripple {
            origin_key,
            frames_remaining: RIPPLE_FRAMES,
        });
        if self.ripples.len() > MAX_RIPPLES {
            self.ripples.remove(0);
        }
    }

    /// Saturating-decrement every active ripple; drop expired ones.
    pub fn tick_paint(&mut self) {
        for r in self.ripples.iter_mut() {
            r.frames_remaining = r.frames_remaining.saturating_sub(1);
        }
        self.ripples.retain(|r| r.frames_remaining > 0);
    }

    /// True iff any active ripple wants `distance` to be highlighted on
    /// this paint pass. Rendered cells call this with their row index
    /// relative to the ripple's origin.
    pub fn is_active_for_distance(&self, distance: usize) -> bool {
        self.ripples
            .iter()
            .any(|r| ripple_active_at(distance, r.frames_remaining))
    }
}
