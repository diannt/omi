# Phase 4 — Centrality Metrics + `/v1/knowledge-graph/enriched` API

**Start:** 2026-03-05 20:17:30

## Goal
Extend `graph_analytics.rs` with degree/betweenness/closeness centrality. Add `GET /v1/knowledge-graph/enriched` route returning clustered + scored nodes. Add `?source=local` param to existing `rebuild_knowledge_graph()` (deferred from P3).

## Algorithms
- **Degree centrality:** `deg(v) / (n−1)` — normalized 0..1
- **Closeness centrality:** `(n−1) / Σ d(v,u)` — BFS distances, undefined for disconnected → use Wasserman-Faust correction: `(reach−1)/(n−1) · (reach−1)/Σd`
- **Betweenness centrality:** Brandes O(V·E). For each source s: BFS shortest paths (track predecessors + path counts σ), reverse-iterate by decreasing distance accumulating dependencies δ. Normalize by `2/((n−1)(n−2))` for undirected.

## User decisions (feedback gate)
- **Exact Brandes, no cap** — 500-node graphs are sub-second, endpoint is GET not hot-path
- **P5 via Codemagic cloud Xcode** — Swift must compile clean, CI is the verifier
- **P6 device = XIAO BLE Sense (nRF52840)** — UF2 drag-drop, P1 firmware needs board retarget

## Files Modified
- `desktop/Backend-Rust/src/services/graph_analytics.rs` — add 3 centrality fns + `enrich_graph()` orchestrator
- `desktop/Backend-Rust/src/models/knowledge_graph.rs` — add `EnrichedGraphResponse`, `EnrichedNodeDto`, `EnrichedEdgeDto`
- `desktop/Backend-Rust/src/routes/knowledge_graph.rs` — add `/enriched` handler, `?source=local` on rebuild

## Roadblocks
- **`?source=local` on rebuild deferred again** — `Config` has no `local_db_path`, adding one means touching `config.rs` + env loading. Not on critical path: persona profile loads from Firestore (which HAS `memory_ids` → real co-occurrence). `load_local_kg()` still usable by P6 `full_pipeline` binary directly without HTTP. Net: 1 plan item dropped, zero functionality lost.
- **No route-level test** — route requires `AppState` with a Firestore mock. Covered by `enrich_two_clique` which exercises the full `enrich_graph()` orchestrator (same code path minus Firestore IO). `/enriched` handler is pure glue: load → convert → enrich → zip-back.

## Implementation Notes
- Brandes normalization: undirected graphs count each pair twice (once per endpoint as source), so divide by `(n-1)(n-2)` not `(n-1)(n-2)/2`. Verified against hand-computed K₁,₄ star (hub=1.0) and P₅ path (middle=8/12, near-end=6/12).
- Closeness uses Wasserman-Faust correction `(r-1)/(n-1)·(r-1)/Σd` so disconnected components don't return nonsense — tested on 2× K₃ isolated triangles, all nodes get 0.4.
- `EnrichedNodeDto` uses `#[serde(flatten)]` on `KnowledgeGraphNode` → Swift sees `memory_ids` at top level alongside `cluster_id` etc.
- Edge weight zip-back: analytics collapses multi-edges onto unordered pair, but API returns original Firestore edge rows. Route builds a `HashMap<(min,max), weight>` lookup so each original edge gets its pair's collapsed weight.

## Test Output

### graph_analytics (12/12 — 4 new centrality tests)
```
test services::graph_analytics::tests::star_centrality ... ok
test services::graph_analytics::tests::path_centrality ... ok
test services::graph_analytics::tests::disconnected_closeness ... ok
test services::graph_analytics::tests::enrich_two_clique ... ok
(+ 8 P3 tests unchanged)
```

### Full suite (40/40)
```
test result: ok. 40 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.76s
```

### Centrality test assertions (exact values)
| Test | Topology | Assertion | Result |
|---|---|---|---|
| `star_centrality` | K₁,₄ | hub deg=1.0, bet=1.0, clo=1.0; leaf deg=0.25, bet=0.0, clo=4/7 | ✓ |
| `path_centrality` | P₅ | endpoints bet=0; middle bet=8/12; near-end bet=6/12; symmetric | ✓ |
| `disconnected_closeness` | 2× isolated K₃ | all nodes clo=0.4 (WF), all bet=0.0 | ✓ |
| `enrich_two_clique` | 2× K₄ + bridge | 8 nodes, 13 edges, 2 clusters, Q>0.3; bridge endpoints highest bet | ✓ |

## Scope Analysis vs PLAN.md §4
| Plan item | Status |
|---|---|
| `degree_centrality()` | ✅ normalized 0..1 |
| `betweenness_centrality()` — Brandes O(V·E) | ✅ exact, normalized for undirected |
| `closeness_centrality()` | ✅ Wasserman-Faust for disconnected |
| `GET /v1/knowledge-graph/enriched` | ✅ route + DTOs |
| `EnrichedGraphResponse{nodes,edges,clusters}` | ✅ + `modularity` bonus field |
| Edge weight = memory_ids.len() | ✅ in route converter |
| `?source=local` on rebuild | ⏭️ **DROPPED** — see Roadblocks |
| Test: star topology hub centrality=1.0 | ✅ `star_centrality` |
| Test: sum(edge weights) sanity | ✅ `enrich_two_clique` counts |

**End:** 2026-03-05 20:26:32

