//! Standalone persona-profile server.
//!
//! Reads the Omi desktop app's local SQLite knowledge graph directly
//! (no Firebase, no auth) and serves an interactive d3.js viewer on localhost.
//!
//! Flow: App loads → scrapes/indexes → writes local_kg_nodes/edges to SQLite
//!       → this binary reads that SQLite → Louvain + centrality → browser.
//!
//! Usage:
//!   persona_server                       # auto-discover ~/Library/Application Support/Omi/users/*/omi.db
//!   persona_server --db /path/to/omi.db  # explicit path
//!   persona_server --port 8081           # custom port (default 8081)
//!
//! Then open: http://localhost:8081/persona

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::Html,
    routing::get,
    Json, Router,
};
use clap::Parser;
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::PathBuf;

// Pull graph_analytics directly by path — no lib target in this crate,
// and graph_analytics.rs has zero `crate::` deps (pure petgraph/rusqlite/serde).
#[path = "../services/graph_analytics.rs"]
mod graph_analytics;
use graph_analytics::{enrich_graph, load_local_kg, EnrichedGraph};

#[derive(Parser, Debug)]
#[command(about = "Serve interactive persona knowledge-graph from local Omi SQLite")]
struct Args {
    /// Path to omi.db. If omitted, auto-discovers under ~/Library/Application Support/Omi/users/*/omi.db
    #[arg(long)]
    db: Option<PathBuf>,

    /// Listen port
    #[arg(long, default_value_t = 8081)]
    port: u16,

    /// Bind address
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
}

#[derive(Clone)]
struct Srv {
    db_path: PathBuf,
}

#[derive(Debug, Deserialize)]
struct OverrideQuery {
    /// Optional ?db=<path> override for serving a different DB without restart
    db: Option<String>,
}

/// Auto-discover the app's SQLite DB.
/// macOS: ~/Library/Application Support/Omi/users/*/omi.db
/// Picks the most recently modified if multiple user dirs exist.
fn discover_db() -> Option<PathBuf> {
    let home = dirs_home()?;
    let users_dir = home
        .join("Library")
        .join("Application Support")
        .join("Omi")
        .join("users");

    let mut candidates: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&users_dir) {
        for entry in entries.flatten() {
            let db = entry.path().join("omi.db");
            if db.is_file() {
                let mtime = db
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                candidates.push((db, mtime));
            }
        }
    }
    candidates.sort_by_key(|(_, m)| *m);
    candidates.pop().map(|(p, _)| p)
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// GET /api/graph — enriched graph JSON (Louvain + centrality)
async fn api_graph(
    State(srv): State<Srv>,
    Query(q): Query<OverrideQuery>,
) -> Result<Json<EnrichedGraph>, (StatusCode, String)> {
    let db_path = q
        .db
        .map(PathBuf::from)
        .unwrap_or_else(|| srv.db_path.clone());

    if !db_path.is_file() {
        return Err((
            StatusCode::NOT_FOUND,
            format!("DB file not found: {}", db_path.display()),
        ));
    }

    let (nodes, edges) = load_local_kg(&db_path).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("SQLite read failed: {}", e),
        )
    })?;

    if nodes.is_empty() {
        return Err((
            StatusCode::NOT_FOUND,
            format!(
                "DB {} has no local_kg_nodes — app hasn't populated the knowledge graph yet",
                db_path.display()
            ),
        ));
    }

    Ok(Json(enrich_graph(&nodes, &edges)))
}

/// GET /persona — single-page d3 viewer
async fn persona_page() -> Html<&'static str> {
    Html(include_str!("persona_profile.html"))
}

/// GET / — redirect-ish landing
async fn root() -> Html<&'static str> {
    Html(r#"<meta http-equiv="refresh" content="0; url=/persona">"#)
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let db_path = match args.db {
        Some(p) => p,
        None => match discover_db() {
            Some(p) => {
                println!("→ auto-discovered DB: {}", p.display());
                p
            }
            None => {
                eprintln!("✗ No omi.db found under ~/Library/Application Support/Omi/users/*/");
                eprintln!("  Pass --db <path> to specify explicitly.");
                eprintln!("  (On macOS, the Omi app must have run and indexed at least once.)");
                std::process::exit(1);
            }
        },
    };

    if !db_path.is_file() {
        eprintln!("✗ DB file does not exist: {}", db_path.display());
        std::process::exit(1);
    }

    // Preview: load once at boot to report stats (not cached — /api/graph rereads live)
    match load_local_kg(&db_path) {
        Ok((nodes, edges)) => {
            println!(
                "→ {} nodes, {} edges in local_kg tables",
                nodes.len(),
                edges.len()
            );
            if nodes.is_empty() {
                println!("  ⚠ graph is empty — open Omi, index files, then refresh");
            }
        }
        Err(e) => {
            eprintln!("✗ Could not read local_kg tables: {}", e);
            eprintln!("  (Tables local_kg_nodes / local_kg_edges not found?)");
            std::process::exit(1);
        }
    }

    let srv = Srv { db_path };
    let app = Router::new()
        .route("/", get(root))
        .route("/persona", get(persona_page))
        .route("/api/graph", get(api_graph))
        .with_state(srv);

    let addr: SocketAddr = format!("{}:{}", args.host, args.port)
        .parse()
        .expect("invalid host:port");
    println!("→ serving on http://{}", addr);
    println!("  open http://{}/persona", addr);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("bind failed");
    axum::serve(listener, app).await.expect("serve failed");
}
