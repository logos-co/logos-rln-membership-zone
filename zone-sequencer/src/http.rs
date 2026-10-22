//! The HTTP API a Module's node reads (its registry access): the roots with the status of
//! their transaction on Bedrock, the Merkle proof path of a leaf at the served root (the newest
//! root in a Bedrock block, or the newest Finalized one on request), a leaf's status.
//! 32-byte values are hex, little-endian (RLN-API Appendix B).

use std::sync::{Arc, RwLock};

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::get,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::registry::{Registry, Status};

pub type Shared = Arc<RwLock<Registry>>;

#[derive(Clone)]
pub struct AppState {
    pub registry: Shared,
    pub channel_id: String,
    pub lez_source: String,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(summary))
        .route("/health", get(|| async { "ok" }))
        .route("/roots/{window_size}", get(roots))
        .route("/merkle_proof/{leaf_index}", get(merkle_proof))
        .route("/leaf/{id_commitment}", get(leaf))
        .route("/leaves", get(leaves))
        .with_state(state)
}

/// `?at=adopted` (default): the newest root in a Bedrock block, as the RFC has Zone indexers
/// act on a batch once it is in a block; `?at=finalized`: the newest root in finalized
/// history.
#[derive(Deserialize)]
struct At {
    #[serde(default)]
    at: Option<String>,
}

impl At {
    fn finalized_only(&self) -> bool {
        self.at.as_deref() == Some("finalized")
    }
}

#[derive(Serialize)]
struct RootEntry {
    root: String,
    from_lez_block_id: u64,
    to_lez_block_id: u64,
    change_count: usize,
    published_at_unix: u64,
    #[serde(flatten)]
    status: Status,
    tx_hash: String,
    msg_id: String,
}

async fn summary(State(state): State<AppState>) -> Json<Value> {
    let registry = state.registry.read().expect("registry lock");
    let adopted = registry.newest_adopted();
    let finalized = registry.newest_finalized();
    Json(json!({
        "zone_channel_id": state.channel_id,
        "lez_source": state.lez_source,
        "first_lez_block_id": registry.history.first_lez_block_id,
        "latest_lez_block_id": registry.head.applied_block_id,
        "lez_timestamp_ms": registry.head.applied_timestamp_ms,
        "covered_to_lez_block_id": registry.history.covered_to,
        "tree": {
            "live_leaves": registry.head.live_count(),
            "removed_leaves": registry.head.removed_leaves().len(),
            "root_at_head": hex::encode(registry.head.root()),
            "events": registry.head.stats,
        },
        "pending_changes": registry.pending_changes.len(),
        "published_batches": registry.history.published.len(),
        "newest_adopted_root": adopted.map(|p| hex::encode(p.root_after)),
        "newest_adopted_to_lez_block_id": adopted.map(|p| p.to_lez_block_id),
        "newest_finalized_root": finalized.map(|p| hex::encode(p.root_after)),
        "newest_finalized_to_lez_block_id": finalized.map(|p| p.to_lez_block_id),
        "endpoints": ["/roots/{window_size}", "/merkle_proof/{leaf_index}[?at=finalized]", "/leaf/{id_commitment}[?at=finalized]", "/leaves", "/health"],
    }))
}

/// The newest `window_size` published roots, newest first, each with its status on Bedrock;
/// the Finalized ones are the valid roots.
async fn roots(State(state): State<AppState>, Path(window_size): Path<usize>) -> Json<Value> {
    let registry = state.registry.read().expect("registry lock");
    let roots: Vec<RootEntry> = registry
        .roots(window_size)
        .into_iter()
        .map(|p| RootEntry {
            root: hex::encode(p.root_after),
            from_lez_block_id: p.from_lez_block_id,
            to_lez_block_id: p.to_lez_block_id,
            change_count: p.change_count,
            published_at_unix: p.published_at_unix,
            status: p.status.clone(),
            tx_hash: hex::encode(p.tx_hash),
            msg_id: hex::encode(p.msg_id),
        })
        .collect();
    Json(json!({
        "latest_lez_block_id": registry.head.applied_block_id,
        "lez_timestamp_ms": registry.head.applied_timestamp_ms,
        "covered_to_lez_block_id": registry.history.covered_to,
        "roots": roots,
    }))
}

/// The Merkle proof path of the leaf at `leaf_index` at the served root.
async fn merkle_proof(
    State(state): State<AppState>,
    Path(leaf_index): Path<u64>,
    Query(at): Query<At>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let registry = state.registry.read().expect("registry lock");
    let Some((published, reader)) = registry.served(at.finalized_only()) else {
        return Err(error(
            StatusCode::SERVICE_UNAVAILABLE,
            "no batch in a Bedrock block yet",
        ));
    };
    let Some(path) = reader.proof_path(leaf_index) else {
        return Err(error(
            StatusCode::NOT_FOUND,
            "no leaf at this index at the served root",
        ));
    };
    Ok(Json(json!({
        "root": hex::encode(published.root_after),
        "to_lez_block_id": published.to_lez_block_id,
        "status": published.status,
        "leaf_index": path.leaf_index,
        "rate_commitment": hex::encode(path.rate_commitment),
        "path_elements": path.path_elements.iter().map(hex::encode).collect::<Vec<_>>(),
        "identity_path_index": path.identity_path_index,
    })))
}

/// Where a membership stands at the LEZ head and at the served root.
async fn leaf(
    State(state): State<AppState>,
    Path(id_commitment): Path<String>,
    Query(at): Query<At>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let bytes = hex::decode(id_commitment.trim_start_matches("0x"))
        .ok()
        .and_then(|v| <[u8; 32]>::try_from(v).ok());
    let Some(id_commitment) = bytes else {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "id_commitment must be 32 bytes of hex",
        ));
    };
    let registry = state.registry.read().expect("registry lock");
    let at_head = registry.head.live_leaf(&id_commitment);
    let served = registry
        .served(at.finalized_only())
        .map(|(published, reader)| {
            json!({
                "root": hex::encode(published.root_after),
                "to_lez_block_id": published.to_lez_block_id,
                "status": published.status,
                "leaf_index": reader.live_leaf(&id_commitment).map(|l| l.leaf_index),
            })
        });
    Ok(Json(json!({
        "at_head": {
            "lez_block_id": registry.head.applied_block_id,
            "leaf_index": at_head.map(|l| l.leaf_index),
            "expiry_timestamp_ms": at_head.map(|l| l.expiry_timestamp_ms),
            "removed": registry.head.removed(&id_commitment).map(|r| format!("{r:?}")),
        },
        "at_served_root": served,
    })))
}

/// Every leaf at the LEZ head: live ones with their expiry, removed ones with the cause.
async fn leaves(State(state): State<AppState>) -> Json<Value> {
    let registry = state.registry.read().expect("registry lock");
    let live: Vec<Value> = registry
        .head
        .live_leaves()
        .into_iter()
        .map(|(leaf_index, id, expiry)| {
            json!({
                "leaf_index": leaf_index,
                "id_commitment": hex::encode(id),
                "expiry_timestamp_ms": expiry,
                "expires_in_ms": expiry.saturating_sub(registry.head.applied_timestamp_ms),
            })
        })
        .collect();
    let removed: Vec<Value> = registry
        .head
        .removed_leaves()
        .into_iter()
        .map(|(id, cause)| json!({ "id_commitment": hex::encode(id), "cause": format!("{cause:?}") }))
        .collect();
    Json(json!({
        "lez_block_id": registry.head.applied_block_id,
        "lez_timestamp_ms": registry.head.applied_timestamp_ms,
        "root_at_head": hex::encode(registry.head.root()),
        "events": registry.head.stats,
        "live": live,
        "removed": removed,
    }))
}

fn error(code: StatusCode, message: &str) -> (StatusCode, Json<Value>) {
    (code, Json(json!({ "error": message })))
}
