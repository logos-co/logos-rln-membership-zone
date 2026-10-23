//! The HTTP API a Module's node reads (its registry access): the roots with the status of
//! their transaction on Bedrock, the Merkle proof path of a leaf at the tree of the LEZ head,
//! a membership's state.
//! 32-byte values are hex, little-endian (RLN-API Appendix B). Every answer carries
//! `lez_block_id` and `lez_timestamp_ms`, the LEZ block the tree is at, and the tree's `root`
//! there; `status` is always a batch's status on Bedrock (`pending`, `adopted`, `orphaned`,
//! `finalized` with `l1_slot`), a membership is `active` or not.

use std::sync::{Arc, RwLock};

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::get,
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    reader::Removed,
    registry::{Published, Registry, RootEntry, Status},
};

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

/// A published batch as the API shows it: the root it carried, the LEZ blocks it covers, how
/// many changes it listed and where its transaction stands on Bedrock. In `/roots`,
/// `carried_to_lez_block_id` is added: the last LEZ block the root stood for.
#[derive(Serialize)]
struct BatchEntry {
    root: String,
    from_lez_block_id: u64,
    to_lez_block_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    carried_to_lez_block_id: Option<u64>,
    change_count: usize,
    published_at_unix: u64,
    #[serde(flatten)]
    status: Status,
    tx_hash: String,
    msg_id: String,
}

impl BatchEntry {
    fn new(p: &Published) -> Self {
        Self {
            root: hex::encode(p.root_after),
            from_lez_block_id: p.from_lez_block_id,
            to_lez_block_id: p.to_lez_block_id,
            carried_to_lez_block_id: None,
            change_count: p.change_count,
            published_at_unix: p.published_at_unix,
            status: p.status.clone(),
            tx_hash: hex::encode(p.tx_hash),
            msg_id: hex::encode(p.msg_id),
        }
    }

    fn root(entry: RootEntry<'_>) -> Self {
        Self {
            carried_to_lez_block_id: Some(entry.carried_to_lez_block_id),
            ..Self::new(entry.produced_by)
        }
    }
}

/// `body` plus where the tree is: `lez_block_id` (the LEZ block applied last),
/// `lez_timestamp_ms` and the `root` there.
fn with_head(registry: &Registry, mut body: Value) -> Json<Value> {
    if let Value::Object(map) = &mut body {
        map.insert("lez_block_id".into(), registry.head.applied_block_id.into());
        map.insert(
            "lez_timestamp_ms".into(),
            registry.head.applied_timestamp_ms.into(),
        );
        map.insert("root".into(), hex::encode(registry.head.root()).into());
    }
    Json(body)
}

async fn summary(State(state): State<AppState>) -> Json<Value> {
    let registry = state.registry.read().expect("registry lock");
    with_head(
        &registry,
        json!({
            "zone_channel_id": state.channel_id,
            "lez_source": state.lez_source,
            "first_lez_block_id": registry.history.first_lez_block_id,
            "covered_to_lez_block_id": registry.history.covered_to,
            "lez_events": registry.head.stats.lez_events(),
            "tree": {
                "active_leaves": registry.head.active_count(),
                "removed_leaves": registry.head.removed_leaves().len(),
                "changes": registry.head.stats.tree_changes(),
            },
            "pending_changes": registry.pending_changes.len(),
            "published_batches": registry.history.published.len(),
            "newest_adopted": registry.newest_adopted().map(root_ref),
            "newest_finalized": registry.newest_finalized().map(root_ref),
            "endpoints": ["/roots/{window_size}", "/batches/{window_size}", "/merkle_proof/{leaf_index or id_commitment}", "/leaves", "/health"],
        }),
    )
}

/// The newest `window_size` distinct roots, newest first, each with the batch that produced
/// it (its status on Bedrock, its `change_count`) and the last LEZ block it stood for.
async fn roots(State(state): State<AppState>, Path(window_size): Path<usize>) -> Json<Value> {
    let registry = state.registry.read().expect("registry lock");
    let roots: Vec<_> = registry
        .roots(window_size)
        .into_iter()
        .map(BatchEntry::root)
        .collect();
    with_head(
        &registry,
        json!({
            "covered_to_lez_block_id": registry.history.covered_to,
            "roots": roots,
        }),
    )
}

/// The newest `window_size` published batches, newest first, heartbeats included.
async fn batches(State(state): State<AppState>, Path(window_size): Path<usize>) -> Json<Value> {
    let registry = state.registry.read().expect("registry lock");
    let batches: Vec<_> = registry
        .batches(window_size)
        .into_iter()
        .map(BatchEntry::new)
        .collect();
    with_head(
        &registry,
        json!({
            "covered_to_lez_block_id": registry.history.covered_to,
            "batches": batches,
        }),
    )
}

/// A membership by its leaf index (decimal) or its identity commitment (64 hex): whether it
/// is `active` (its leaf is in the tree) or why it was `removed`, and while active the Merkle
/// proof path at the head's root, with the newest roots a batch carried to Bedrock for a
/// validator to compare.
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
    let proof = registry.head.proof_path(member.leaf_index).map(|path| {
        json!({
            "rate_commitment": hex::encode(path.rate_commitment),
            "path_elements": path.path_elements.iter().map(hex::encode).collect::<Vec<_>>(),
            "identity_path_index": path.identity_path_index,
        })
    });
    Ok(with_head(
        &registry,
        json!({
            "id_commitment": hex::encode(id_commitment),
            "leaf_index": member.leaf_index,
            "active": member.active,
            "removed": member.removed,
            "expiry_timestamp_ms": member.expiry_timestamp_ms,
            "newest_adopted": registry.newest_adopted().map(root_ref),
            "newest_finalized": registry.newest_finalized().map(root_ref),
            "proof": proof,
        }),
    ))
}

fn root_ref(p: &Published) -> Value {
    json!({ "root": hex::encode(p.root_after), "to_lez_block_id": p.to_lez_block_id })
}

/// Every membership at the LEZ head: the active leaves with their expiry, the removed ones
/// with why (`expired`, `slashed`, `erased`).
async fn leaves(State(state): State<AppState>) -> Json<Value> {
    let registry = state.registry.read().expect("registry lock");
    let active: Vec<Value> = registry
        .head
        .active_leaves()
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
        .map(|(leaf_index, id, cause): (u64, [u8; 32], Removed)| {
            json!({
                "leaf_index": leaf_index,
                "id_commitment": hex::encode(id),
                "removed": cause,
            })
        })
        .collect();
    with_head(
        &registry,
        json!({
            "active": active,
            "removed": removed,
        }),
    )
}

fn error(code: StatusCode, message: &str) -> (StatusCode, Json<Value>) {
    (code, Json(json!({ "error": message })))
}
