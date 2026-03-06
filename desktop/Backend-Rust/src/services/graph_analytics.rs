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
}
