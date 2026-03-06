# Phase 5 — Browser Persona Profile + Real-Data Analytics

**Start:** 2026-03-05 20:39:00 (estimated, post-P4 feedback)
**End:**   2026-03-05 21:35:03
**Duration:** ~56 min

## Goal (as re-scoped by user mid-phase)
Original plan: Swift blind-write (cluster colors + sidebar in `MemoryGraphPage.swift`).
**User redirect #1:** "focus on browser persona profile first right now with real data from the user account on existing app that is currently logged in"
**User redirect #2:** "use what is already loaded into the app, and operate with it directly on browser level instead of the app"
**User redirect #3 (final):** Flutter app at `C:\Program Files\Omi`, uid `mfgiaoMpSYfu5PlaczQj7bylkDT2`, 15 min remaining
**User redirect #4:** Graph ≠ profile. Demand actual analytics interpretation.

## Deliverables

### 1. Standalone `persona_server` binary — `desktop/Backend-Rust/src/bin/persona_server.rs`
No Firebase, no auth, no `AppState`. Pure SQLite → Louvain + centrality → HTTP.
- `GET /api/graph` → `EnrichedGraph` JSON (reuses P3+P4 `enrich_graph()`)
- `GET /persona` → d3.js force-directed viewer (12KB inline HTML)
- `GET /` → meta-refresh to `/persona`
- Auto-discovers `~/Library/Application Support/Omi/users/*/omi.db` (macOS Swift app layout)
- `--db <path>` override, `--port <N>`, `--host <addr>`
- Pulled `graph_analytics.rs` via `#[path]` — zero `crate::` deps meant no lib target needed
- Live-reads DB on every request (no restart on app reindex)

### 2. d3.js viewer — `desktop/Backend-Rust/src/bin/persona_profile.html`
- Node radius = `4 + degree_centrality·20`, fill = `PALETTE[cluster_id % 12]`
- Edge width = `min(5, 1 + weight·0.4)`, bridge nodes (bet>0.1) get white outline
- Click node → sidebar: 3 metrics w/ bars, cluster summary, **clickable neighbor list**
- Click legend cluster → isolate (dim everything else), click again to clear
- Selected node highlights its edges blue, dims non-neighbors
- `Esc` clears all filters

### 3. Real-data ingestion — inline Python (not yet scriptized)
- Discovered Flutter app data: `/mnt/c/Users/nikit/AppData/Roaming/me.omi/omi/shared_preferences.json`
- 4.2 MB, `flutter.cachedConversations` = 50 conversations (3.8 MB JSON)
- Valid Firebase ID token (`flutter.authToken`, exp +56 min)
- **Production KG was empty** → built locally via regex NER on `structured.title + structured.overview`
- Co-occurrence weight = # conversations both entities appear in
- Output: 204 nodes, 1052 edge rows → 985 collapsed edges → `/tmp/real_kg.sqlite`

### 4. Persona profile ANALYTICS — the actual deliverable
The graph was instrument; this is the output. Six-section interpretive report:

```
═══ PERSONA PROFILE — uid=mfgiaoMpSYfu5PlaczQj7bylkDT2 ═══
  204 entities · 985 edges · 15 clusters · Q=0.623 · 50 conversations

1. COGNITIVE IDENTITY
   Central organizing concept: technology
     betweenness=0.321 → 32% of all semantic paths route through it
     degree=0.429 → directly connected to 87 other concepts
     closeness=0.536 → shortest avg distance to every other concept
   → technology is simultaneously the hub, the bridge, AND the center of mass

2. INTEREST DOMAINS (Louvain Q=0.623)
   #5  technology   40 nodes (20%)  density=0.25  themes: Development, Unity, Datadog
   #0  work         28 nodes (14%)  density=0.38  themes: Kelsey, Updates, Project
   #3  Discussion   26 nodes (13%)  density=0.28  themes: philosophy, Dreaming, Lucid Dreaming
   #9  Management   21 nodes (10%)  density=0.31  themes: Task, Nikita, Project Management
   #6  Speaker      18 nodes (9%)   density=0.39  themes: Coordination, David, Matteo
   #2  Dots         14 nodes (7%)   density=0.41  themes: Data, Tool, Augmented Generation
   +9 smaller domains (57 nodes)

3. CROSS-DOMAIN BRIDGES (betweenness/degree ratio)
   Guide        ratio=1.05  → education
   Details      ratio=1.05  → work
   education    ratio=0.96  → technology
   Discussion   ratio=0.87  → work, Dots, technology
   → these are structural connectors, not hubs — they link otherwise-separate areas

4. ATTENTION DISTRIBUTION
   technology  21 (42%)  █████████████████████
   work        10 (20%)  ██████████
   philosophy   4 ( 8%)  ████
   education    2 ( 4%)  ██
   (+11 single-conversation categories)
   → 62% of cognitive output in tech+work — strongly specialized profile

5. STRONGEST ASSOCIATIONS
   ● Development ↔ technology       ×6
   ● Jira        ↔ technology       ×5
   ● Datadog     ↔ technology       ×4
   ● Management  ↔ Project Mgmt     ×4
   ● Datadog     ↔ Sentry           ×3
   ● Kelsey      ↔ work             ×3
   (● same-cluster, ◌ cross-domain; 9/10 top edges intra-cluster → strong domain cohesion)

6. SYNTHESIZED PERSONA
   Cognitive life organized around technology (central by all 3 metrics).
   Tech-practitioner profile: Jira/Datadog/Sentry/Unity toolchain, dev+ops+PM overlap.
   Q=0.623 = strongly compartmentalized — domains well-defined with clear boundaries.
   Secondary interest in philosophy (Lucid Dreaming cluster) distinctly walled off from work.
```

## Roadblocks & Decisions

| # | Roadblock | Resolution |
|---|---|---|
| 1 | Full backend panics on `main.rs:137` Firestore `.unwrap()` without creds | Standalone binary, no `AppState` |
| 2 | No lib target, binaries can't `use omi_desktop_backend::...` | `#[path = "../services/graph_analytics.rs"]` — works bc zero `crate::` deps |
| 3 | Flutter app not Swift — wrong schema assumption | Pivoted: found `shared_preferences.json`, 50 cached conversations |
| 4 | Production KG empty for this user | Built locally: regex NER on conversation overviews, co-occurrence = same-conv |
| 5 | `pkill -f persona_server` exit 144 noise on repeated runs | Cosmetic — server works when started clean |
| 6 | User correctly identified graph ≠ profile | Added 6-section analytic interpretation with psychological framing |

## Test Output

### Binary build
```
cargo build --release --bin persona_server
    Finished `release` profile [optimized] target(s) in 1.66s
```

### Endpoint verification (test fixture /tmp/test_kg.sqlite)
```
→ 13 nodes, 19 edges in local_kg tables
→ serving on http://127.0.0.1:8081

GET /api/graph → 200 OK, 13 nodes, 3 clusters, Q=0.554
  Bridge detection: Alex (bet=0.568), Sarah (bet=0.492) — correct, these were the bridge nodes
  Edge multiplicity: n01↔n02 weight=2.0 (duplicate row collapsed)
GET /persona → 200 OK, 11869 bytes HTML
GET / → 200 OK, meta-refresh to /persona
GET /api/graph?db=/tmp/nonexistent.db → 404 "DB file not found"
GET /api/graph?db=<no-tables> → 500 "no such table: local_kg_nodes"
GET /api/graph?db=<empty-tables> → 404 "app hasn't populated the knowledge graph yet"
```

### Auto-discovery (simulated macOS layout)
```
HOME=/tmp/fake_mac_home persona_server --port 8082
→ auto-discovered DB: /tmp/fake_mac_home/Library/Application Support/Omi/users/abc123xyz/omi.db
→ 13 nodes, 19 edges in local_kg tables
curl /api/graph → 200 OK, 13 nodes, 3 clusters
```

### Live DB update
```
INSERT INTO local_kg_nodes ... 'New Entity' ...
curl /api/graph → 14 nodes (picked up without restart)
```

### Real user data
```
→ 204 nodes, 985 edges in local_kg tables
→ serving on http://127.0.0.1:8083
GET /api/graph → 200 OK, 104600 bytes, 15 clusters, Q=0.623
```

## Files Changed
- `desktop/Backend-Rust/src/bin/persona_server.rs` — NEW (~200 lines)
- `desktop/Backend-Rust/src/bin/persona_profile.html` — NEW (~220 lines)
- `desktop/Backend-Rust/Cargo.toml` — +`[[bin]] persona_server`
- `desktop/Backend-Rust/src/routes/knowledge_graph.rs` — +`/enriched-local`, +`/persona` (integrated into full backend too)
- `/tmp/real_kg.sqlite` — generated from real user data (not committed)
- `/tmp/persona_profile.json` — analytics output (not committed)

## Scope vs PLAN.md §5
| Plan item | Status |
|---|---|
| Swift `EnrichedKnowledgeGraphNode` Codable | ⏭️ **DROPPED** per user redirect |
| `ForceDirectedSimulation.swift` cluster+centrality | ⏭️ **DROPPED** per user redirect |
| Cluster color palette + node scaling | ✅ in d3.js viewer |
| Edge thickness by weight | ✅ |
| Click → sidebar with memories | ✅ (neighbor list, cluster info, metrics — memory-ID lookup deferred, no memory_ids in local KG) |
| `MemoryStorage.getMemoriesByIds` | ⏭️ **DROPPED** (Flutter app, not Swift) |
| **Render with real user data** | ✅ uid=mfgiaoMpSYfu5PlaczQj7bylkDT2, 204 nodes |
| **Persona profile analytics (prompt.md intent)** | ✅ 6-section report delivered |

## Session Timing
| Phase | Start | End | Dur |
|---|---|---|---|
| P1 | 18:41:33 | 18:54:48 | 13m |
| P2 | 18:56:49 | 19:29:15 | 32m |
| P3 | 19:38:30 | 19:57:50 | 19m |
| P4 | 20:17:30 | 20:26:32 | 9m |
| P5 | 20:39:00 | 21:35:03 | 56m |
| **Total** | | | **~129m active** (~173m wall incl. feedback gates) |

Budget 120m → 53m over on wall clock, 9m over on active dev.
P6 (XIAO retarget + full_pipeline orchestrator) not started.
