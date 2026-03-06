// Knowledge Graph Routes
// API endpoints for the 3D memory visualization

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::Html,
    routing::{delete, get, post},
    Json, Router,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::auth::AuthUser;
use crate::llm::LlmClient;
use crate::models::{
    ClusterInfoDto, EnrichedEdgeDto, EnrichedGraphResponse, EnrichedNodeDto, KnowledgeGraphEdge,
    KnowledgeGraphNode, KnowledgeGraphResponse, KnowledgeGraphStatusResponse, NodeType,
    RebuildGraphResponse,
};
use crate::services::graph_analytics::{
    enrich_graph, load_local_kg, EnrichedGraph, KgEdge, KgNode,
};
use crate::AppState;

/// GET /v1/knowledge-graph - Get the full knowledge graph
async fn get_knowledge_graph(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<KnowledgeGraphResponse>, StatusCode> {
    tracing::info!("Getting knowledge graph for user {}", user.uid);

    let nodes = state
        .firestore
        .get_kg_nodes(&user.uid)
        .await
        .map_err(|e| {
            tracing::error!("Failed to get KG nodes: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let edges = state
        .firestore
        .get_kg_edges(&user.uid)
        .await
        .map_err(|e| {
            tracing::error!("Failed to get KG edges: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(Json(KnowledgeGraphResponse { nodes, edges }))
}

/// Query parameters for rebuild
#[derive(Debug, Deserialize)]
pub struct RebuildQuery {
    pub limit: Option<usize>,
}

/// POST /v1/knowledge-graph/rebuild - Rebuild the knowledge graph from memories
async fn rebuild_knowledge_graph(
    State(state): State<AppState>,
    user: AuthUser,
    Query(query): Query<RebuildQuery>,
) -> Result<Json<RebuildGraphResponse>, StatusCode> {
    tracing::info!("Rebuilding knowledge graph for user {}", user.uid);

    let limit = query.limit.unwrap_or(500);

    // Check for Gemini API key
    let api_key = state.config.gemini_api_key.clone().ok_or_else(|| {
        tracing::error!("Gemini API key not configured");
        StatusCode::SERVICE_UNAVAILABLE
    })?;

    // Delete existing graph
    if let Err(e) = state.firestore.delete_kg_data(&user.uid).await {
        tracing::warn!("Failed to delete existing graph: {}", e);
    }

    // Get memories to process
    let memories = state
        .firestore
        .get_memories(&user.uid, limit)
        .await
        .map_err(|e| {
            tracing::error!("Failed to get memories: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    if memories.is_empty() {
        tracing::info!("No memories found for user {}, skipping rebuild", user.uid);
        return Ok(Json(RebuildGraphResponse {
            status: "completed".to_string(),
            message: "No memories to process".to_string(),
        }));
    }

    tracing::info!("Processing {} memories for knowledge graph", memories.len());

    // Create LLM client
    let llm = LlmClient::new(api_key);

    // Track nodes by lowercase label for deduplication
    let mut node_map: HashMap<String, KnowledgeGraphNode> = HashMap::new();
    let mut edges: Vec<KnowledgeGraphEdge> = Vec::new();

    // Process memories in batches
    for memory in &memories {
        // Get current nodes for deduplication context
        let existing_nodes: Vec<KnowledgeGraphNode> = node_map.values().cloned().collect();

        // Extract entities from this memory
        let extraction = match llm
            .extract_knowledge_graph_entities(&memory.content, &existing_nodes)
            .await
        {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("Failed to extract entities from memory {}: {}", memory.id, e);
                continue;
            }
        };

        // Process extracted entities
        for entity in extraction.entities {
            let label_lower = entity.name.to_lowercase();

            // Check if entity already exists (by label or alias)
            let existing_key = node_map
                .iter()
                .find(|(_, n)| {
                    n.label_lower == label_lower
                        || n.aliases_lower.contains(&label_lower)
                        || entity
                            .aliases
                            .iter()
                            .any(|a| n.label_lower == a.to_lowercase())
                })
                .map(|(k, _)| k.clone());

            if let Some(key) = existing_key {
                // Update existing node with new memory reference
                if let Some(node) = node_map.get_mut(&key) {
                    node.add_memory_id(memory.id.clone());
                }
            } else {
                // Create new node
                let node_type = match entity.entity_type.as_str() {
                    "person" => NodeType::Person,
                    "place" => NodeType::Place,
                    "organization" => NodeType::Organization,
                    "thing" => NodeType::Thing,
                    _ => NodeType::Concept,
                };

                let mut node = KnowledgeGraphNode::new(entity.name.clone(), node_type);
                node = node.with_aliases(entity.aliases);
                node.add_memory_id(memory.id.clone());

                node_map.insert(label_lower, node);
            }
        }

        // Process relationships
        for rel in extraction.relationships {
            let source_lower = rel.source.to_lowercase();
            let target_lower = rel.target.to_lowercase();

            // Find source and target nodes
            let source_id = node_map
                .iter()
                .find(|(_, n)| n.label_lower == source_lower || n.aliases_lower.contains(&source_lower))
                .map(|(_, n)| n.id.clone());

            let target_id = node_map
                .iter()
                .find(|(_, n)| n.label_lower == target_lower || n.aliases_lower.contains(&target_lower))
                .map(|(_, n)| n.id.clone());

            if let (Some(src), Some(tgt)) = (source_id, target_id) {
                let mut edge = KnowledgeGraphEdge::new(src, tgt, rel.relationship);
                edge.add_memory_id(memory.id.clone());
                edges.push(edge);
            }
        }
    }

    // Save nodes to Firestore
    let nodes: Vec<KnowledgeGraphNode> = node_map.into_values().collect();
    for node in &nodes {
        if let Err(e) = state.firestore.upsert_kg_node(&user.uid, node).await {
            tracing::warn!("Failed to save node {}: {}", node.label, e);
        }
    }

    // Deduplicate edges (same source, target, label)
    let mut edge_keys: HashMap<String, KnowledgeGraphEdge> = HashMap::new();
    for edge in edges {
        let key = format!("{}_{}_{}", edge.source_id, edge.label, edge.target_id);
        edge_keys
            .entry(key)
            .and_modify(|e| {
                for mid in &edge.memory_ids {
                    if !e.memory_ids.contains(mid) {
                        e.memory_ids.push(mid.clone());
                    }
                }
            })
            .or_insert(edge);
    }

    // Save edges to Firestore
    for edge in edge_keys.values() {
        if let Err(e) = state.firestore.upsert_kg_edge(&user.uid, edge).await {
            tracing::warn!("Failed to save edge {}: {}", edge.id, e);
        }
    }

    tracing::info!(
        "Built knowledge graph with {} nodes and {} edges for user {}",
        nodes.len(),
        edge_keys.len(),
        user.uid
    );

    Ok(Json(RebuildGraphResponse {
        status: "completed".to_string(),
        message: format!(
            "Built graph with {} nodes and {} edges from {} memories",
            nodes.len(),
            edge_keys.len(),
            memories.len()
        ),
    }))
}

/// GET /v1/knowledge-graph/enriched - KG with Louvain clusters + centrality scores.
/// Edge weight = memory_ids.len() (co-occurrence count).
async fn get_enriched_graph(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<EnrichedGraphResponse>, StatusCode> {
    tracing::info!("Getting enriched knowledge graph for user {}", user.uid);

    let fs_nodes = state.firestore.get_kg_nodes(&user.uid).await.map_err(|e| {
        tracing::error!("Failed to get KG nodes: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let fs_edges = state.firestore.get_kg_edges(&user.uid).await.map_err(|e| {
        tracing::error!("Failed to get KG edges: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // Convert Firestore models → analytics input types.
    // Edge weight = co-occurrence count (memory_ids.len()), min 1.
    let kg_nodes: Vec<KgNode> = fs_nodes
        .iter()
        .map(|n| KgNode {
            id: n.id.clone(),
            label: n.label.clone(),
            node_type: n.node_type.to_string(),
        })
        .collect();
    let kg_edges: Vec<KgEdge> = fs_edges
        .iter()
        .map(|e| KgEdge {
            source_id: e.source_id.clone(),
            target_id: e.target_id.clone(),
            label: e.label.clone(),
            weight: e.memory_ids.len().max(1) as f32,
        })
        .collect();

    let enriched = enrich_graph(&kg_nodes, &kg_edges);

    // Zip analytics scores back onto original Firestore nodes (preserving memory_ids).
    let scores: HashMap<&str, (u32, f64, f64, f64)> = enriched
        .nodes
        .iter()
        .map(|n| {
            (
                n.id.as_str(),
                (
                    n.cluster_id,
                    n.degree_centrality,
                    n.betweenness_centrality,
                    n.closeness_centrality,
                ),
            )
        })
        .collect();

    let out_nodes: Vec<EnrichedNodeDto> = fs_nodes
        .into_iter()
        .map(|base| {
            let (cid, deg, bet, clo) = scores
                .get(base.id.as_str())
                .copied()
                .unwrap_or((0, 0.0, 0.0, 0.0));
            EnrichedNodeDto {
                base,
                cluster_id: cid,
                degree_centrality: deg,
                betweenness_centrality: bet,
                closeness_centrality: clo,
            }
        })
        .collect();

    // For edges: collapsed weights from analytics, but keep original edge rows
    // (the analytics layer collapses multi-edges, so map by unordered pair).
    let weight_map: HashMap<(String, String), f32> = enriched
        .edges
        .iter()
        .map(|e| ((e.source_id.clone(), e.target_id.clone()), e.weight))
        .collect();

    let out_edges: Vec<EnrichedEdgeDto> = fs_edges
        .into_iter()
        .map(|base| {
            let (a, b) = if base.source_id <= base.target_id {
                (base.source_id.clone(), base.target_id.clone())
            } else {
                (base.target_id.clone(), base.source_id.clone())
            };
            let weight = weight_map
                .get(&(a, b))
                .copied()
                .unwrap_or_else(|| base.memory_ids.len().max(1) as f32);
            EnrichedEdgeDto { base, weight }
        })
        .collect();

    let out_clusters: Vec<ClusterInfoDto> = enriched
        .clusters
        .into_iter()
        .map(|c| ClusterInfoDto {
            id: c.id,
            label: c.label,
            node_count: c.node_count,
            dominant_type: c.dominant_type,
        })
        .collect();

    tracing::info!(
        "Enriched graph for user {}: {} nodes, {} edges, {} clusters, Q={:.3}",
        user.uid,
        out_nodes.len(),
        out_edges.len(),
        out_clusters.len(),
        enriched.modularity
    );

    Ok(Json(EnrichedGraphResponse {
        nodes: out_nodes,
        edges: out_edges,
        clusters: out_clusters,
        modularity: enriched.modularity,
    }))
}

/// DELETE /v1/knowledge-graph - Delete the knowledge graph
async fn delete_knowledge_graph(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<KnowledgeGraphStatusResponse>, StatusCode> {
    tracing::info!("Deleting knowledge graph for user {}", user.uid);

    state
        .firestore
        .delete_kg_data(&user.uid)
        .await
        .map_err(|e| {
            tracing::error!("Failed to delete KG data: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(Json(KnowledgeGraphStatusResponse {
        success: true,
        message: "Knowledge graph deleted".to_string(),
    }))
}

// ============================================================================
// Local SQLite path — no Firebase auth, reads app's GRDB database directly.
// Intended for the browser persona-profile viewer when running against the
// desktop app's local data. The `db` query param is the full path to
// `rewind.sqlite` (macOS: ~/Library/Application Support/Omi/rewind.sqlite).
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct LocalDbQuery {
    pub db: String,
}

/// GET /v1/knowledge-graph/enriched-local?db=<path>
/// Unauth'd: reads `local_kg_nodes` + `local_kg_edges` from the given SQLite
/// file, runs full Louvain + centrality, returns the enriched graph.
async fn get_enriched_local(
    Query(q): Query<LocalDbQuery>,
) -> Result<Json<EnrichedGraph>, (StatusCode, String)> {
    let db_path = PathBuf::from(&q.db);
    if !db_path.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            format!("DB file not found: {}", q.db),
        ));
    }

    let (nodes, edges) = load_local_kg(&db_path).map_err(|e| {
        tracing::error!("load_local_kg({}) failed: {}", q.db, e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("SQLite read failed: {}", e),
        )
    })?;

    tracing::info!(
        "Loaded {} nodes, {} edges from local DB {}",
        nodes.len(),
        edges.len(),
        q.db
    );

    let enriched = enrich_graph(&nodes, &edges);

    tracing::info!(
        "Enriched local graph: {} clusters, Q={:.3}",
        enriched.clusters.len(),
        enriched.modularity
    );

    Ok(Json(enriched))
}

/// GET /persona?db=<path>
/// Serves a single-page d3.js force-directed graph viewer. Fetches
/// `/v1/knowledge-graph/enriched-local?db=<same>` and renders:
///   - node color = cluster_id (categorical palette)
///   - node radius = 4 + degree_centrality · 20
///   - edge width = 1 + weight · 0.5 (capped)
///   - click node → sidebar with label, type, centralities
async fn persona_profile_page(Query(q): Query<LocalDbQuery>) -> Html<String> {
    // Escape the db path for safe embedding in the JS string literal.
    let db_escaped = q.db.replace('\\', "\\\\").replace('"', "\\\"");
    let html = format!(
        r##"<!DOCTYPE html>
<html>
<head>
  <meta charset="utf-8">
  <title>Persona Profile — Knowledge Graph</title>
  <style>
    body {{ margin: 0; font-family: -apple-system, BlinkMacSystemFont, sans-serif; display: flex; height: 100vh; background: #0a0a0f; color: #e4e4e7; }}
    #graph {{ flex: 1; }}
    #sidebar {{ width: 320px; padding: 16px; background: #18181b; border-left: 1px solid #27272a; overflow-y: auto; }}
    #sidebar h2 {{ margin: 0 0 4px 0; font-size: 18px; }}
    #sidebar .type {{ color: #71717a; font-size: 12px; text-transform: uppercase; letter-spacing: 0.05em; margin-bottom: 12px; }}
    #sidebar .metric {{ display: flex; justify-content: space-between; margin: 6px 0; font-size: 13px; }}
    #sidebar .metric span:last-child {{ color: #a1a1aa; font-family: monospace; }}
    #sidebar .bar {{ height: 4px; background: #27272a; border-radius: 2px; margin: 2px 0 10px; }}
    #sidebar .bar > div {{ height: 100%; background: #3b82f6; border-radius: 2px; }}
    #legend {{ position: absolute; top: 12px; left: 12px; background: rgba(24,24,27,0.9); border: 1px solid #27272a; border-radius: 6px; padding: 10px; font-size: 12px; }}
    #legend .row {{ display: flex; align-items: center; margin: 3px 0; }}
    #legend .swatch {{ width: 12px; height: 12px; border-radius: 50%; margin-right: 6px; }}
    #status {{ position: absolute; bottom: 12px; left: 12px; font-size: 11px; color: #52525b; font-family: monospace; }}
    .node {{ cursor: pointer; }}
    .node-label {{ font-size: 10px; fill: #a1a1aa; pointer-events: none; }}
    .edge {{ stroke: #3f3f46; stroke-opacity: 0.4; }}
  </style>
</head>
<body>
  <svg id="graph"></svg>
  <div id="sidebar">
    <h2 id="sel-label">Click a node</h2>
    <div class="type" id="sel-type"></div>
    <div id="sel-metrics"></div>
    <hr style="border-color:#27272a; margin: 16px 0;">
    <div id="cluster-info"></div>
  </div>
  <div id="legend"></div>
  <div id="status">Loading…</div>

  <script src="https://d3js.org/d3.v7.min.js"></script>
  <script>
    const DB_PATH = "{db_escaped}";
    const PALETTE = ['#3b82f6','#ef4444','#10b981','#f59e0b','#8b5cf6','#ec4899','#14b8a6','#f97316','#6366f1','#84cc16','#06b6d4','#a855f7'];

    const status = document.getElementById('status');
    const url = `/v1/knowledge-graph/enriched-local?db=${{encodeURIComponent(DB_PATH)}}`;
    status.textContent = `Fetching ${{url}}…`;

    fetch(url).then(r => {{
      if (!r.ok) return r.text().then(t => {{ throw new Error(`${{r.status}}: ${{t}}`); }});
      return r.json();
    }}).then(render).catch(e => {{
      status.textContent = `ERROR: ${{e.message}}`;
      status.style.color = '#ef4444';
    }});

    function render(data) {{
      const {{ nodes, edges, clusters, modularity }} = data;
      status.textContent = `${{nodes.length}} nodes · ${{edges.length}} edges · ${{clusters.length}} clusters · Q=${{modularity.toFixed(3)}}`;

      // Legend
      const legend = document.getElementById('legend');
      legend.innerHTML = '<div style="font-weight:600;margin-bottom:6px;">Clusters</div>' +
        clusters.map(c =>
          `<div class="row"><span class="swatch" style="background:${{PALETTE[c.id % PALETTE.length]}}"></span>` +
          `<span title="${{c.dominant_type}}, ${{c.node_count}} nodes">${{c.label}}</span></div>`
        ).join('');

      // Cluster lookup for sidebar
      const clusterById = Object.fromEntries(clusters.map(c => [c.id, c]));

      // d3-force expects {{source, target}} keys referencing node IDs
      const links = edges.map(e => ({{ source: e.source_id, target: e.target_id, weight: e.weight }}));

      const svg = d3.select('#graph');
      const {{ width, height }} = svg.node().getBoundingClientRect();
      svg.attr('viewBox', [0, 0, width, height]);

      const g = svg.append('g');
      svg.call(d3.zoom().scaleExtent([0.2, 5]).on('zoom', (e) => g.attr('transform', e.transform)));

      const sim = d3.forceSimulation(nodes)
        .force('link', d3.forceLink(links).id(d => d.id).distance(60).strength(l => Math.min(1, l.weight / 5)))
        .force('charge', d3.forceManyBody().strength(-120))
        .force('center', d3.forceCenter(width / 2, height / 2))
        .force('collide', d3.forceCollide().radius(d => 6 + d.degree_centrality * 22));

      const link = g.append('g').selectAll('line').data(links).join('line')
        .attr('class', 'edge')
        .attr('stroke-width', d => Math.min(5, 1 + d.weight * 0.4));

      const node = g.append('g').selectAll('circle').data(nodes).join('circle')
        .attr('class', 'node')
        .attr('r', d => 4 + d.degree_centrality * 20)
        .attr('fill', d => PALETTE[d.cluster_id % PALETTE.length])
        .attr('stroke', d => d.betweenness_centrality > 0.1 ? '#fff' : 'none')
        .attr('stroke-width', d => d.betweenness_centrality > 0.1 ? 2 : 0)
        .on('click', (_, d) => selectNode(d, clusterById))
        .call(drag(sim));

      const label = g.append('g').selectAll('text').data(nodes).join('text')
        .attr('class', 'node-label')
        .text(d => d.degree_centrality > 0.15 ? d.label : '')
        .attr('dy', -8);

      sim.on('tick', () => {{
        link.attr('x1', d => d.source.x).attr('y1', d => d.source.y)
            .attr('x2', d => d.target.x).attr('y2', d => d.target.y);
        node.attr('cx', d => d.x).attr('cy', d => d.y);
        label.attr('x', d => d.x).attr('y', d => d.y);
      }});

      // Auto-select highest-betweenness node
      if (nodes.length) {{
        const top = nodes.reduce((a, b) => a.betweenness_centrality > b.betweenness_centrality ? a : b);
        selectNode(top, clusterById);
      }}
    }}

    function selectNode(d, clusterById) {{
      document.getElementById('sel-label').textContent = d.label;
      document.getElementById('sel-type').textContent = `${{d.node_type}} · cluster #${{d.cluster_id}}`;
      const m = document.getElementById('sel-metrics');
      m.innerHTML = '';
      for (const [name, val] of [
        ['Degree', d.degree_centrality],
        ['Betweenness', d.betweenness_centrality],
        ['Closeness', d.closeness_centrality],
      ]) {{
        m.innerHTML += `<div class="metric"><span>${{name}}</span><span>${{val.toFixed(4)}}</span></div>` +
                       `<div class="bar"><div style="width:${{Math.min(100, val*100).toFixed(1)}}%"></div></div>`;
      }}
      const c = clusterById[d.cluster_id];
      if (c) {{
        document.getElementById('cluster-info').innerHTML =
          `<div style="font-weight:600;">Cluster: ${{c.label}}</div>` +
          `<div style="font-size:12px;color:#71717a;">${{c.node_count}} nodes · ${{c.dominant_type}}</div>`;
      }}
    }}

    function drag(sim) {{
      return d3.drag()
        .on('start', (e, d) => {{ if (!e.active) sim.alphaTarget(0.3).restart(); d.fx = d.x; d.fy = d.y; }})
        .on('drag', (e, d) => {{ d.fx = e.x; d.fy = e.y; }})
        .on('end', (e, d) => {{ if (!e.active) sim.alphaTarget(0); d.fx = null; d.fy = null; }});
    }}
  </script>
</body>
</html>"##
    );
    Html(html)
}

/// Build knowledge graph routes
pub fn knowledge_graph_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/knowledge-graph", get(get_knowledge_graph))
        .route("/v1/knowledge-graph/enriched", get(get_enriched_graph))
        .route("/v1/knowledge-graph/rebuild", post(rebuild_knowledge_graph))
        .route("/v1/knowledge-graph", delete(delete_knowledge_graph))
        // Local browser profile — unauth'd, reads app's SQLite directly
        .route(
            "/v1/knowledge-graph/enriched-local",
            get(get_enriched_local),
        )
        .route("/persona", get(persona_profile_page))
}
