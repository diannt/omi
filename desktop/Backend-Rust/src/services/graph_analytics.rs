//! Graph analytics for the knowledge graph: Louvain community detection,
//! centrality measures, and SQLite local-KG reader.
//!
//! Data sources (pluggable):
//!   - `load_local_kg()` → `local_kg_nodes` / `local_kg_edges` SQLite tables
//!     (schema: `RewindDatabase.swift:2130-2150`). No `memoryIds` column,
//!     so edge weight = multiplicity (count of distinct edges between a pair).
//!   - Firestore KG (via existing `routes/knowledge_graph.rs`) has `memory_ids`
//!     → edge weight = `memory_ids.len()` (real co-occurrence). Wired in P4.
//!
//! Algorithms are source-agnostic; they operate on `petgraph::UnGraph<String, f32>`.

use petgraph::graph::{NodeIndex, UnGraph};
use petgraph::visit::EdgeRef;
use rusqlite::{Connection, Result as SqlResult};
use std::collections::HashMap;
use std::path::Path;

// ===========================================================================
// Graph construction
// ===========================================================================

/// Simple node payload carried through the enrichment pipeline.
/// Mirrors the subset of `KnowledgeGraphNode` that matters for analytics.
#[derive(Debug, Clone)]
pub struct KgNode {
    pub id: String,
    pub label: String,
    pub node_type: String,
}

/// Simple edge payload. `weight` = co-occurrence count (or multiplicity).
#[derive(Debug, Clone)]
pub struct KgEdge {
    pub source_id: String,
    pub target_id: String,
    pub label: String,
    pub weight: f32,
}

/// Build an undirected petgraph from node/edge lists.
/// Returns the graph plus a `nodeId → NodeIndex` map for result translation.
/// Multiple edges between the same pair are collapsed and their weights summed.
pub fn build_graph(
    nodes: &[KgNode],
    edges: &[KgEdge],
) -> (UnGraph<String, f32>, HashMap<String, NodeIndex>) {
    let mut g = UnGraph::<String, f32>::new_undirected();
    let mut idx_map: HashMap<String, NodeIndex> = HashMap::with_capacity(nodes.len());

    for n in nodes {
        let ix = g.add_node(n.id.clone());
        idx_map.insert(n.id.clone(), ix);
    }

    // Collapse multi-edges: (min_ix, max_ix) → accumulated weight
    let mut edge_acc: HashMap<(NodeIndex, NodeIndex), f32> = HashMap::new();
    for e in edges {
        let (Some(&a), Some(&b)) = (idx_map.get(&e.source_id), idx_map.get(&e.target_id)) else {
            continue; // dangling edge, skip
        };
        if a == b {
            continue; // self-loop — Louvain modularity formula handles these awkwardly, drop them
        }
        let key = if a < b { (a, b) } else { (b, a) };
        *edge_acc.entry(key).or_insert(0.0) += e.weight.max(1.0);
    }

    for ((a, b), w) in edge_acc {
        g.add_edge(a, b, w);
    }

    (g, idx_map)
}

// ===========================================================================
// Louvain community detection
// ===========================================================================

/// Louvain method for modularity optimization.
///
/// Modularity Q = (1/2m) Σ_{i,j} [A_ij − k_i·k_j/(2m)] · δ(c_i, c_j)
///
/// Phase 1 (local move): for each node, compute ΔQ of moving to each neighbor's
/// community; greedily take the best positive move. Loop until stable.
/// Phase 2 (aggregate): contract communities into supernodes. Repeat.
///
/// Returns: `NodeIndex → cluster_id (u32)`, community count.
pub fn louvain_communities(g: &UnGraph<String, f32>) -> (HashMap<NodeIndex, u32>, usize) {
    let n = g.node_count();
    if n == 0 {
        return (HashMap::new(), 0);
    }

    // Initial state: every node in its own community.
    // We track a "community label" per original node throughout all aggregation levels.
    let mut node_community: Vec<usize> = (0..n).collect();

    // `current_nodes[i]` = set of original-node indices in supernode i
    // After aggregation these merge.
    let mut current_nodes: Vec<Vec<usize>> = (0..n).map(|i| vec![i]).collect();

    // Working graph at this level: adjacency as (neighbor_super_ix → weight)
    // Plus self-loop weight per supernode (from intra-community edges at prev level)
    let mut adj: Vec<HashMap<usize, f32>> = vec![HashMap::new(); n];
    let mut self_loop: Vec<f32> = vec![0.0; n];
    for e in g.edge_references() {
        let a = e.source().index();
        let b = e.target().index();
        let w = *e.weight();
        *adj[a].entry(b).or_insert(0.0) += w;
        *adj[b].entry(a).or_insert(0.0) += w;
    }

    loop {
        let level_n = adj.len();
        // k_i = weighted degree of supernode i = Σ_j A_ij + 2·self_loop_i
        let k: Vec<f32> = (0..level_n)
            .map(|i| adj[i].values().sum::<f32>() + 2.0 * self_loop[i])
            .collect();
        let two_m: f32 = k.iter().sum();
        if two_m == 0.0 {
            break;
        }

        // community[i] = current community of supernode i (at this level)
        let mut community: Vec<usize> = (0..level_n).collect();
        // sigma_tot[c] = Σ k_i for i in community c
        let mut sigma_tot: Vec<f32> = k.clone();
        // sigma_in[c] = Σ A_ij for i,j both in c (counting each pair once) + Σ self_loop_i
        let mut sigma_in: Vec<f32> = self_loop.clone();

        let mut improved = true;
        let mut any_move = false;
        while improved {
            improved = false;
            for i in 0..level_n {
                let c_i = community[i];
                let k_i = k[i];

                // Remove i from its community
                sigma_tot[c_i] -= k_i;
                // k_{i,c_i} = sum of edge weights from i to nodes currently in c_i (excluding i)
                let k_i_in_old: f32 = adj[i]
                    .iter()
                    .filter(|(&j, _)| community[j] == c_i)
                    .map(|(_, &w)| w)
                    .sum();
                sigma_in[c_i] -= k_i_in_old + self_loop[i];

                // Gather candidate communities from neighbors
                let mut k_i_in: HashMap<usize, f32> = HashMap::new();
                for (&j, &w) in &adj[i] {
                    *k_i_in.entry(community[j]).or_insert(0.0) += w;
                }
                // Always consider staying in the old community
                k_i_in.entry(c_i).or_insert(0.0);

                // ΔQ = [ k_{i,in} − Σ_tot·k_i / 2m ] / m
                // We compare the bracket term; dividing by m is monotone.
                let mut best_c = c_i;
                let mut best_gain = f32::NEG_INFINITY;
                for (&c, &k_in) in &k_i_in {
                    let gain = k_in - sigma_tot[c] * k_i / two_m;
                    if gain > best_gain {
                        best_gain = gain;
                        best_c = c;
                    }
                }

                // Reinsert i into best_c
                let k_i_in_new: f32 = *k_i_in.get(&best_c).unwrap_or(&0.0);
                sigma_tot[best_c] += k_i;
                sigma_in[best_c] += k_i_in_new + self_loop[i];
                community[i] = best_c;

                if best_c != c_i {
                    improved = true;
                    any_move = true;
                }
            }
        }

        if !any_move {
            break;
        }

        // Aggregate: renumber communities → [0, num_communities)
        let mut remap: HashMap<usize, usize> = HashMap::new();
        for &c in &community {
            let next = remap.len();
            remap.entry(c).or_insert(next);
        }
        let num_c = remap.len();

        // Propagate to original nodes
        let mut new_current: Vec<Vec<usize>> = vec![Vec::new(); num_c];
        for (super_ix, originals) in current_nodes.iter().enumerate() {
            let new_c = remap[&community[super_ix]];
            for &orig in originals {
                node_community[orig] = new_c;
                new_current[new_c].push(orig);
            }
        }
        current_nodes = new_current;

        // Build aggregated adjacency
        let mut new_adj: Vec<HashMap<usize, f32>> = vec![HashMap::new(); num_c];
        let mut new_self: Vec<f32> = vec![0.0; num_c];
        for i in 0..level_n {
            let ci = remap[&community[i]];
            new_self[ci] += self_loop[i];
            for (&j, &w) in &adj[i] {
                let cj = remap[&community[j]];
                if ci == cj {
                    // Intra-community edge → becomes self-loop weight.
                    // Each edge seen twice (i→j and j→i), so divide by 2.
                    new_self[ci] += w / 2.0;
                } else {
                    *new_adj[ci].entry(cj).or_insert(0.0) += w;
                }
            }
        }
        adj = new_adj;
        self_loop = new_self;

        if num_c == level_n {
            break; // no collapse happened despite a move → done
        }
    }

    // Renumber final communities densely
    let mut final_remap: HashMap<usize, u32> = HashMap::new();
    for &c in &node_community {
        let next = final_remap.len() as u32;
        final_remap.entry(c).or_insert(next);
    }
    let mut result: HashMap<NodeIndex, u32> = HashMap::with_capacity(n);
    for (orig_ix, &c) in node_community.iter().enumerate() {
        result.insert(NodeIndex::new(orig_ix), final_remap[&c]);
    }
    let k = final_remap.len();
    (result, k)
}

/// Compute modularity Q for a given partition. Useful for assertions.
pub fn modularity(g: &UnGraph<String, f32>, communities: &HashMap<NodeIndex, u32>) -> f32 {
    let two_m: f32 = g.edge_references().map(|e| *e.weight()).sum::<f32>() * 2.0;
    if two_m == 0.0 {
        return 0.0;
    }
    let k: HashMap<NodeIndex, f32> = g
        .node_indices()
        .map(|n| (n, g.edges(n).map(|e| *e.weight()).sum()))
        .collect();

    let mut q = 0.0f32;
    // Q = (1/2m) Σ_{ij} (A_ij − k_i k_j / 2m) δ(c_i, c_j)
    // = Σ_c [ Σ_in_c / m − (Σ_tot_c / 2m)^2 ]
    let mut sigma_in: HashMap<u32, f32> = HashMap::new();
    let mut sigma_tot: HashMap<u32, f32> = HashMap::new();
    for (ix, &c) in communities {
        *sigma_tot.entry(c).or_insert(0.0) += k[ix];
    }
    for e in g.edge_references() {
        let ca = communities[&e.source()];
        let cb = communities[&e.target()];
        if ca == cb {
            *sigma_in.entry(ca).or_insert(0.0) += *e.weight();
        }
    }
    for (&c, &s_in) in &sigma_in {
        let s_tot = sigma_tot[&c];
        q += s_in / (two_m / 2.0) - (s_tot / two_m).powi(2);
    }
    // Communities with zero internal edges still subtract their (Σ_tot/2m)^2
    for (&c, &s_tot) in &sigma_tot {
        if !sigma_in.contains_key(&c) {
            q -= (s_tot / two_m).powi(2);
        }
    }
    q
}

// ===========================================================================
// Cluster labeling
// ===========================================================================

/// Pick a human-readable label for each cluster: the highest-degree node's label.
/// Returns `cluster_id → (label, node_count, dominant_type)`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ClusterInfo {
    pub id: u32,
    pub label: String,
    pub node_count: usize,
    pub dominant_type: String,
}

pub fn label_clusters(
    g: &UnGraph<String, f32>,
    idx_map: &HashMap<String, NodeIndex>,
    nodes: &[KgNode],
    communities: &HashMap<NodeIndex, u32>,
) -> Vec<ClusterInfo> {
    // Reverse lookup: NodeIndex → KgNode position
    let ix_to_node: HashMap<NodeIndex, usize> = idx_map
        .iter()
        .filter_map(|(id, &ix)| nodes.iter().position(|n| &n.id == id).map(|p| (ix, p)))
        .collect();

    // Group by cluster
    let mut clusters: HashMap<u32, Vec<NodeIndex>> = HashMap::new();
    for (&ix, &c) in communities {
        clusters.entry(c).or_default().push(ix);
    }

    let mut out: Vec<ClusterInfo> = clusters
        .into_iter()
        .map(|(cid, members)| {
            // Hub = highest weighted degree
            let hub = members
                .iter()
                .copied()
                .max_by(|&a, &b| {
                    let da: f32 = g.edges(a).map(|e| *e.weight()).sum();
                    let db: f32 = g.edges(b).map(|e| *e.weight()).sum();
                    da.partial_cmp(&db).unwrap()
                })
                .unwrap();
            let hub_node = &nodes[ix_to_node[&hub]];

            // Dominant node_type = mode
            let mut type_count: HashMap<&str, usize> = HashMap::new();
            for &m in &members {
                if let Some(&p) = ix_to_node.get(&m) {
                    *type_count.entry(nodes[p].node_type.as_str()).or_insert(0) += 1;
                }
            }
            let dominant = type_count
                .into_iter()
                .max_by_key(|&(_, c)| c)
                .map(|(t, _)| t.to_string())
                .unwrap_or_else(|| "concept".into());

            ClusterInfo {
                id: cid,
                label: hub_node.label.clone(),
                node_count: members.len(),
                dominant_type: dominant,
            }
        })
        .collect();
    out.sort_by_key(|c| c.id);
    out
}

// ===========================================================================
// SQLite reader for local KG tables
// ===========================================================================

/// Load from `local_kg_nodes` + `local_kg_edges` tables (schema from
/// `RewindDatabase.swift:2130-2150`). Edge weight = multiplicity between
/// the same (source, target) pair, since there's no `memoryIds` column.
pub fn load_local_kg(db_path: &Path) -> SqlResult<(Vec<KgNode>, Vec<KgEdge>)> {
    let conn = Connection::open(db_path)?;

    let mut node_stmt =
        conn.prepare("SELECT nodeId, label, nodeType FROM local_kg_nodes")?;
    let nodes: Vec<KgNode> = node_stmt
        .query_map([], |r| {
            Ok(KgNode {
                id: r.get(0)?,
                label: r.get(1)?,
                node_type: r.get(2)?,
            })
        })?
        .collect::<SqlResult<_>>()?;

    // Count multiplicity: GROUP BY on the unordered pair
    let mut edge_stmt = conn.prepare(
        "SELECT sourceNodeId, targetNodeId, MIN(label), COUNT(*) \
         FROM local_kg_edges \
         GROUP BY \
           CASE WHEN sourceNodeId < targetNodeId THEN sourceNodeId ELSE targetNodeId END, \
           CASE WHEN sourceNodeId < targetNodeId THEN targetNodeId ELSE sourceNodeId END",
    )?;
    let edges: Vec<KgEdge> = edge_stmt
        .query_map([], |r| {
            Ok(KgEdge {
                source_id: r.get(0)?,
                target_id: r.get(1)?,
                label: r.get(2)?,
                weight: r.get::<_, i64>(3)? as f32,
            })
        })?
        .collect::<SqlResult<_>>()?;

    Ok((nodes, edges))
}

// ===========================================================================
// Centrality measures
// ===========================================================================

/// Normalized degree centrality: `deg(v) / (n-1)`.
/// For isolated nodes or n<2 returns 0.
pub fn degree_centrality(g: &UnGraph<String, f32>) -> HashMap<NodeIndex, f64> {
    let n = g.node_count();
    if n < 2 {
        return g.node_indices().map(|ix| (ix, 0.0)).collect();
    }
    let denom = (n - 1) as f64;
    g.node_indices()
        .map(|ix| (ix, g.edges(ix).count() as f64 / denom))
        .collect()
}

/// Closeness centrality with Wasserman-Faust correction for disconnected graphs:
/// `C(v) = (r-1)/(n-1) · (r-1)/Σd(v,u)` where r = number of reachable nodes.
/// Unweighted BFS (edge weights ignored — KG edges are co-occurrence counts,
/// not distances).
pub fn closeness_centrality(g: &UnGraph<String, f32>) -> HashMap<NodeIndex, f64> {
    let n = g.node_count();
    let mut out = HashMap::with_capacity(n);
    if n < 2 {
        for ix in g.node_indices() {
            out.insert(ix, 0.0);
        }
        return out;
    }
    let n_minus_1 = (n - 1) as f64;

    for src in g.node_indices() {
        // BFS
        let mut dist: HashMap<NodeIndex, u32> = HashMap::new();
        let mut queue = std::collections::VecDeque::new();
        dist.insert(src, 0);
        queue.push_back(src);
        while let Some(u) = queue.pop_front() {
            let d = dist[&u];
            for v in g.neighbors(u) {
                if !dist.contains_key(&v) {
                    dist.insert(v, d + 1);
                    queue.push_back(v);
                }
            }
        }
        let reach = dist.len(); // includes src itself
        let sum_d: u64 = dist.values().map(|&d| d as u64).sum();
        if reach < 2 || sum_d == 0 {
            out.insert(src, 0.0);
        } else {
            let r_minus_1 = (reach - 1) as f64;
            // Wasserman-Faust: scale by fraction reachable
            out.insert(src, (r_minus_1 / n_minus_1) * (r_minus_1 / sum_d as f64));
        }
    }
    out
}

/// Brandes' betweenness centrality, O(V·E) for unweighted graphs.
/// Normalized for undirected: divide by `(n-1)(n-2)/2` so values are in [0,1].
pub fn betweenness_centrality(g: &UnGraph<String, f32>) -> HashMap<NodeIndex, f64> {
    let n = g.node_count();
    let mut bc: HashMap<NodeIndex, f64> = g.node_indices().map(|ix| (ix, 0.0)).collect();
    if n < 3 {
        return bc;
    }

    for s in g.node_indices() {
        // Single-source shortest-path BFS
        let mut stack: Vec<NodeIndex> = Vec::new();
        let mut pred: HashMap<NodeIndex, Vec<NodeIndex>> = HashMap::new();
        let mut sigma: HashMap<NodeIndex, f64> = HashMap::new();
        let mut dist: HashMap<NodeIndex, i32> = HashMap::new();

        for v in g.node_indices() {
            pred.insert(v, Vec::new());
            sigma.insert(v, 0.0);
            dist.insert(v, -1);
        }
        sigma.insert(s, 1.0);
        dist.insert(s, 0);

        let mut queue = std::collections::VecDeque::new();
        queue.push_back(s);
        while let Some(v) = queue.pop_front() {
            stack.push(v);
            let dv = dist[&v];
            for w in g.neighbors(v) {
                if dist[&w] < 0 {
                    dist.insert(w, dv + 1);
                    queue.push_back(w);
                }
                if dist[&w] == dv + 1 {
                    let sv = sigma[&v];
                    *sigma.get_mut(&w).unwrap() += sv;
                    pred.get_mut(&w).unwrap().push(v);
                }
            }
        }

        // Accumulate dependencies in reverse BFS order
        let mut delta: HashMap<NodeIndex, f64> = g.node_indices().map(|ix| (ix, 0.0)).collect();
        while let Some(w) = stack.pop() {
            let coeff = (1.0 + delta[&w]) / sigma[&w];
            for &v in &pred[&w] {
                *delta.get_mut(&v).unwrap() += sigma[&v] * coeff;
            }
            if w != s {
                *bc.get_mut(&w).unwrap() += delta[&w];
            }
        }
    }

    // Normalize: undirected counts each pair twice (once per endpoint as source)
    // Standard normalization: divide by (n-1)(n-2) to map to [0,1]
    let norm = ((n - 1) * (n - 2)) as f64;
    for v in bc.values_mut() {
        *v /= norm;
    }
    bc
}

// ===========================================================================
// Enrichment orchestrator
// ===========================================================================

/// Full enriched node — base identity plus analytics scores.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EnrichedNode {
    pub id: String,
    pub label: String,
    pub node_type: String,
    pub cluster_id: u32,
    pub degree_centrality: f64,
    pub betweenness_centrality: f64,
    pub closeness_centrality: f64,
}

/// Enriched edge — collapsed undirected edge with accumulated weight.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EnrichedEdge {
    pub source_id: String,
    pub target_id: String,
    pub weight: f32,
}

/// Output of the full enrichment pipeline.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EnrichedGraph {
    pub nodes: Vec<EnrichedNode>,
    pub edges: Vec<EnrichedEdge>,
    pub clusters: Vec<ClusterInfo>,
    pub modularity: f32,
}

/// Run the full pipeline: build graph → Louvain → centralities → label clusters.
/// One-stop entry point for the `/enriched` API route.
pub fn enrich_graph(nodes: &[KgNode], edges: &[KgEdge]) -> EnrichedGraph {
    let (g, idx_map) = build_graph(nodes, edges);

    if g.node_count() == 0 {
        return EnrichedGraph {
            nodes: vec![],
            edges: vec![],
            clusters: vec![],
            modularity: 0.0,
        };
    }

    let (communities, _k) = louvain_communities(&g);
    let q = modularity(&g, &communities);
    let deg = degree_centrality(&g);
    let bet = betweenness_centrality(&g);
    let clo = closeness_centrality(&g);
    let clusters = label_clusters(&g, &idx_map, nodes, &communities);

    // Reverse map for node output
    let ix_to_id: HashMap<NodeIndex, &str> =
        idx_map.iter().map(|(id, &ix)| (ix, id.as_str())).collect();

    let mut enriched_nodes: Vec<EnrichedNode> = nodes
        .iter()
        .filter_map(|n| {
            let ix = *idx_map.get(&n.id)?;
            Some(EnrichedNode {
                id: n.id.clone(),
                label: n.label.clone(),
                node_type: n.node_type.clone(),
                cluster_id: *communities.get(&ix).unwrap_or(&0),
                degree_centrality: deg.get(&ix).copied().unwrap_or(0.0),
                betweenness_centrality: bet.get(&ix).copied().unwrap_or(0.0),
                closeness_centrality: clo.get(&ix).copied().unwrap_or(0.0),
            })
        })
        .collect();
    // Stable ordering for reproducible API responses
    enriched_nodes.sort_by(|a, b| a.id.cmp(&b.id));

    let mut enriched_edges: Vec<EnrichedEdge> = g
        .edge_references()
        .map(|e| {
            let a = ix_to_id[&e.source()];
            let b = ix_to_id[&e.target()];
            // Canonicalize so edges are stable regardless of petgraph internals
            let (s, t) = if a <= b { (a, b) } else { (b, a) };
            EnrichedEdge {
                source_id: s.to_string(),
                target_id: t.to_string(),
                weight: *e.weight(),
            }
        })
        .collect();
    enriched_edges.sort_by(|a, b| (a.source_id.as_str(), a.target_id.as_str()).cmp(&(b.source_id.as_str(), b.target_id.as_str())));

    EnrichedGraph {
        nodes: enriched_nodes,
        edges: enriched_edges,
        clusters,
        modularity: q,
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_nodes(ids: &[&str]) -> Vec<KgNode> {
        ids.iter()
            .map(|&id| KgNode {
                id: id.to_string(),
                label: id.to_string(),
                node_type: "concept".to_string(),
            })
            .collect()
    }

    fn make_edge(a: &str, b: &str) -> KgEdge {
        KgEdge {
            source_id: a.to_string(),
            target_id: b.to_string(),
            label: "rel".to_string(),
            weight: 1.0,
        }
    }

    /// Two 4-cliques connected by a single bridge edge.
    /// Louvain must find exactly 2 communities.
    #[test]
    fn two_clique_split() {
        let nodes = make_nodes(&["a0", "a1", "a2", "a3", "b0", "b1", "b2", "b3"]);
        let mut edges = Vec::new();
        // Clique A
        for i in 0..4 {
            for j in (i + 1)..4 {
                edges.push(make_edge(&format!("a{i}"), &format!("a{j}")));
            }
        }
        // Clique B
        for i in 0..4 {
            for j in (i + 1)..4 {
                edges.push(make_edge(&format!("b{i}"), &format!("b{j}")));
            }
        }
        // Bridge
        edges.push(make_edge("a0", "b0"));

        let (g, idx) = build_graph(&nodes, &edges);
        assert_eq!(g.node_count(), 8);
        assert_eq!(g.edge_count(), 13); // 6+6+1

        let (comm, k) = louvain_communities(&g);
        assert_eq!(k, 2, "two cliques → two communities");

        // All a* nodes share one community, all b* the other
        let ca = comm[&idx["a0"]];
        let cb = comm[&idx["b0"]];
        assert_ne!(ca, cb);
        for n in ["a1", "a2", "a3"] {
            assert_eq!(comm[&idx[n]], ca, "{} should be in clique A", n);
        }
        for n in ["b1", "b2", "b3"] {
            assert_eq!(comm[&idx[n]], cb, "{} should be in clique B", n);
        }

        // Modularity should be positive (good partition)
        let q = modularity(&g, &comm);
        assert!(q > 0.3, "modularity {} too low for clear clique structure", q);
    }

    /// Three 5-node cliques + 2 bridge edges → exactly 3 communities.
    #[test]
    fn three_clique_communities() {
        let prefixes = ["x", "y", "z"];
        let nodes: Vec<KgNode> = prefixes
            .iter()
            .flat_map(|p| (0..5).map(move |i| format!("{p}{i}")))
            .map(|id| KgNode {
                id: id.clone(),
                label: id,
                node_type: "concept".into(),
            })
            .collect();

        let mut edges = Vec::new();
        for p in &prefixes {
            for i in 0..5 {
                for j in (i + 1)..5 {
                    edges.push(make_edge(&format!("{p}{i}"), &format!("{p}{j}")));
                }
            }
        }
        // Bridges: x0-y0, y0-z0
        edges.push(make_edge("x0", "y0"));
        edges.push(make_edge("y0", "z0"));

        let (g, idx) = build_graph(&nodes, &edges);
        let (comm, k) = louvain_communities(&g);
        assert_eq!(k, 3);

        // Each prefix → distinct community
        let cx = comm[&idx["x0"]];
        let cy = comm[&idx["y0"]];
        let cz = comm[&idx["z0"]];
        assert_eq!(
            [cx, cy, cz].iter().collect::<std::collections::HashSet<_>>().len(),
            3
        );
        for i in 0..5 {
            assert_eq!(comm[&idx[&format!("x{i}")]], cx);
            assert_eq!(comm[&idx[&format!("y{i}")]], cy);
            assert_eq!(comm[&idx[&format!("z{i}")]], cz);
        }

        let q = modularity(&g, &comm);
        assert!(q > 0.5, "modularity {} too low for 3 tight cliques", q);
    }

    /// Single connected component where all nodes are equivalent (cycle) →
    /// Louvain may find 1 or split arbitrarily, but modularity must be ≥ 0.
    #[test]
    fn ring_modularity_nonnegative() {
        let n = 10;
        let ids: Vec<String> = (0..n).map(|i| format!("n{i}")).collect();
        let nodes = make_nodes(&ids.iter().map(|s| s.as_str()).collect::<Vec<_>>());
        let edges: Vec<KgEdge> = (0..n)
            .map(|i| make_edge(&ids[i], &ids[(i + 1) % n]))
            .collect();

        let (g, _) = build_graph(&nodes, &edges);
        let (comm, k) = louvain_communities(&g);
        assert!(k >= 1 && k <= n);
        let q = modularity(&g, &comm);
        assert!(q >= 0.0, "Louvain should never produce negative modularity, got {}", q);
    }

    /// Multi-edges between the same pair should collapse into weight.
    #[test]
    fn edge_weight_from_multiplicity() {
        let nodes = make_nodes(&["a", "b", "c"]);
        let edges = vec![
            make_edge("a", "b"),
            make_edge("a", "b"),
            make_edge("b", "a"), // reversed, still collapses
            make_edge("b", "c"),
        ];
        let (g, idx) = build_graph(&nodes, &edges);
        assert_eq!(g.edge_count(), 2);

        let ab_w = g
            .edges_connecting(idx["a"], idx["b"])
            .next()
            .map(|e| *e.weight())
            .unwrap();
        assert_eq!(ab_w, 3.0);
        let bc_w = g
            .edges_connecting(idx["b"], idx["c"])
            .next()
            .map(|e| *e.weight())
            .unwrap();
        assert_eq!(bc_w, 1.0);
    }

    /// Cluster labeling picks the hub node's label.
    #[test]
    fn cluster_labels_pick_hub() {
        // Star topology: hub "alice" + 4 leaves
        let nodes = vec![
            KgNode {
                id: "alice".into(),
                label: "Alice".into(),
                node_type: "person".into(),
            },
            KgNode {
                id: "bob".into(),
                label: "Bob".into(),
                node_type: "person".into(),
            },
            KgNode {
                id: "carol".into(),
                label: "Carol".into(),
                node_type: "person".into(),
            },
            KgNode {
                id: "dave".into(),
                label: "Dave".into(),
                node_type: "person".into(),
            },
            KgNode {
                id: "proj".into(),
                label: "Project X".into(),
                node_type: "concept".into(),
            },
        ];
        let edges = vec![
            make_edge("alice", "bob"),
            make_edge("alice", "carol"),
            make_edge("alice", "dave"),
            make_edge("alice", "proj"),
        ];
        let (g, idx) = build_graph(&nodes, &edges);
        let (comm, _) = louvain_communities(&g);
        let labels = label_clusters(&g, &idx, &nodes, &comm);

        // Star is one community; label should be the hub (highest degree)
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].label, "Alice");
        assert_eq!(labels[0].node_count, 5);
        assert_eq!(labels[0].dominant_type, "person"); // 4 person > 1 concept
    }

    /// SQLite roundtrip: write local KG tables, load, build graph, cluster.
    #[test]
    fn sqlite_local_kg_roundtrip() {
        let db_path = std::env::temp_dir().join(format!(
            "kg_test_{}.sqlite",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE local_kg_nodes (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                nodeId TEXT NOT NULL UNIQUE,
                label TEXT NOT NULL,
                nodeType TEXT NOT NULL,
                aliasesJson TEXT,
                sourceFileIds TEXT,
                createdAt TEXT NOT NULL,
                updatedAt TEXT NOT NULL
            );
            CREATE TABLE local_kg_edges (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                edgeId TEXT NOT NULL UNIQUE,
                sourceNodeId TEXT NOT NULL,
                targetNodeId TEXT NOT NULL,
                label TEXT NOT NULL,
                createdAt TEXT NOT NULL
            );",
        )
        .unwrap();

        let now = "2024-01-01 00:00:00.000";
        // Two cliques
        for p in ["a", "b"] {
            for i in 0..4 {
                conn.execute(
                    "INSERT INTO local_kg_nodes (nodeId, label, nodeType, createdAt, updatedAt) VALUES (?1, ?2, 'concept', ?3, ?3)",
                    rusqlite::params![format!("{p}{i}"), format!("{}{}", p.to_uppercase(), i), now],
                ).unwrap();
            }
            let mut eid = 0;
            for i in 0..4 {
                for j in (i + 1)..4 {
                    conn.execute(
                        "INSERT INTO local_kg_edges (edgeId, sourceNodeId, targetNodeId, label, createdAt) VALUES (?1, ?2, ?3, 'rel', ?4)",
                        rusqlite::params![format!("{p}-{eid}"), format!("{p}{i}"), format!("{p}{j}"), now],
                    ).unwrap();
                    eid += 1;
                }
            }
        }
        // Bridge + duplicate edge (weight 2 after collapse)
        conn.execute(
            "INSERT INTO local_kg_edges (edgeId, sourceNodeId, targetNodeId, label, createdAt) VALUES ('bridge1', 'a0', 'b0', 'knows', ?1)",
            [now],
        ).unwrap();
        conn.execute(
            "INSERT INTO local_kg_edges (edgeId, sourceNodeId, targetNodeId, label, createdAt) VALUES ('bridge2', 'b0', 'a0', 'met', ?1)",
            [now],
        ).unwrap();

        // Load
        let (nodes, edges) = load_local_kg(&db_path).unwrap();
        assert_eq!(nodes.len(), 8);
        // 6+6 clique edges + 1 collapsed bridge pair = 13
        assert_eq!(edges.len(), 13);
        // Bridge weight should be 2 (from two rows)
        let bridge = edges
            .iter()
            .find(|e| (e.source_id == "a0" && e.target_id == "b0") || (e.source_id == "b0" && e.target_id == "a0"))
            .unwrap();
        assert_eq!(bridge.weight, 2.0);

        // Build + cluster
        let (g, _) = build_graph(&nodes, &edges);
        let (comm, k) = louvain_communities(&g);
        // Bridge weight=2 is still weak vs 6 internal clique edges → still 2 clusters
        assert_eq!(k, 2);
        let q = modularity(&g, &comm);
        assert!(q > 0.25, "modularity {} with weight-2 bridge", q);

        std::fs::remove_file(&db_path).ok();
    }

    /// Empty graph doesn't panic.
    #[test]
    fn empty_graph() {
        let (g, _) = build_graph(&[], &[]);
        let (comm, k) = louvain_communities(&g);
        assert_eq!(k, 0);
        assert!(comm.is_empty());
    }

    /// Dangling edges (referencing unknown nodes) are dropped.
    #[test]
    fn dangling_edges_dropped() {
        let nodes = make_nodes(&["a", "b"]);
        let edges = vec![
            make_edge("a", "b"),
            make_edge("a", "ghost"), // ghost not in nodes
        ];
        let (g, _) = build_graph(&nodes, &edges);
        assert_eq!(g.edge_count(), 1);
    }

    // -----------------------------------------------------------------------
    // Centrality tests
    // -----------------------------------------------------------------------

    /// Star K₁,₄: hub connected to 4 leaves.
    /// Hub: degree=4/4=1.0, betweenness=1.0 (all leaf-pairs go through hub),
    ///      closeness=4/(1+1+1+1)=1.0
    /// Leaf: degree=1/4=0.25, betweenness=0.0, closeness < hub
    #[test]
    fn star_centrality() {
        let nodes = make_nodes(&["hub", "l1", "l2", "l3", "l4"]);
        let edges = vec![
            make_edge("hub", "l1"),
            make_edge("hub", "l2"),
            make_edge("hub", "l3"),
            make_edge("hub", "l4"),
        ];
        let (g, idx) = build_graph(&nodes, &edges);
        let deg = degree_centrality(&g);
        let bet = betweenness_centrality(&g);
        let clo = closeness_centrality(&g);

        let hub_ix = idx["hub"];
        let l1_ix = idx["l1"];

        // Degree
        assert!((deg[&hub_ix] - 1.0).abs() < 1e-9);
        assert!((deg[&l1_ix] - 0.25).abs() < 1e-9);

        // Betweenness: hub is on all C(4,2)=6 shortest paths between leaves.
        // Normalized: 6 / ((n-1)(n-2)/2) = 6/6 = 1.0, but Brandes counts each
        // pair twice (undirected → both directions), so raw=12, norm=(n-1)(n-2)=12.
        assert!((bet[&hub_ix] - 1.0).abs() < 1e-9, "hub betweenness = {}", bet[&hub_ix]);
        assert!((bet[&l1_ix]).abs() < 1e-9);

        // Closeness: hub reaches all 4 at distance 1 → sum=4 → C=4/4=1.0
        assert!((clo[&hub_ix] - 1.0).abs() < 1e-9);
        // Leaf reaches hub at 1, other 3 leaves at 2 → sum=7 → C=(4/4)·(4/7)=4/7≈0.571
        assert!((clo[&l1_ix] - 4.0 / 7.0).abs() < 1e-6);
    }

    /// Path P₅: a—b—c—d—e. Middle node c has highest betweenness.
    #[test]
    fn path_centrality() {
        let nodes = make_nodes(&["a", "b", "c", "d", "e"]);
        let edges = vec![
            make_edge("a", "b"),
            make_edge("b", "c"),
            make_edge("c", "d"),
            make_edge("d", "e"),
        ];
        let (g, idx) = build_graph(&nodes, &edges);
        let bet = betweenness_centrality(&g);

        let bc_a = bet[&idx["a"]];
        let bc_b = bet[&idx["b"]];
        let bc_c = bet[&idx["c"]];
        let bc_d = bet[&idx["d"]];
        let bc_e = bet[&idx["e"]];

        // Endpoints have 0 betweenness
        assert!(bc_a.abs() < 1e-9);
        assert!(bc_e.abs() < 1e-9);
        // Middle is strictly highest
        assert!(bc_c > bc_b);
        assert!(bc_c > bc_d);
        // Symmetric
        assert!((bc_b - bc_d).abs() < 1e-9);

        // Exact values for P₅:
        // b is on paths: a-c (a-b-c), a-d (a-b-c-d), a-e (a-b-c-d-e) → 3 pairs × 2 dirs = 6 raw
        // c is on paths: a-d, a-e, b-d, b-e → 4 pairs × 2 dirs = 8 raw
        // Normalized by (n-1)(n-2) = 12
        assert!((bc_b - 6.0 / 12.0).abs() < 1e-9, "bc_b = {}", bc_b);
        assert!((bc_c - 8.0 / 12.0).abs() < 1e-9, "bc_c = {}", bc_c);
    }

    /// Two disconnected K₃ triangles. Closeness uses Wasserman-Faust
    /// so each node's closeness reflects its own component, scaled by
    /// fraction of total graph reachable.
    #[test]
    fn disconnected_closeness() {
        let nodes = make_nodes(&["a", "b", "c", "x", "y", "z"]);
        let edges = vec![
            make_edge("a", "b"),
            make_edge("b", "c"),
            make_edge("a", "c"),
            make_edge("x", "y"),
            make_edge("y", "z"),
            make_edge("x", "z"),
        ];
        let (g, idx) = build_graph(&nodes, &edges);
        let clo = closeness_centrality(&g);

        // In a K₃: each node reaches 2 others at distance 1 → sum_d=2, r=3
        // WF: (r-1)/(n-1) · (r-1)/sum_d = (2/5)·(2/2) = 0.4
        for id in &["a", "b", "c", "x", "y", "z"] {
            let c = clo[&idx[*id]];
            assert!((c - 0.4).abs() < 1e-9, "{} closeness = {}", id, c);
        }

        // Betweenness: in a triangle, all shortest paths are direct edges,
        // so no node is an intermediate. All betweenness = 0.
        let bet = betweenness_centrality(&g);
        for (_, &b) in &bet {
            assert!(b.abs() < 1e-9);
        }
    }

    /// enrich_graph produces a coherent response: correct node/edge counts,
    /// every node has a cluster_id, all centralities in [0,1].
    #[test]
    fn enrich_two_clique() {
        let nodes = make_nodes(&["a0", "a1", "a2", "a3", "b0", "b1", "b2", "b3"]);
        let mut edges = Vec::new();
        for p in &["a", "b"] {
            for i in 0..4 {
                for j in (i + 1)..4 {
                    edges.push(make_edge(&format!("{p}{i}"), &format!("{p}{j}")));
                }
            }
        }
        edges.push(make_edge("a0", "b0")); // bridge

        let eg = enrich_graph(&nodes, &edges);
        assert_eq!(eg.nodes.len(), 8);
        assert_eq!(eg.edges.len(), 13); // 6+6 clique edges + 1 bridge
        assert_eq!(eg.clusters.len(), 2);
        assert!(eg.modularity > 0.3);

        for n in &eg.nodes {
            assert!(n.degree_centrality >= 0.0 && n.degree_centrality <= 1.0);
            assert!(n.betweenness_centrality >= 0.0 && n.betweenness_centrality <= 1.0);
            assert!(n.closeness_centrality >= 0.0 && n.closeness_centrality <= 1.0);
        }

        // Bridge endpoints a0,b0 should have highest betweenness
        let bc: HashMap<&str, f64> = eg
            .nodes
            .iter()
            .map(|n| (n.id.as_str(), n.betweenness_centrality))
            .collect();
        assert!(bc["a0"] > bc["a1"]);
        assert!(bc["b0"] > bc["b1"]);
    }
}
