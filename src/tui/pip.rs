//! PiP (picture-in-picture) transcript popup (yggdrasil-167). Hover-
//! dwell on an agent row → a floating 40×10 panel of the agent's last
//! N stdout lines, dismissable, follows selection. Solves the real
//! user job (passive fleet watching with intervention bursts) better
//! than any animation.
//!
//! State machine + ring buffer here; the actual `tmux pipe-pane` tail
//! that fills the buffer + the ratatui render layer compose on top
//! once yggdrasil-158 (tui-term + portable-pty) lands.

use std::time::{Duration, Instant};

#[derive(Debug, Clone, Default)]
pub struct PipState {
    /// `Some(_)` while a hover is being timed; `None` means nothing
    /// is dwelling.
    pub hover: Option<HoverTimer>,
    /// `Some(_)` once dwell elapsed and the popup opened; carries the
    /// agent name + transcript ring.
    pub open: Option<PipOpen>,
}

#[derive(Debug, Clone)]
pub struct HoverTimer {
    pub agent: String,
    pub started_at: Instant,
}

#[derive(Debug, Clone)]
pub struct PipOpen {
    pub agent: String,
    pub lines: std::collections::VecDeque<String>,
}

/// Hover dwell threshold — how long the cursor must rest on a row
/// before the popup auto-opens. Engagement-UX research called for
/// 200 ms; the brain reads cursor-rest as intent at that point.
pub const HOVER_DWELL: Duration = Duration::from_millis(200);

/// Maximum stdout lines retained per popup. 200 lines × 80 cols
/// covers a typical Claude turn end-to-end without bloating memory.
pub const TRANSCRIPT_CAP: usize = 200;

impl PipState {
    /// User cursor moved to a new row. If the same agent was already
    /// being hovered, leave the timer alone; otherwise reset it. The
    /// open popup tracks the cursor — moving to a different agent
    /// retargets without closing.
    pub fn cursor_on(&mut self, agent: impl Into<String>) {
        let name = agent.into();
        if let Some(open) = &self.open {
            if open.agent == name {
                self.hover = None;
                return;
            }
            // Different agent under cursor — close the old popup; a
            // fresh hover starts the dwell timer.
            self.open = None;
        }
        if self.hover.as_ref().map(|h| &h.agent) == Some(&name) {
            return;
        }
        self.hover = Some(HoverTimer {
            agent: name,
            started_at: Instant::now(),
        });
    }

    /// User moved cursor away. Drops both the hover timer and any
    /// open popup so we never leak a popup pinned to a row that left.
    pub fn cursor_off(&mut self) {
        self.hover = None;
        self.open = None;
    }

    /// Called every paint pass. Promotes a dwelling hover to an open
    /// popup once HOVER_DWELL elapses. Returns whether the popup
    /// state changed (caller can use this to gate extra DB queries).
    pub fn tick(&mut self) -> bool {
        let Some(timer) = &self.hover else {
            return false;
        };
        if timer.started_at.elapsed() < HOVER_DWELL {
            return false;
        }
        let agent = timer.agent.clone();
        self.hover = None;
        self.open = Some(PipOpen {
            agent,
            lines: std::collections::VecDeque::with_capacity(TRANSCRIPT_CAP),
        });
        true
    }

    /// Append a new transcript line to the open popup. No-op when no
    /// popup is open. Trims to TRANSCRIPT_CAP from the front.
    pub fn push_line(&mut self, line: impl Into<String>) {
        let Some(open) = &mut self.open else {
            return;
        };
        if open.lines.len() == TRANSCRIPT_CAP {
            open.lines.pop_front();
        }
        open.lines.push_back(line.into());
    }

    /// Esc / explicit close.
    pub fn close(&mut self) {
        self.open = None;
        self.hover = None;
    }

    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }
}
