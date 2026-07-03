//! Regression for memory-similarity ridgeline math (yggdrasil-164).

use ygg::tui::ridgeline::{BIN_COUNT, RidgeQuality, bin_ridge, quality_class, render_ridge};

#[test]
fn empty_scores_produce_zero_bins() {
    let r = bin_ridge("q", &[]);
    assert_eq!(r.bins.len(), BIN_COUNT);
    assert!(r.bins.iter().all(|&b| b == 0));
}

#[test]
fn single_score_lands_in_correct_bin() {
    // 0.5 should land in the middle bin.
    let r = bin_ridge("q", &[0.5]);
    let mid = BIN_COUNT / 2;
    assert_eq!(r.bins[mid], 1);
    let total: u32 = r.bins.iter().sum();
    assert_eq!(total, 1);
}

#[test]
fn out_of_range_scores_clamp_into_endpoint_bins() {
    let r = bin_ridge("q", &[-0.2, 1.5]);
    assert_eq!(r.bins[0], 1);
    assert_eq!(r.bins[BIN_COUNT - 1], 1);
}

#[test]
fn render_ridge_produces_one_glyph_per_bin() {
    let r = bin_ridge("q", &[0.5; 4]);
    let glyphs = render_ridge(&r);
    assert_eq!(glyphs.chars().count(), BIN_COUNT);
}

#[test]
fn empty_ridge_renders_spaces() {
    let r = bin_ridge("q", &[]);
    let glyphs = render_ridge(&r);
    assert!(glyphs.chars().all(|c| c == ' '));
}

#[test]
fn quality_class_strong_when_top_band_dominates() {
    // 80% of weight in [0.85, 1.0] should read as Strong.
    let mut scores: Vec<f64> = vec![0.95; 8];
    scores.extend(vec![0.20; 2]);
    let r = bin_ridge("q", &scores);
    assert_eq!(quality_class(&r), RidgeQuality::Strong);
}

#[test]
fn quality_class_weak_when_top_band_empty() {
    // No high-band hits → Weak.
    let r = bin_ridge("q", &[0.30; 10]);
    assert_eq!(quality_class(&r), RidgeQuality::Weak);
}

#[test]
fn quality_class_empty_for_zero_data() {
    let r = bin_ridge("q", &[]);
    assert_eq!(quality_class(&r), RidgeQuality::Empty);
}
