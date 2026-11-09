use std::net::{Ipv4Addr, SocketAddr};

use anyhow::{Context, Result};
use axum::{
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    middleware::{from_fn_with_state, Next},
    response::Response,
    routing::get,
    Json, Router,
};
use serde_json::json;
use tracing_subscriber::EnvFilter;

use ryu_places::{
    api, geocode::Geocoder, mcp, paths, seal_keys::PluginKeys, sidecar_link, state::AppState,
    state::Config, state::PLUGIN_ID, tenancy::Stores,
};

const MOUNT: &str = "/api/places";

fn main() -> Result<()> {
    // Take the sealing key Core delivered out of the environment before any
    // thread starts (the async runtime spawns workers).
    ryu_places::seal_keys::capture()?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let port = std::env::var("RYU_PLACES_PORT")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(ryu_places::state::DEFAULT_PORT);
    let data_dir = paths::data_dir();
    let db_path = paths::local_db_path(&data_dir);
    let mcp_mode = std::env::args().nth(1).as_deref() == Some("mcp");
    // The sidecar resolves its key normally. The stdio MCP process gets no key
    // from Core and never generates a standalone root (it would seal under a
    // key the sidecar does not hold); it runs its tools through the sidecar.
    let keys = if mcp_mode {
        PluginKeys::resolve_existing(PLUGIN_ID, &data_dir)?
    } else {
        PluginKeys::resolve(PLUGIN_ID, &data_dir)?
    };
    let stores = Stores::on_disk_with_keys(data_dir.clone(), keys);
    // Open the local map eagerly so a broken data dir fails at boot, not on use.
    stores
        .get(ryu_places::tenancy::LOCAL_TENANT)
        .await
        .with_context(|| format!("opening {}", db_path.display()))?;
    let state = AppState::new(stores, Geocoder::from_env(), Config::from_env(port));

    if mcp_mode {
        return mcp::serve(state, Some(data_dir)).await;
    }

    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("binding {address}"))?;
    // Publish the loopback handoff for the keyless stdio `mcp` process.
    let handoff = sidecar_link::publish(&data_dir, listener.local_addr()?.port())?;
    tracing::info!(port, mount = MOUNT, db = %db_path.display(), "ryu-places: listening");
    let served = axum::serve(listener, app(state, handoff.token.clone()))
        .with_graceful_shutdown(sidecar_link::shutdown_signal())
        .await;
    sidecar_link::withdraw(&data_dir, &handoff);
    served.context("places server stopped")?;
    Ok(())
}

/// The sidecar's router: `/health`, the Core-proxied mount behind the ext
/// token, and the handoff route behind the per-run handoff token.
fn app(state: AppState, handoff_token: String) -> Router {
    let state_for_handoff = state.clone();
    let gated = Router::new()
        .nest(MOUNT, api::routes(state.clone()))
        .layer(from_fn_with_state(state.clone(), bearer_gate));
    Router::new()
        .route("/health", get(health))
        .with_state(state)
        .merge(gated)
        .merge(mcp::handoff_router(state_for_handoff, handoff_token))
}

async fn health(State(state): State<AppState>) -> Result<Json<serde_json::Value>, StatusCode> {
    let local = state.stores.get(ryu_places::tenancy::LOCAL_TENANT).await;
    match async { local?.counts().await }.await {
        Ok(count) => {
            // The sealing state of every map opened so far (unhealthy wins).
            let mut sealing = json!({ "state": "ok", "unreadable_rows": 0 });
            let mut unreadable = 0u64;
            for (_, store) in state.stores.open_maps().await {
                let health = store.seal_health();
                unreadable += health["unreadable_rows"].as_u64().unwrap_or(0);
                let rank = |state: &serde_json::Value| match state.as_str() {
                    Some("unhealthy") => 2,
                    Some("compat") => 1,
                    _ => 0,
                };
                if rank(&health["state"]) > rank(&sealing["state"]) {
                    sealing = health;
                }
            }
            sealing["unreadable_rows"] = unreadable.into();
            Ok(Json(
                json!({ "ok": true, "places": count, "sealing": sealing }),
            ))
        }
        Err(error) => {
            tracing::error!(error = %error, "places health check failed");
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

async fn bearer_gate(
    State(state): State<AppState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let provided = bearer_of(request.headers());
    if ryu_sidecar_runtime::token_ok(provided.as_deref(), state.config.token.as_deref()) {
        Ok(next.run(request).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

fn bearer_of(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_owned)
}
