//! Spinner widget + per-pane pending state (yggdrasil-154).
//!
//! Today every pane refreshes synchronously inside the render loop —
//! a slow database query freezes the whole UI. The plan (laid out in
//! the orchestrator-pattern research) is to move every refresh to a
//! `tokio::spawn` returning over an mpsc channel; while the result is
//! pending, the pane shows a braille spinner in its title bar so the
//! user sees "alive, waiting" rather than "frozen".
//!
//! This module ships:
//!   - the braille frame cycle + `frame_at(tick)` picker;
//!   - `PendingSet<K>` — a generic per-key pending tracker so panes
//!     can flag individual rows mid-flight when the bulk-action
//!     handlers (yggdrasil-129) start streaming results;
//!   - a `pending_glyph_for_age(elapsed)` helper that escalates the
//!     glyph from spinner → "still working" → "stuck?" so the user
//!     gets feedback on long queries without re-implementing the
//!     escalation in every callsite.

use std::collections::BTreeSet;
use std::time::Duration;

/// Braille spinner frames. Eight frames at 2 Hz tick = 4 s full cycle —
/// fast enough to read as motion, slow enough not to flicker.
pub const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'];

/// Pick the spinner glyph for a global `tick` counter. The renderer
/// passes its own paint counter so all spinners across the TUI step
/// in sync — looks better than each row picking its own phase.
pub fn frame_at(tick: u64) -> char {
    SPINNER_FRAMES[(tick as usize) % SPINNER_FRAMES.len()]
}

/// Generic per-key pending tracker. Panes hand out `K` (Uuid for
/// rows, String for resource names) when an async query starts and
/// pop it on completion; `is_pending(k)` answers whether to render
/// the spinner cell.
#[derive(Debug, Clone, Default)]
pub struct PendingSet<K: Ord + Clone> {
    pending: BTreeSet<K>,
}

impl<K: Ord + Clone> PendingSet<K> {
    pub fn new() -> Self {
        Self {
            pending: BTreeSet::new(),
        }
    }

    pub fn mark(&mut self, key: K) {
        self.pending.insert(key);
    }

    pub fn clear(&mut self, key: &K) {
        self.pending.remove(key);
    }

    pub fn is_pending(&self, key: &K) -> bool {
        self.pending.contains(key)
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

/// Long-query escalation. After 2 s the glyph adds an exclamation hint;
/// after 8 s it switches to a warning glyph so the user knows
/// something is genuinely wrong vs. just slow. Tick counter feeds the
/// underlying spinner; `elapsed` drives the escalation.
pub fn pending_glyph_for_age(tick: u64, elapsed: Duration) -> char {
    if elapsed >= Duration::from_secs(8) {
        '⚠'
    } else if elapsed >= Duration::from_secs(2) {
        // Heavier braille frames carry the "still working" hint.
        const SLOW_FRAMES: &[char] = &['⣾', '⣽', '⣻', '⢿', '⡿', '⣟', '⣯', '⣷'];
        SLOW_FRAMES[(tick as usize) % SLOW_FRAMES.len()]
    } else {
        frame_at(tick)
    }
}
