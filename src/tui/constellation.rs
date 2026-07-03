//! Agent constellation — force-directed graph (yggdrasil-160). Nodes
//! are agents; edges are shared resources (locks, mutual files).
//! Cluster reveals which agents are *entangled* on the same work — a
//! row table can't show "these three agents are circling the same
//! migration file."
//!
//! This module ships the layout solver:
//!   - Fruchterman-Reingold relaxed positions (one tick per render),
//!   - cluster detection via union-find on edges (so the renderer can
//!     colour clusters distinctly),
//!   - bounding-box helper that keeps positions inside the rect.
//!
//! The braille-canvas renderer + per-tick relax loop layer on top.

use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub struct AgentNode {
    pub agent_id: String,
    pub label: String,
    /// Position in the layout's [0.0, 1.0] × [0.0, 1.0] unit square.
    /// Fed into a Rect mapping at render time.
    pub x: f32,
    pub y: f32,
    /// Velocity carried across relaxation ticks so motion damps
    /// rather than thrashing on each frame.
    pub vx: f32,
    pub vy: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentEdge {
    pub from: String,
    pub to: String,
    /// Edge weight — shared-resource intensity. The renderer scales
    /// thickness by it so heavy contention reads visually.
    pub weight: f32,
}

/// One Fruchterman-Reingold iteration. `cooling` damps motion over
/// successive ticks (1.0 = full force, 0.0 = frozen). At the
/// existing 500ms refresh tick, ~0.85 is the sweet spot — fast enough
/// to settle, slow enough not to oscillate.
pub fn relax(nodes: &mut [AgentNode], edges: &[AgentEdge], cooling: f32) {
    let n = nodes.len() as f32;
    if n < 2.0 {
        return;
    }
    // Optimal edge length k ≈ √(area / n), with the unit-square
    // assumption area = 1.
    let k = (1.0_f32 / n).sqrt();
    let k_sq = k * k;

    // Repulsive force between every pair.
    let snapshot: Vec<(String, f32, f32)> = nodes
        .iter()
        .map(|a| (a.agent_id.clone(), a.x, a.y))
        .collect();
    for a in nodes.iter_mut() {
        let (mut fx, mut fy) = (0.0_f32, 0.0_f32);
        for (oid, ox, oy) in &snapshot {
            if *oid == a.agent_id {
                continue;
            }
            let dx = a.x - ox;
            let dy = a.y - oy;
            let dist_sq = (dx * dx + dy * dy).max(1e-4);
            let force = k_sq / dist_sq;
            fx += dx * force;
            fy += dy * force;
        }
        a.vx = (a.vx + fx) * cooling;
        a.vy = (a.vy + fy) * cooling;
    }

    // Attractive force along each edge.
    let lookup: HashMap<String, (f32, f32)> = nodes
        .iter()
        .map(|a| (a.agent_id.clone(), (a.x, a.y)))
        .collect();
    let mut deltas: HashMap<String, (f32, f32)> = HashMap::new();
    for e in edges {
        let Some(&(ax, ay)) = lookup.get(&e.from) else {
            continue;
        };
        let Some(&(bx, by)) = lookup.get(&e.to) else {
            continue;
        };
        let dx = ax - bx;
        let dy = ay - by;
        let dist = (dx * dx + dy * dy).sqrt().max(1e-4);
        let force = (dist * dist) / k * e.weight;
        let fx = dx * force / dist;
        let fy = dy * force / dist;
        deltas.entry(e.from.clone()).or_insert((0.0, 0.0)).0 -= fx;
        deltas.entry(e.from.clone()).or_insert((0.0, 0.0)).1 -= fy;
        deltas.entry(e.to.clone()).or_insert((0.0, 0.0)).0 += fx;
        deltas.entry(e.to.clone()).or_insert((0.0, 0.0)).1 += fy;
    }
    for a in nodes.iter_mut() {
        if let Some(&(dx, dy)) = deltas.get(&a.agent_id) {
            a.vx += dx * cooling;
            a.vy += dy * cooling;
        }
        // Step + clamp to the unit square so blob never escapes.
        a.x = (a.x + a.vx).clamp(0.0, 1.0);
        a.y = (a.y + a.vy).clamp(0.0, 1.0);
    }
}

/// Identify connected components — useful for the renderer to colour
/// each cluster distinctly. Returns a parent vector keyed by agent_id.
pub fn cluster_of(nodes: &[AgentNode], edges: &[AgentEdge]) -> HashMap<String, String> {
    // Union-find over agent_ids.
    let mut parent: HashMap<String, String> = nodes
        .iter()
        .map(|n| (n.agent_id.clone(), n.agent_id.clone()))
        .collect();
    fn find(parent: &mut HashMap<String, String>, x: String) -> String {
        let mut cur = x;
        while parent[&cur] != cur {
            let next = parent[&cur].clone();
            let grand = parent[&next].clone();
            parent.insert(cur.clone(), grand); // path compression
            cur = parent[&cur].clone();
        }
        cur
    }
    for e in edges {
        let ra = find(&mut parent, e.from.clone());
        let rb = find(&mut parent, e.to.clone());
        if ra != rb {
            parent.insert(ra, rb);
        }
    }
    // Final pass to flatten roots.
    let keys: Vec<String> = parent.keys().cloned().collect();
    for k in keys {
        let root = find(&mut parent, k.clone());
        parent.insert(k, root);
    }
    parent
}
