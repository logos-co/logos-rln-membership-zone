//! The HTTP API a Module's node reads (its registry access): the roots with the Zone SDK
//! `TxStatus` of their transaction on Bedrock, the Merkle proof path of a leaf at the newest Finalized root, a leaf's status.
//! 32-byte values are hex, little-endian (RLN-API Appendix B).

use std::sync::{Arc, RwLock};

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::get,
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::registry::{Registry, source_name, status_name};

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
        .with_state(state)
}

#[derive(Serialize)]
struct RootEntry {
    root: String,
    from_lez_block_id: u64,
    to_lez_block_id: u64,
    change_count: usize,
    published_at_unix: u64,
    /// Zone SDK `TxStatus` of the transaction, snake case.
    status: &'static str,
    source: Option<&'static str>,
    l1_slot: Option<u64>,
    tx_hash: String,
    msg_id: String,
}

async fn summary(State(state): State<AppState>) -> Json<Value> {
    let registry = state.registry.read().expect("registry lock");
    let served = registry.newest_finalized();
    Json(json!({
        "zone_channel_id": state.channel_id,
        "lez_source": state.lez_source,
        "first_lez_block_id": registry.history.first_lez_block_id,
        "latest_lez_block_id": registry.head.applied_block_id,
        "lez_timestamp_ms": registry.head.applied_timestamp_ms,
        "covered_to_lez_block_id": registry.history.covered_to,
        "live_leaves_at_head": registry.head.live_count(),
        "pending_changes": registry.pending_changes.len(),
        "published_batches": registry.history.published.len(),
        "newest_finalized_root": served.map(|p| hex::encode(p.root_after)),
        "newest_finalized_to_lez_block_id": served.map(|p| p.to_lez_block_id),
        "endpoints": ["/roots/{window_size}", "/merkle_proof/{leaf_index}", "/leaf/{id_commitment}", "/health"],
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
            status: status_name(&p.status),
            source: source_name(&p.status),
            l1_slot: p.l1_slot,
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

/// The Merkle proof path of the leaf at `leaf_index` at the newest Finalized root.
async fn merkle_proof(
    State(state): State<AppState>,
    Path(leaf_index): Path<u64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let registry = state.registry.read().expect("registry lock");
    let Some((published, reader)) = registry.served() else {
        return Err(error(
            StatusCode::SERVICE_UNAVAILABLE,
            "no Finalized root yet",
        ));
    };
    let Some(path) = reader.proof_path(leaf_index) else {
        return Err(error(
            StatusCode::NOT_FOUND,
            "no leaf at this index at the newest Finalized root",
        ));
    };
    Ok(Json(json!({
        "root": hex::encode(published.root_after),
        "to_lez_block_id": published.to_lez_block_id,
        "leaf_index": path.leaf_index,
        "rate_commitment": hex::encode(path.rate_commitment),
        "path_elements": path.path_elements.iter().map(hex::encode).collect::<Vec<_>>(),
        "identity_path_index": path.identity_path_index,
    })))
}

/// Where a membership stands at the LEZ head and at the newest Finalized root.
async fn leaf(
    State(state): State<AppState>,
    Path(id_commitment): Path<String>,
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
    let served = registry.served().map(|(published, reader)| {
        json!({
            "root": hex::encode(published.root_after),
            "to_lez_block_id": published.to_lez_block_id,
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
        "at_newest_finalized_root": served,
    })))
}

fn error(code: StatusCode, message: &str) -> (StatusCode, Json<Value>) {
    (code, Json(json!({ "error": message })))
}
