//! The HTTP API a Module's node reads (its registry access): the valid roots with the batches
//! that carry them, the Merkle proof path of a leaf at the tree of the LEZ head, a membership's
//! state. 32-byte values are hex, little-endian (RLN-API Appendix B). Every answer carries
//! `head`: `lez_block_id` and `lez_timestamp_ms`, the LEZ block the tree is at, and the tree's
//! `root` there. Where a batch stands on Bedrock is its `bedrock_status`, the Zone SDK's word:
//! `pending` (published, no Bedrock block yet), `adopted` (in a block on the current branch),
//! `finalized` (with the `l1_slot`). Only the batches that applied are served; the first that
//! did not is the `stall` in `/` (LEZ `StallReason`). Every key is always present: a value
//! that does not apply is `null`, never absent.

use std::sync::{Arc, RwLock};

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::get,
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::registry::{EntryView, Registry, RootEntry};

pub type Shared = Arc<RwLock<Registry>>;

#[derive(Clone)]
pub struct AppState {
    pub registry: Shared,
    pub channel_id: String,
    pub lez_source: String,
    /// `sequencer` or `indexer`.
    pub mode: String,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(summary))
        .route("/health", get(|| async { "ok" }))
        .route("/roots/{window_size}", get(roots))
        .route("/merkle_proof/{leaf}", get(merkle_proof))
        .route("/leaves", get(leaves))
        .with_state(state)
}

/// A root of the valid-root window as the API shows it: the LEZ blocks it stood for
/// (`from_lez_block_id` of the batch that produced it to `to_lez_block_id` of the newest batch
/// carrying it), and `batches`, every batch of the run, the producing one first (its `changes`
/// made the root; the ones after it carry it further with none), each with its own span.
#[derive(Serialize)]
struct RootWindowEntry {
    root: String,
    from_lez_block_id: u64,
    to_lez_block_id: u64,
    batches: Vec<EntryView>,
}

impl RootWindowEntry {
    fn new(run: RootEntry) -> Self {
        let produced_by = run.produced_by();
        Self {
            root: produced_by.root.clone().unwrap_or_default(),
            from_lez_block_id: produced_by.from_lez_block_id.unwrap_or_default(),
            to_lez_block_id: run.carried_to_lez_block_id(),
            batches: run.entries,
        }
    }
}

/// `body` plus `head`, where the tree is: `lez_block_id` (the LEZ block applied last),
/// `lez_timestamp_ms` and the `root` there.
fn with_head(registry: &Registry, mut body: Value) -> Json<Value> {
    body["head"] = json!({
        "lez_block_id": registry.head().lez_block_id,
        "lez_timestamp_ms": registry.head().lez_timestamp_ms,
        "root": hex::encode(registry.head().root()),
    });
    Json(body)
}

fn root_ref(entry: Option<EntryView>) -> Value {
    match entry {
        Some(e) => json!({ "root": e.root, "to_lez_block_id": e.to_lez_block_id }),
        None => Value::Null,
    }
}

async fn summary(State(state): State<AppState>) -> Json<Value> {
    let registry = state.registry.read().expect("registry lock");
    let applied = registry
        .entries()
        .iter()
        .filter(|e| e.outcome == "applied")
        .count();
    let body = json!({
        "zone_channel_id": state.channel_id,
        "lez_source": state.lez_source,
        "first_lez_block_id": registry.first_lez_block_id,
        "lez_events": registry.head().stats.lez_events(),
        "private_transaction_count": registry.head().stats.private_transaction_count,
        "tree": {
            "active_leaf_count": registry.head().active_count(),
            "removed_leaf_count": registry.head().removed_leaves().len(),
            "changes": registry.head().stats.tree_changes(),
        },
        "mode": state.mode,
        "pending_change_count": registry.pending_changes.len(),
        "batch_count": applied,
        "stall": registry.zone.stall(),
        "in_flight": registry.in_flight().map(hex::encode),
        "lez": {
            "adopted_block_id": registry.head().lez_block_id,
            "adopted_block_count": registry.lez.view().len(),
            "finalized_block_id": registry.lez.final_reader().lez_block_id,
        },
        "orphaned_count": registry.orphaned_count(),
        "newest_published": root_ref(registry.newest(false)),
        "newest_finalized": root_ref(registry.newest(true)),
        "endpoints": [
            "/roots/{window_size}",
            "/merkle_proof/{leaf_index or id_commitment}",
            "/leaves",
            "/health"
        ],
    });
    with_head(&registry, body)
}

/// The newest `window_size` roots a batch carries, newest first, each with every batch of its
/// run (the head's own root, ahead of the batches while changes wait, is `head.root`). A
/// validator reads its policy off the producing batch's `changes`: the roots back to the
/// newest one that removes a leaf, say, are the ones no removed member can still prove under.
async fn roots(State(state): State<AppState>, Path(window_size): Path<usize>) -> Json<Value> {
    let registry = state.registry.read().expect("registry lock");
    let roots: Vec<_> = registry
        .roots(window_size)
        .into_iter()
        .map(RootWindowEntry::new)
        .collect();
    with_head(&registry, json!({ "roots": roots }))
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
        registry.head().member_at(leaf_index)
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
            .head()
            .member(&id_commitment)
            .map(|m| (id_commitment, m))
    };
    let Some((id_commitment, member)) = found else {
        return Err(error(StatusCode::NOT_FOUND, "no such membership"));
    };
    let proof = registry.head().proof_path(member.leaf_index).map(|path| {
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
            "registered_at_lez_block_id": member.registered_at_lez_block_id,
            "removed": member.removed,
            "removed_at_lez_block_id": member.removed_at_lez_block_id,
            "expiry_timestamp_ms": member.expiry_timestamp_ms,
            "newest_published": root_ref(registry.newest(false)),
            "newest_finalized": root_ref(registry.newest(true)),
            "proof": proof,
        }),
    ))
}

/// Every membership at the LEZ head: the active leaves with their expiry, the removed ones
/// with why (`expired`, `slashed`, `erased`).
async fn leaves(State(state): State<AppState>) -> Json<Value> {
    let registry = state.registry.read().expect("registry lock");
    let active: Vec<Value> = registry
        .head()
        .active_leaves()
        .into_iter()
        .map(|(id, m)| {
            json!({
                "leaf_index": m.leaf_index,
                "id_commitment": hex::encode(id),
                "registered_at_lez_block_id": m.registered_at_lez_block_id,
                "expiry_timestamp_ms": m.expiry_timestamp_ms,
                "expires_in_ms": m
                    .expiry_timestamp_ms
                    .saturating_sub(registry.head().lez_timestamp_ms),
            })
        })
        .collect();
    let removed: Vec<Value> = registry
        .head()
        .removed_leaves()
        .into_iter()
        .map(|(id, m)| {
            json!({
                "leaf_index": m.leaf_index,
                "id_commitment": hex::encode(id),
                "registered_at_lez_block_id": m.registered_at_lez_block_id,
                "removed": m.removed,
                "removed_at_lez_block_id": m.removed_at_lez_block_id,
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
