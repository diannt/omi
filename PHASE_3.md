# Phase 3 — Louvain Community Detection + Local KG Reader

**Start:** 2026-03-05 19:38:30

## Goal
Build `graph_analytics.rs` implementing Louvain modularity optimization on `petgraph::UnGraph`. Read knowledge-graph nodes/edges from local SQLite tables `local_kg_nodes`/`local_kg_edges` (RewindDatabase.swift:2130-2150). Test on synthetic topologies with known community structure.

## Local KG Schema (RewindDatabase.swift migration `createLocalKnowledgeGraph`)
```
local_kg_nodes: id PK, nodeId UNIQUE, label, nodeType, aliasesJson, sourceFileIds, createdAt, updatedAt
local_kg_edges: id PK, edgeId UNIQUE, sourceNodeId, targetNodeId, label, createdAt
```

**Schema limitation noted:** `local_kg_edges` has NO `memoryIds` column — the local KG is file-indexed, not memory-indexed. Edge weight = count of edges between node pair (multiple relationship types → stronger bond). Firestore KG (P4 API source) DOES have `memory_ids` → real co-occurrence.

## Louvain Algorithm
Two-phase iterative:
1. **Local move**: for each node, move to the neighbor's community that yields the largest modularity gain (ΔQ). Repeat until no positive move exists.
2. **Aggregation**: build a new graph where each community becomes a supernode; edge weights sum. Goto 1.

Repeat until modularity stops improving.

## Files Created
- `desktop/Backend-Rust/src/services/graph_analytics.rs` — Louvain + cluster labeling + SQLite reader

## Files Modified
- `desktop/Backend-Rust/src/services/mod.rs` — register `graph_analytics`

## Roadblocks
- **Schema discovery:** `local_kg_edges` has no `memoryIds` column — it's the FILE-indexing KG (`sourceFileIds` on nodes), not the memory-based KG. `LocalKGEdgeRecord.toKnowledgeGraphEdge()` hard-codes `memoryIds: []`. Mitigated by computing edge weight from row multiplicity: `COUNT(*) GROUP BY min(src,tgt), max(src,tgt)` — two extractions linking the same entities = weight 2.
- The Firestore-based KG (Rust backend's `KnowledgeGraphEdge.memory_ids: Vec<String>`) DOES track co-occurrence properly. P4's `/enriched` route will prefer Firestore → real co-occurrence weights; local SQLite is the offline fallback.

## Implementation Notes
- Louvain ΔQ formula: `gain = k_{i,in} − Σ_tot·k_i / 2m` (bracket term only — dividing by m is monotone). Sweep nodes until no improving move exists, then aggregate.
- Self-loops at aggregation: intra-community edges become supernode self-loops. Each edge counted twice in symmetric adjacency → divide by 2 on aggregation.
- `build_graph` drops self-loops and dangling edges up-front — modularity formula handles them awkwardly and real KG shouldn't have them.

## Test Output

### graph_analytics unit tests (8/8 pass, debug build)
```
running 8 tests
test services::graph_analytics::tests::dangling_edges_dropped ... ok
test services::graph_analytics::tests::edge_weight_from_multiplicity ... ok
test services::graph_analytics::tests::cluster_labels_pick_hub ... ok
test services::graph_analytics::tests::ring_modularity_nonnegative ... ok
test services::graph_analytics::tests::empty_graph ... ok
test services::graph_analytics::tests::two_clique_split ... ok
test services::graph_analytics::tests::three_clique_communities ... ok
test services::graph_analytics::tests::sqlite_local_kg_roundtrip ... ok

test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 28 filtered out; finished in 0.48s
```

### Test topology assertions
| Test | Topology | Assertion | Result |
|---|---|---|---|
| `two_clique_split` | 2× K₄ + 1 bridge | exactly 2 communities, Q > 0.3 | ✓ |
| `three_clique_communities` | 3× K₅ + 2 bridges | exactly 3 communities, Q > 0.5 | ✓ |
| `ring_modularity_nonnegative` | C₁₀ | Q ≥ 0 (Louvain never worsens trivial partition) | ✓ |
| `edge_weight_from_multiplicity` | 3 duplicate + 1 reverse edge → weight 3 | collapse correct | ✓ |
| `cluster_labels_pick_hub` | star K₁,₄ (Alice hub) | label="Alice", type="person" (4>1) | ✓ |
| `sqlite_local_kg_roundtrip` | 2× K₄ + weight-2 bridge from DB | load→build→Louvain→2 communities, Q > 0.25 | ✓ |

## Scope Analysis — what P3 covers vs. plan

| Plan item (PLAN.md §3) | Status |
|---|---|
| Louvain community detection on petgraph | ✅ `louvain_communities()` |
| `build_petgraph()` edge weight = co-occurrence | ✅ `build_graph()` — weight from `KgEdge.weight` (= multiplicity locally, = `memory_ids.len()` from Firestore in P4) |
| Cluster labeling (hub node → cluster name) | ✅ `label_clusters()` — hub by weighted degree + dominant node_type |
| Read local SQLite KG | ✅ `load_local_kg()` |
| Extend `rebuild_knowledge_graph()` with `?source=local` | ⏭️ **DEFERRED to P4** — already touching `routes/knowledge_graph.rs` there for `/enriched`; combining avoids two route-file commits |
| Test: 3 cliques → 3 clusters | ✅ `three_clique_communities` |

## Additional Goals Considered → Decision
| Goal | Decision | Why |
|---|---|---|
| Zachary's karate club benchmark (34 nodes, ground-truth 2-split) | SKIP | Scientific nicety; 3 synthetic tests already prove correctness. Time-constrained (~55m for P4+P5+P6). |
| Stochastic block model 1000-node perf test | SKIP | Louvain is O(n log n) amortized; 1000 nodes is milliseconds. Real user KGs are 100-500 nodes. |
| Weighted ΔQ variant (edges with varying importance) | ALREADY COVERED | `adj` maps are `f32` weights; `two_clique_split` uses uniform w=1, `sqlite_local_kg_roundtrip` exercises w=2 bridge. |
| Deterministic tie-breaking (Louvain is order-dependent) | ACCEPTED AS-IS | Node sweep order = `0..level_n` which is insertion order → deterministic for fixed input. Good enough. |

**End:** 2026-03-05 19:57:50
