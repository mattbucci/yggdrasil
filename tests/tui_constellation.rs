//! Regression for the agent-constellation force-directed layout
//! (yggdrasil-160).

use uuid::Uuid;
use ygg::tui::constellation::{AgentEdge, AgentNode, cluster_of, relax};

fn node(label: &str, x: f32, y: f32) -> AgentNode {
    AgentNode {
        agent_id: Uuid::new_v4().to_string(),
        label: label.into(),
        x,
        y,
        vx: 0.0,
        vy: 0.0,
    }
}

fn edge(from: String, to: String) -> AgentEdge {
    AgentEdge {
        from,
        to,
        weight: 1.0,
    }
}

#[test]
fn empty_relax_is_safe() {
    let mut nodes: Vec<AgentNode> = Vec::new();
    relax(&mut nodes, &[], 0.85);
    assert!(nodes.is_empty());
}

#[test]
fn single_node_does_not_move() {
    let mut nodes = vec![node("a", 0.5, 0.5)];
    let before = (nodes[0].x, nodes[0].y);
    relax(&mut nodes, &[], 0.85);
    assert_eq!((nodes[0].x, nodes[0].y), before);
}

#[test]
fn relax_keeps_positions_inside_unit_square() {
    let mut nodes = vec![
        node("a", 0.0, 0.0),
        node("b", 1.0, 1.0),
        node("c", 0.5, 0.5),
    ];
    for _ in 0..20 {
        relax(&mut nodes, &[], 0.85);
    }
    for n in &nodes {
        assert!(n.x >= 0.0 && n.x <= 1.0, "x out of range: {}", n.x);
        assert!(n.y >= 0.0 && n.y <= 1.0, "y out of range: {}", n.y);
    }
}

#[test]
fn cluster_of_isolated_nodes_returns_self_root() {
    let nodes = vec![node("a", 0.0, 0.0), node("b", 1.0, 1.0)];
    let parents = cluster_of(&nodes, &[]);
    assert_eq!(parents[&nodes[0].agent_id], nodes[0].agent_id);
    assert_eq!(parents[&nodes[1].agent_id], nodes[1].agent_id);
}

#[test]
fn cluster_of_groups_connected_nodes_under_one_root() {
    let a = node("a", 0.0, 0.0);
    let b = node("b", 0.5, 0.5);
    let c = node("c", 1.0, 1.0);
    let nodes = vec![a.clone(), b.clone(), c.clone()];
    let edges = vec![
        edge(a.agent_id.clone(), b.agent_id.clone()),
        edge(b.agent_id.clone(), c.agent_id.clone()),
    ];
    let parents = cluster_of(&nodes, &edges);
    let ra = &parents[&a.agent_id].clone();
    let rb = &parents[&b.agent_id].clone();
    let rc = &parents[&c.agent_id].clone();
    assert_eq!(ra, rb);
    assert_eq!(rb, rc);
}

#[test]
fn cluster_of_keeps_disconnected_components_separate() {
    let a = node("a", 0.0, 0.0);
    let b = node("b", 0.5, 0.5);
    let c = node("c", 1.0, 1.0);
    let d = node("d", 0.2, 0.7);
    let nodes = vec![a.clone(), b.clone(), c.clone(), d.clone()];
    // a-b in one component, c-d in another.
    let edges = vec![
        edge(a.agent_id.clone(), b.agent_id.clone()),
        edge(c.agent_id.clone(), d.agent_id.clone()),
    ];
    let parents = cluster_of(&nodes, &edges);
    assert_eq!(parents[&a.agent_id], parents[&b.agent_id]);
    assert_eq!(parents[&c.agent_id], parents[&d.agent_id]);
    assert_ne!(parents[&a.agent_id], parents[&c.agent_id]);
}
