//! The HTTP API a Module's node reads (its registry access): the roots with the changes that
//! made them and the status of their batches on Bedrock, the Merkle proof path of a leaf at
//! the tree of the LEZ head, a membership's state.
//! 32-byte values are hex, little-endian (RLN-API Appendix B). Every answer carries
//! `lez_block_id` and `lez_timestamp_ms`, the LEZ block the tree is at, and the tree's `root`
//! there; `status` is always a batch's status on Bedrock (`pending`, `adopted`, `orphaned`,
//! `finalized`, with `l1_slot` set at `finalized` and `null` before), a membership is `active`
//! or not. Every key is always present: a value that does not apply is `null`, never absent.

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
    registry::{ChangeRecord, Published, Registry, RootEntry, Status},
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
        .route("/merkle_proof/{leaf}", get(merkle_proof))
        .route("/leaves", get(leaves))
        .with_state(state)
}

/// A published batch as the API shows it: the LEZ blocks it covers, how many changes it
/// listed and where its transaction stands on Bedrock.
#[derive(Serialize)]
struct BatchEntry {
    msg_id: String,
    tx_hash: String,
    from_lez_block_id: u64,
    to_lez_block_id: u64,
    change_count: usize,
    published_at_unix: u64,
    status: &'static str,
    /// The L1 slot of the block that finalized it; `null` before.
    l1_slot: Option<u64>,
}

impl BatchEntry {
    fn new(p: &Published) -> Self {
        let (status, l1_slot) = match p.status {
            Status::Pending => ("pending", None),
            Status::Adopted => ("adopted", None),
            Status::Orphaned => ("orphaned", None),
            Status::Finalized { l1_slot } => ("finalized", Some(l1_slot)),
        };
        Self {
            msg_id: hex::encode(p.msg_id),
            tx_hash: hex::encode(p.tx_hash),
            from_lez_block_id: p.from_lez_block_id,
            to_lez_block_id: p.to_lez_block_id,
            change_count: p.change_count,
            published_at_unix: p.published_at_unix,
            status,
            l1_slot,
        }
    }
}

/// A root of the valid-root window as the API shows it: the batch that produced it (its
/// fields at the top, its `changes` what made the root), `carried_to_lez_block_id`, the last
/// LEZ block the root stood for, and `batches`, every batch of the run that carried it, the
/// producing one first.
#[derive(Serialize)]
struct RootWindowEntry {
    root: String,
    #[serde(flatten)]
    produced_by: BatchEntry,
    carried_to_lez_block_id: u64,
    changes: Vec<ChangeRecord>,
    batches: Vec<BatchEntry>,
}

impl RootWindowEntry {
    fn new(entry: &RootEntry<'_>) -> Self {
        let produced_by = entry.produced_by();
        Self {
            root: hex::encode(produced_by.root_after),
            produced_by: BatchEntry::new(produced_by),
            carried_to_lez_block_id: entry.carried_to_lez_block_id(),
            changes: produced_by.changes.clone(),
            batches: entry.batches.iter().map(|p| BatchEntry::new(p)).collect(),
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
            "endpoints": ["/roots/{window_size}", "/merkle_proof/{leaf_index or id_commitment}", "/leaves", "/health"],
        }),
    )
}

/// The newest `window_size` roots, newest first: each with the batch that produced it, the
/// changes that made it, the last LEZ block it stood for and every batch that carried it. A
/// validator reads its policy off this: the roots back to the newest one whose `changes`
/// remove a leaf, say, are the ones no removed member can still prove under.
async fn roots(State(state): State<AppState>, Path(window_size): Path<usize>) -> Json<Value> {
    let registry = state.registry.read().expect("registry lock");
    let roots: Vec<_> = registry
        .roots(window_size)
        .iter()
        .map(RootWindowEntry::new)
        .collect();
    with_head(
        &registry,
        json!({
            "covered_to_lez_block_id": registry.history.covered_to,
            "roots": roots,
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
