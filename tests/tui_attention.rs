//! Regression for attention-item label formatting + counting (yggdrasil-130).

use chrono::Utc;
use ygg::tui::attention::{
    AttentionItem, AttentionKind, NUDGE_THRESHOLD, WAITING_TOOL_THRESHOLD_SECS, count, item_label,
};

#[test]
fn waiting_tool_label_carries_agent_name_and_idle_secs() {
    let i = AttentionItem {
        since: Utc::now(),
        kind: AttentionKind::AgentWaitingTool {
            agent: "test-agent".into(),
            idle_secs: 720,
        },
    };
    let label = item_label(&i);
    assert!(label.contains("test-agent"));
    assert!(label.contains("720"));
    assert!(label.contains("waiting"));
}

#[test]
fn awaiting_review_label_carries_task_ref() {
    let i = AttentionItem {
        since: Utc::now(),
        kind: AttentionKind::RunAwaitingReview {
            task_ref: "yggdrasil-42".into(),
            run_id: String::new(),
        },
    };
    let label = item_label(&i);
    assert!(label.contains("yggdrasil-42"));
    assert!(label.contains("review"));
}

#[test]
fn nudged_label_includes_count() {
    let i = AttentionItem {
        since: Utc::now(),
        kind: AttentionKind::RunRepeatedlyNudged {
            task_ref: "ygg-123".into(),
            nudges: 4,
        },
    };
    let label = item_label(&i);
    assert!(label.contains("4×"));
    assert!(label.contains("ygg-123"));
}

#[test]
fn count_returns_slice_length() {
    let items = vec![
        AttentionItem {
            since: Utc::now(),
            kind: AttentionKind::AgentWaitingTool {
                agent: "a".into(),
                idle_secs: 700,
            },
        },
        AttentionItem {
            since: Utc::now(),
            kind: AttentionKind::RunAwaitingReview {
                task_ref: "t-1".into(),
                run_id: String::new(),
            },
        },
    ];
    assert_eq!(count(&items), 2);
}

#[test]
fn thresholds_are_within_research_recommendations() {
    // Engagement-UX research called for ≥10 min wait threshold and
    // ≥2 nudges before raising attention. Pin the constants so a
    // reflex tweak surfaces in review.
    assert!(WAITING_TOOL_THRESHOLD_SECS >= 600);
    assert!(NUDGE_THRESHOLD >= 2);
}
