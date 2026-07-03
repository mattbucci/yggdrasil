//! Run river — per-agent token horizon chart (yggdrasil-159).
//!
//! Tufte's horizon chart applied to LLM cost: stack each agent's
//! token-spend trace into 2 lines × N columns of half-block glyphs.
//! Coincident spikes across agents read as a synchronised storm; idle
//! troughs as ambient quiet. The single visual where "where did the
//! day's tokens go?" answers itself at a glance.
//!
//! Substrate first: this module ships the half-cosine-band folding
//! math + the glyph picker. The SQL query (rolling per-minute
//! token rate per agent) and the pane render layer compose on top.

/// One sample (per minute) of an agent's token rate.
#[derive(Debug, Clone, Copy, Default)]
pub struct RiverSample {
    pub tokens: u64,
}

/// Bands per row. Each row of the horizon chart shows three intensity
/// bands; folding above the row's max value flips into the next row.
/// Three bands × 2 rows = six effective magnitudes per agent strip.
pub const BANDS_PER_ROW: u8 = 3;

/// Half-block glyphs covering [empty .. quarter .. half .. three-quarter
/// .. full] in a single cell. Picked to compose with `▀▄` half-blocks
/// for the row split.
pub const BAND_GLYPHS: &[char] = &[' ', '▂', '▄', '▆', '█'];

/// Map a value in `[0, max]` to a glyph index 0..=4. The fold-into-rows
/// caller decides which row the glyph lands in.
pub fn glyph_for(value: u64, max: u64) -> char {
    if max == 0 {
        return BAND_GLYPHS[0];
    }
    let fraction = (value as f64 / max as f64).clamp(0.0, 1.0);
    let idx = (fraction * (BAND_GLYPHS.len() as f64 - 1.0)).round() as usize;
    BAND_GLYPHS[idx.min(BAND_GLYPHS.len() - 1)]
}

/// Fold a sample into the (top_row_glyph, bottom_row_glyph) pair the
/// horizon chart renders. The top row carries the high-intensity
/// portion of the value (everything above 50% of max); the bottom row
/// carries the low-intensity portion. Together they double the
/// effective vertical resolution without doubling row count.
pub fn fold_to_horizon(value: u64, max: u64) -> (char, char) {
    if max == 0 {
        return (' ', ' ');
    }
    let half = max / 2;
    if value <= half {
        // All in the bottom row, top row is empty.
        (' ', glyph_for(value * 2, max))
    } else {
        // Bottom row is full; top row carries the overflow.
        (glyph_for((value - half) * 2, max), '█')
    }
}

/// Compute the maximum sample across an agent strip — used as the
/// normaliser for `glyph_for` / `fold_to_horizon`. Returns 1 when
/// everything is zero so the renderer doesn't divide by zero.
pub fn strip_max(samples: &[RiverSample]) -> u64 {
    samples.iter().map(|s| s.tokens).max().unwrap_or(1).max(1)
}

/// Render an agent's strip as a (top_line, bottom_line) pair of
/// glyph strings — the renderer drops them into two stacked rows.
pub fn render_strip(samples: &[RiverSample]) -> (String, String) {
    let max = strip_max(samples);
    let mut top = String::with_capacity(samples.len());
    let mut bottom = String::with_capacity(samples.len());
    for s in samples {
        let (t, b) = fold_to_horizon(s.tokens, max);
        top.push(t);
        bottom.push(b);
    }
    (top, bottom)
}
