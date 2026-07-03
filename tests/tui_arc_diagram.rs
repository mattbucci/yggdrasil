//! Regression for the DAG arc-diagram layout math (yggdrasil-162).

use uuid::Uuid;
use ygg::tui::arc_diagram::{
    DagEdge, DagNode, assign_positions, critical_path, longest_path_depths,
};

fn node(label: &str) -> DagNode {
    DagNode {
        task_id: Uuid::new_v4().to_string(),
        label: label.into(),
    }
}

fn edge(from: String, to: String) -> DagEdge {
    DagEdge { from, to }
}

#[test]
fn isolated_node_is_at_depth_zero() {
    let n = node("a");
    let depths = longest_path_depths(&[n.clone()], &[]);
    assert_eq!(depths.get(&n.task_id), Some(&0));
}

#[test]
fn linear_chain_increments_depth() {
    let a = node("a");
    let b = node("b");
    let c = node("c");
    let depths = longest_path_depths(
        &[a.clone(), b.clone(), c.clone()],
        &[
            edge(a.task_id.clone(), b.task_id.clone()),
            edge(b.task_id.clone(), c.task_id.clone()),
        ],
    );
    assert_eq!(depths[&a.task_id], 0);
    assert_eq!(depths[&b.task_id], 1);
    assert_eq!(depths[&c.task_id], 2);
}

#[test]
fn diamond_takes_longest_path_for_depth() {
    // a → b → d
    // a → c → d  (also a → b → c → d adds an extra level)
    let a = node("a");
    let b = node("b");
    let c = node("c");
    let d = node("d");
    let depths = longest_path_depths(
        &[a.clone(), b.clone(), c.clone(), d.clone()],
        &[
            edge(a.task_id.clone(), b.task_id.clone()),
            edge(a.task_id.clone(), c.task_id.clone()),
            edge(b.task_id.clone(), c.task_id.clone()),
            edge(c.task_id.clone(), d.task_id.clone()),
        ],
    );
    assert_eq!(depths[&d.task_id], 3, "longest path a→b→c→d = depth 3");
}

#[test]
fn assign_positions_groups_same_depth_into_distinct_columns() {
    let a = node("a");
    let b = node("b");
    let c = node("c");
    // Two roots + one child: a, b at depth 0; c child of a at depth 1.
    let depths = longest_path_depths(
        &[a.clone(), b.clone(), c.clone()],
        &[edge(a.task_id.clone(), c.task_id.clone())],
    );
    let pos = assign_positions(&[a.clone(), b.clone(), c.clone()], &depths);
    assert_eq!(pos[&a.task_id].column, 0);
    assert_eq!(pos[&b.task_id].column, 1);
    assert_eq!(pos[&c.task_id].column, 0); // first at depth=1
}

#[test]
fn critical_path_walks_longest_chain() {
    // Chain a→b→c (length 3) and side a→d (length 2).
    let a = node("a");
    let b = node("b");
    let c = node("c");
    let d = node("d");
    let path = critical_path(
        &[a.clone(), b.clone(), c.clone(), d.clone()],
        &[
            edge(a.task_id.clone(), b.task_id.clone()),
            edge(b.task_id.clone(), c.task_id.clone()),
            edge(a.task_id.clone(), d.task_id.clone()),
        ],
    );
    assert_eq!(path.len(), 3);
    assert_eq!(*path.first().unwrap(), a.task_id);
    assert_eq!(*path.last().unwrap(), c.task_id);
}

#[test]
fn empty_input_returns_empty_critical_path() {
    let path = critical_path(&[], &[]);
    assert!(path.is_empty());
}
