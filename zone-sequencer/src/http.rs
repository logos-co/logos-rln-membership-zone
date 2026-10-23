//! The HTTP API a Module's node reads (its registry access): the roots with the status of
//! their transaction on Bedrock, the Merkle proof path of a leaf at the tree of the LEZ head,
//! a leaf's status.
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
        .route("/batches/{window_size}", get(batches))
        .route("/merkle_proof/{leaf}", get(merkle_proof))
        .route("/leaves", get(leaves))
        .with_state(state)
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
        "endpoints": ["/roots/{window_size}", "/batches/{window_size}", "/merkle_proof/{leaf_index or id_commitment}", "/leaves", "/health"],
    }))
}

/// The newest `window_size` distinct roots, newest first, each with the newest batch that
/// carries it and that batch's status on Bedrock.
async fn roots(State(state): State<AppState>, Path(window_size): Path<usize>) -> Json<Value> {
    let registry = state.registry.read().expect("registry lock");
    Json(json!({
        "latest_lez_block_id": registry.head.applied_block_id,
        "lez_timestamp_ms": registry.head.applied_timestamp_ms,
        "covered_to_lez_block_id": registry.history.covered_to,
        "roots": entries(registry.roots(window_size)),
    }))
}

/// The newest `window_size` published batches, newest first, heartbeats included.
async fn batches(State(state): State<AppState>, Path(window_size): Path<usize>) -> Json<Value> {
    let registry = state.registry.read().expect("registry lock");
    Json(json!({
        "latest_lez_block_id": registry.head.applied_block_id,
        "lez_timestamp_ms": registry.head.applied_timestamp_ms,
        "covered_to_lez_block_id": registry.history.covered_to,
        "batches": entries(registry.batches(window_size)),
    }))
}

fn entries(published: Vec<&crate::registry::Published>) -> Vec<RootEntry> {
    published
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
        .collect()
}

/// A membership by its leaf index (decimal) or its identity commitment (64 hex): where it
/// stands at the LEZ head and, while its leaf is in the tree, the Merkle proof path at the
/// head's root, with the newest roots a batch carried to Bedrock for a validator to compare.
async fn merkle_proof(
    State(state): State<AppState>,
    Path(leaf): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let registry = state.registry.read().expect("registry lock");
    let found = if let Ok(leaf_index) = leaf.parse::<u64>() {
        registry.head.member_at(leaf_index)
    } else {
        let bytes = hex::decode(leaf.trim_start_matches("0x"))
            .ok()
            .and_then(|v| <[u8; 32]>::try_from(v).ok());
        let Some(id_commitment) = bytes else {
            return Err(error(
                StatusCode::BAD_REQUEST,
                "give a leaf index or an id_commitment of 32 bytes in hex",
            ));
        };
        registry
            .head
            .member(&id_commitment)
            .map(|m| (id_commitment, m))
    };
    let Some((id_commitment, member)) = found else {
        return Err(error(StatusCode::NOT_FOUND, "no such membership"));
    };
    let status = match member.removed {
        None => "live",
        Some(crate::reader::Removed::Expired) => "expired",
        Some(crate::reader::Removed::Slashed) => "slashed",
        Some(crate::reader::Removed::Erased) => "erased",
    };
    let proof = registry.head.proof_path(member.leaf_index).map(|path| {
        json!({
            "rate_commitment": hex::encode(path.rate_commitment),
            "path_elements": path.path_elements.iter().map(hex::encode).collect::<Vec<_>>(),
            "identity_path_index": path.identity_path_index,
        })
    });
    Ok(Json(json!({
        "id_commitment": hex::encode(id_commitment),
        "leaf_index": member.leaf_index,
        "status": status,
        "expiry_timestamp_ms": member.expiry_timestamp_ms,
        "root": hex::encode(registry.head.root()),
        "lez_block_id": registry.head.applied_block_id,
        "lez_timestamp_ms": registry.head.applied_timestamp_ms,
        "newest_adopted": registry.newest_adopted().map(root_ref),
        "newest_finalized": registry.newest_finalized().map(root_ref),
        "proof": proof,
    })))
}

fn root_ref(p: &crate::registry::Published) -> Value {
    json!({ "root": hex::encode(p.root_after), "to_lez_block_id": p.to_lez_block_id })
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
