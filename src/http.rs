//! The HTTP API a Module's node reads (its registry access): the roots with the changes that
//! made them and the status of their batches on Bedrock, the Merkle proof path of a leaf at
//! the tree of the LEZ head, a membership's state.
//! 32-byte values are hex, little-endian (RLN-API Appendix B). Every answer carries `head`:
//! `lez_block_id` and `lez_timestamp_ms`, the LEZ block the tree is at, and the tree's `root`
//! there; `status` is always a batch's status on Bedrock (`pending`, `adopted`, `orphaned`,
//! `finalized`, with `l1_slot` set at `finalized` and `null` before), a membership is `active`
//! or not. Every key is always present: a value that does not apply is `null`, never absent.
//! The one exception is `mode`: the indexer's verdicts (`verification`, `rejection`,
//! `verified_batches`, `rejected_batches`, `replay_root`) exist only in its answers.

use std::sync::{Arc, RwLock};

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::get,
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::registry::{ChangeRecord, Published, Registry, RootEntry, Status, Verification};

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

/// A published batch as the API shows it: the LEZ blocks it covers, when it went out and
/// where its transaction stands on Bedrock.
#[derive(Serialize)]
struct BatchEntry {
    msg_id: String,
    tx_hash: String,
    from_lez_block_id: u64,
    to_lez_block_id: u64,
    /// The sequencer's clock at publish.
    published_at_unix: u64,
    /// The L1 slot at publish, the chain's clock; `null` when the node did not answer then or
    /// the batch predates the record.
    published_at_slot: Option<u64>,
    status: &'static str,
    /// The L1 slot of the block that finalized it; `null` before.
    l1_slot: Option<u64>,
    /// An indexer's verdict; a sequencer checks nothing of its own and has no such keys.
    #[serde(flatten)]
    verdict: Option<Verdict>,
}

#[derive(Serialize)]
struct Verdict {
    /// `verified`, `rejected` or `pending`.
    verification: &'static str,
    /// Why the batch was rejected; `null` otherwise.
    rejection: Option<String>,
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
            published_at_unix: p.published_at_unix,
            published_at_slot: p.published_at_slot,
            status,
            l1_slot,
            verdict: p.verification.as_ref().map(|v| match v {
                Verification::Verified => Verdict {
                    verification: "verified",
                    rejection: None,
                },
                Verification::Pending => Verdict {
                    verification: "pending",
                    rejection: None,
                },
                Verification::Rejected(reason) => Verdict {
                    verification: "rejected",
                    rejection: Some(reason.clone()),
                },
            }),
        }
    }
}

/// A root of the valid-root window as the API shows it: the LEZ blocks it stood for
/// (`from_lez_block_id` of the batch that produced it to `to_lez_block_id` of the newest batch
/// carrying it), the `changes` that made it, and `batches`, every batch of the run, the
/// producing one first with its status on Bedrock.
#[derive(Serialize)]
struct RootWindowEntry {
    root: String,
    from_lez_block_id: u64,
    to_lez_block_id: u64,
    changes: Vec<ChangeRecord>,
    batches: Vec<BatchEntry>,
}

impl RootWindowEntry {
    /// The root at the LEZ head, ahead of any batch: the changes not published yet (the
    /// sequencer) or not read from the channel yet (the indexer). `None` while no change
    /// waits, the head's root being the newest batch's then.
    fn head(registry: &Registry) -> Option<Self> {
        if registry.pending_changes.is_empty() {
            return None;
        }
        Some(Self {
            root: hex::encode(registry.head.root()),
            from_lez_block_id: registry.history.covered_to() + 1,
            to_lez_block_id: registry.head.lez_block_id,
            changes: registry
                .pending_changes
                .iter()
                .filter_map(|(_, change)| ChangeRecord::from_change(change))
                .collect(),
            batches: vec![],
        })
    }

    fn new(entry: &RootEntry<'_>) -> Self {
        let produced_by = entry.produced_by();
        Self {
            root: hex::encode(produced_by.root_after),
            from_lez_block_id: produced_by.from_lez_block_id,
            to_lez_block_id: entry.carried_to_lez_block_id(),
            changes: produced_by.changes.clone(),
            batches: entry.batches.iter().map(|p| BatchEntry::new(p)).collect(),
        }
    }
}

/// `body` plus `head`, where the tree is: `lez_block_id` (the LEZ block applied last),
/// `lez_timestamp_ms` and the `root` there.
fn with_head(registry: &Registry, mut body: Value) -> Json<Value> {
    body["head"] = json!({
        "lez_block_id": registry.head.lez_block_id,
        "lez_timestamp_ms": registry.head.lez_timestamp_ms,
        "root": hex::encode(registry.head.root()),
    });
    Json(body)
}

async fn summary(State(state): State<AppState>) -> Json<Value> {
    let registry = state.registry.read().expect("registry lock");
    let mut body = json!({
        "zone_channel_id": state.channel_id,
        "lez_source": state.lez_source,
        "first_lez_block_id": registry.history.first_lez_block_id,
        "lez_events": registry.head.stats.lez_events(),
        "private_transaction_count": registry.head.stats.private_transaction_count,
        "tree": {
            "active_leaf_count": registry.head.active_count(),
            "removed_leaf_count": registry.head.removed_leaves().len(),
            "changes": registry.head.stats.tree_changes(),
        },
        "mode": state.mode,
        "pending_change_count": registry.pending_changes.len(),
        "published_batch_count": registry.history.published.len(),
        "newest_published": registry.newest_published().map(root_ref),
        "newest_adopted": registry.newest_adopted().map(root_ref),
        "newest_finalized": registry.newest_finalized().map(root_ref),
        "endpoints": ["/roots/{window_size}", "/merkle_proof/{leaf_index or id_commitment}", "/leaves", "/health"],
    });
    if state.mode == "indexer" {
        let published = &registry.history.published;
        body["verified_batch_count"] = published
            .iter()
            .filter(|p| p.verification == Some(Verification::Verified))
            .count()
            .into();
        body["rejected_batch_count"] = published
            .iter()
            .filter(|p| matches!(p.verification, Some(Verification::Rejected(_))))
            .count()
            .into();
        body["replay_root"] = registry.replay_root().map(hex::encode).into();
    }
    with_head(&registry, body)
}

/// The newest `window_size` roots, newest first: the head's when it is ahead of the batches
/// (`batches` empty), then each root with the changes that made it, the LEZ blocks it stood
/// for and every batch that carried it. A validator reads its policy off this: the roots back
/// to the newest one whose `changes` remove a leaf, say, are the ones no removed member can
/// still prove under.
async fn roots(State(state): State<AppState>, Path(window_size): Path<usize>) -> Json<Value> {
    let registry = state.registry.read().expect("registry lock");
    let roots: Vec<_> = RootWindowEntry::head(&registry)
        .into_iter()
        .chain(registry.roots(window_size).iter().map(RootWindowEntry::new))
        .take(window_size)
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
            "registered_at_lez_block_id": member.registered_at_lez_block_id,
            "removed": member.removed,
            "removed_at_lez_block_id": member.removed_at_lez_block_id,
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
        .map(|(id, m)| {
            json!({
                "leaf_index": m.leaf_index,
                "id_commitment": hex::encode(id),
                "registered_at_lez_block_id": m.registered_at_lez_block_id,
                "expiry_timestamp_ms": m.expiry_timestamp_ms,
                "expires_in_ms": m.expiry_timestamp_ms.saturating_sub(registry.head.lez_timestamp_ms),
            })
        })
        .collect();
    let removed: Vec<Value> = registry
        .head
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
