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

use crate::{
    batch::{Batch, Change, Kind},
    lez::{self, ProgramEvent, RlnEvent},
    registry::{BedrockStatus, Registry},
    zone_chain::{BatchIngestError, Tip},
};

pub type Shared = Arc<RwLock<Registry>>;

/// One tree change a batch carried, as the API shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChangeRecord {
    pub kind: ChangeKind,
    pub leaf_index: u64,
    /// The leaf inserted, 32 bytes little-endian in hex; `None` for a removal.
    pub rate_commitment: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Insert,
    Remove,
}

impl ChangeRecord {
    fn from_change(change: &Change) -> Option<Self> {
        Some(match change.kind.as_ref()? {
            Kind::Insert(insert) => Self {
                kind: ChangeKind::Insert,
                leaf_index: insert.leaf_index,
                rate_commitment: Some(hex::encode(&insert.rate_commitment)),
            },
            Kind::Remove(remove) => Self {
                kind: ChangeKind::Remove,
                leaf_index: remove.leaf_index,
                rate_commitment: None,
            },
        })
    }
}

/// A channel entry as the API shows it: its span, root and changes when it carries a batch,
/// its `bedrock_status` and, finalized, the L1 slot of the block that finalized it. The fold's
/// error stays internal (LEZ `BatchIngestError`): the API serves the applied entries and the
/// `stall`.
#[derive(Debug, Clone, Serialize)]
pub struct EntryView {
    pub msg_id: String,
    pub parent_msg: String,
    /// `None` for an entry that carries no batch.
    pub from_lez_block_id: Option<u64>,
    pub to_lez_block_id: Option<u64>,
    pub root: Option<String>,
    pub changes: Vec<ChangeRecord>,
    pub bedrock_status: BedrockStatus,
    /// The L1 slot of the block that finalized it; `null` before.
    pub l1_slot: Option<u64>,
    /// Why its batch did not apply, when it did not (retryable: not judged yet).
    #[serde(skip)]
    pub error: Option<BatchIngestError>,
}

impl EntryView {
    fn new(
        msg: [u8; 32],
        parent: [u8; 32],
        batch: Option<&Batch>,
        bedrock_status: BedrockStatus,
        l1_slot: Option<u64>,
        error: Option<&BatchIngestError>,
    ) -> Self {
        Self {
            msg_id: hex::encode(msg),
            parent_msg: hex::encode(parent),
            from_lez_block_id: batch.map(|b| b.from_lez_block_id),
            to_lez_block_id: batch.map(|b| b.to_lez_block_id),
            root: batch.map(|b| hex::encode(Tip::from(b).root)),
            changes: batch.map_or_else(Vec::new, |b| {
                b.changes
                    .iter()
                    .filter_map(ChangeRecord::from_change)
                    .collect()
            }),
            bedrock_status,
            l1_slot,
            // An entry without a batch has nothing to apply and nothing to park on.
            error: error.filter(|_| batch.is_some()).cloned(),
        }
    }

    /// Whether the entry carries a batch the fold applied (LEZ `AcceptOutcome::Applied`).
    pub fn applied(&self) -> bool {
        self.root.is_some() && self.error.is_none()
    }
}

/// Every entry of the channel, final tier first then the view, as the API shows them.
pub fn entries(registry: &Registry) -> Vec<EntryView> {
    let mut out: Vec<EntryView> = registry
        .zone
        .final_entries()
        .iter()
        .map(|f| {
            EntryView::new(
                f.msg,
                f.parent,
                f.batch().as_ref(),
                BedrockStatus::Finalized,
                Some(f.l1_slot),
                f.error.as_ref(),
            )
        })
        .collect();
    for (entry, outcome) in registry.zone.view_outcomes() {
        let msg: [u8; 32] = entry.msg.into();
        let status = if registry.is_pending(&msg) {
            BedrockStatus::Pending
        } else {
            BedrockStatus::Adopted
        };
        out.push(EntryView::new(
            msg,
            entry.parent.into(),
            entry.batch.as_ref(),
            status,
            None,
            outcome.as_ref().err(),
        ));
    }
    out
}

/// The batches carrying one root, the one that produced it first (its changes made the root).
pub struct RootEntry {
    pub entries: Vec<EntryView>,
}

impl RootEntry {
    pub fn produced_by(&self) -> &EntryView {
        &self.entries[0]
    }

    pub fn carried_to_lez_block_id(&self) -> u64 {
        self.entries
            .iter()
            .filter_map(|e| e.to_lez_block_id)
            .max()
            .unwrap_or_default()
    }
}

/// The roots newest first, at most `window_size`: one entry per run of applied batches
/// carrying the same root, the batch that produced it first and every batch of the run after
/// it (heartbeats as a rule). A root the tree comes back to later is a new run.
pub fn roots_of(registry: &Registry, window_size: usize) -> Vec<RootEntry> {
    let mut runs: Vec<RootEntry> = vec![];
    for entry in entries(registry).into_iter().filter(EntryView::applied) {
        match runs.last_mut() {
            Some(run) if run.produced_by().root == entry.root => run.entries.push(entry),
            _ => runs.push(RootEntry {
                entries: vec![entry],
            }),
        }
    }
    runs.reverse();
    runs.truncate(window_size);
    runs
}

/// The newest applied entry, a finalized one only when `finalized_only`.
pub fn newest(registry: &Registry, finalized_only: bool) -> Option<EntryView> {
    entries(registry)
        .into_iter()
        .rev()
        .find(|e| e.applied() && (e.bedrock_status == BedrockStatus::Finalized || !finalized_only))
}

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
    let applied = entries(&registry).iter().filter(|e| e.applied()).count();
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
            "sequencer_block_id": registry.sequencer_block_id(),
            "not_on_bedrock": registry
                .sequencer_block_id()
                .map(|n| n.saturating_sub(registry.head().lez_block_id)),
        },
        "orphaned_count": registry.orphaned_count(),
        "newest_published": root_ref(newest(&registry, false)),
        "newest_finalized": root_ref(newest(&registry, true)),
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
    let roots: Vec<_> = roots_of(&registry, window_size)
        .into_iter()
        .map(RootWindowEntry::new)
        .collect();
    with_head(&registry, json!({ "roots": roots }))
}

/// What the LEZ Sequencer holds about a membership ahead of Bedrock, as the API shows it: the
/// newest event about it in a LEZ block not on Bedrock yet (`null` when none). A preview: it
/// becomes the membership's state once Bedrock adopts that block, which may take minutes, or
/// hours when LEZ stops inscribing.
fn lez_preview(found: Option<(u64, &ProgramEvent)>) -> Value {
    match found {
        Some((lez_block_id, event)) => json!({
            "lez_block_id": lez_block_id,
            "event": lez::selector_name(&event.event),
            "tx_hash": hex::encode(event.tx_hash),
        }),
        None => Value::Null,
    }
}

/// A membership by its leaf index (decimal) or its identity commitment (64 hex): whether it
/// is `active` (its leaf is in the tree) or why it was `removed`, and while active the Merkle
/// proof path at the head's root, with the newest roots a batch carried to Bedrock for a
/// validator to compare. `lez_preview` says what the LEZ Sequencer holds about it ahead of
/// Bedrock; a membership only the preview knows answers with `active` false and no `proof`.
async fn merkle_proof(
    State(state): State<AppState>,
    Path(leaf): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let registry = state.registry.read().expect("registry lock");
    let (found, preview) = if let Ok(leaf_index) = leaf.parse::<u64>() {
        (
            registry.head().member_at(leaf_index),
            registry.preview_for_leaf(leaf_index),
        )
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
        (
            registry
                .head()
                .member(&id_commitment)
                .map(|m| (id_commitment, m)),
            registry.preview_for_id(&id_commitment),
        )
    };
    let Some((id_commitment, member)) = found else {
        let Some((_, event)) = preview else {
            return Err(error(StatusCode::NOT_FOUND, "no such membership"));
        };
        let leaf_index = match &event.event {
            RlnEvent::Registered(r) => Some(r.leaf_index),
            _ => None,
        };
        return Ok(with_head(
            &registry,
            json!({
                "id_commitment": hex::encode(crate::registry::event_id_commitment(&event.event)),
                "leaf_index": leaf_index,
                "active": false,
                "registered_at_lez_block_id": null,
                "registered_tx_hash": null,
                "removed": null,
                "removed_at_lez_block_id": null,
                "removed_tx_hash": null,
                "expiry_timestamp_ms": null,
                "newest_published": root_ref(newest(&registry, false)),
                "newest_finalized": root_ref(newest(&registry, true)),
                "proof": null,
                "lez_preview": lez_preview(preview),
            }),
        ));
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
            "registered_tx_hash": hex::encode(member.registered_tx_hash),
            "removed": member.removed,
            "removed_at_lez_block_id": member.removed_at_lez_block_id,
            "removed_tx_hash": member.removed_tx_hash.map(hex::encode),
            "expiry_timestamp_ms": member.expiry_timestamp_ms,
            "newest_published": root_ref(newest(&registry, false)),
            "newest_finalized": root_ref(newest(&registry, true)),
            "proof": proof,
            "lez_preview": lez_preview(preview),
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
                "registered_tx_hash": hex::encode(m.registered_tx_hash),
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
                "registered_tx_hash": hex::encode(m.registered_tx_hash),
                "removed": m.removed,
                "removed_at_lez_block_id": m.removed_at_lez_block_id,
                "removed_tx_hash": m.removed_tx_hash.map(hex::encode),
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

#[cfg(test)]
mod test {
    use lb_core::mantle::ops::channel::MsgId;

    use super::*;
    use crate::{
        lez::{Block, BlockHeader, ProgramEvent, Registered, RlnEvent},
        zone_chain::ChannelEntry,
    };

    fn block(block_id: u64, events: Vec<ProgramEvent>) -> Block {
        Block {
            header: BlockHeader {
                block_id,
                prev_block_hash: common::HashType([block_id as u8 - 1; 32]),
                hash: common::HashType([block_id as u8; 32]),
                timestamp: 1_000 * block_id,
            },
            events,
            private_transaction_count: 0,
        }
    }

    fn registered(n: u8, leaf_index: u64) -> ProgramEvent {
        let mut id_commitment = [0u8; 32];
        id_commitment[0] = n;
        ProgramEvent::from(RlnEvent::Registered(Registered {
            tree_id: [0; 32],
            id_commitment,
            rate_limit: 100,
            expiry_timestamp_ms: u64::MAX,
            leaf_index,
        }))
    }

    /// A sequencer that registered leaf 0 in LEZ block 1, published it (finalized on Bedrock
    /// at slot 500), then a heartbeat over block 2 (adopted) and one over block 3 (in flight).
    fn state() -> AppState {
        let mut registry = Registry::open_temporary(1, false).unwrap();
        registry
            .apply_lez_adopted(&block(1, vec![registered(1, 0)]))
            .unwrap();
        let first = registry.next_batch().unwrap();
        registry.record_publish(MsgId::from([1; 32]), MsgId::root(), &first);
        registry.mark_adopted_up_to([1; 32]);
        registry.apply_zone_finalized(
            ChannelEntry {
                msg: MsgId::from([1; 32]),
                parent: MsgId::root(),
                batch: Some(first),
            },
            500,
        );
        registry.apply_lez_adopted(&block(2, vec![])).unwrap();
        let second = registry.next_batch().unwrap();
        registry.record_publish(MsgId::from([2; 32]), MsgId::from([1; 32]), &second);
        registry.mark_adopted_up_to([2; 32]);
        registry.apply_lez_adopted(&block(3, vec![])).unwrap();
        let third = registry.next_batch().unwrap();
        registry.record_publish(MsgId::from([3; 32]), MsgId::from([2; 32]), &third);
        AppState {
            registry: Arc::new(RwLock::new(registry)),
            channel_id: "aa".repeat(32),
            lez_source: "test".into(),
            mode: "sequencer".into(),
        }
    }

    #[test]
    fn test_entries_roots_and_newest_follow_the_lineage() {
        let state = state();
        let registry = state.registry.read().unwrap();
        let all = entries(&registry);
        assert_eq!(all.len(), 3);
        assert_eq!(
            all.iter().map(|e| e.bedrock_status).collect::<Vec<_>>(),
            vec![
                BedrockStatus::Finalized,
                BedrockStatus::Adopted,
                BedrockStatus::Pending
            ]
        );
        assert_eq!(all[0].l1_slot, Some(500));
        assert_eq!(all[0].changes.len(), 1);
        assert_eq!(all[0].changes[0].kind, ChangeKind::Insert);
        assert!(all.iter().all(EntryView::applied));
        // One root, carried by three batches to block 3.
        let roots = roots_of(&registry, 5);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].entries.len(), 3);
        assert_eq!(roots[0].produced_by().from_lez_block_id, Some(1));
        assert_eq!(roots[0].carried_to_lez_block_id(), 3);
        assert_eq!(
            newest(&registry, false).unwrap().msg_id,
            hex::encode([3; 32])
        );
        assert_eq!(
            newest(&registry, true).unwrap().msg_id,
            hex::encode([1; 32])
        );
    }

    #[tokio::test]
    async fn test_the_handlers_answer_from_the_head() {
        let state = state();
        let Json(overview) = summary(State(state.clone())).await;
        assert_eq!(overview["batch_count"], 3);
        assert_eq!(overview["in_flight"], json!(hex::encode([3; 32])));
        assert_eq!(overview["lez"]["adopted_block_id"], 3);
        assert_eq!(overview["lez"]["finalized_block_id"], 0);
        assert_eq!(overview["tree"]["active_leaf_count"], 1);
        assert!(overview["stall"].is_null());
        assert_eq!(overview["head"]["lez_block_id"], 3);
        assert_eq!(overview["newest_finalized"]["to_lez_block_id"], 1);

        let Json(roots) = roots(State(state.clone()), Path(5)).await;
        assert_eq!(roots["roots"].as_array().unwrap().len(), 1);
        assert_eq!(roots["roots"][0]["from_lez_block_id"], 1);
        assert_eq!(roots["roots"][0]["to_lez_block_id"], 3);
        assert_eq!(roots["roots"][0]["root"], overview["head"]["root"]);
        assert_eq!(
            roots["roots"][0]["batches"][0]["bedrock_status"],
            "finalized"
        );
        assert!(roots["roots"][0]["batches"][0].get("error").is_none());

        let id = hex::encode({
            let mut id = [0u8; 32];
            id[0] = 1;
            id
        });
        let Json(by_index) = merkle_proof(State(state.clone()), Path("0".into()))
            .await
            .unwrap();
        assert_eq!(by_index["id_commitment"], id);
        assert_eq!(by_index["active"], true);
        assert_eq!(
            by_index["proof"]["path_elements"].as_array().unwrap().len(),
            20
        );
        let Json(by_id) = merkle_proof(State(state.clone()), Path(id)).await.unwrap();
        assert_eq!(by_id["leaf_index"], 0);
        assert!(by_id["lez_preview"].is_null());
        assert_eq!(by_id["registered_tx_hash"], json!("00".repeat(32)));
        let (code, _) = merkle_proof(State(state.clone()), Path("7".into()))
            .await
            .unwrap_err();
        assert_eq!(code, StatusCode::NOT_FOUND);
        let (code, _) = merkle_proof(State(state.clone()), Path("zz".into()))
            .await
            .unwrap_err();
        assert_eq!(code, StatusCode::BAD_REQUEST);

        let Json(leaves) = leaves(State(state.clone())).await;
        assert_eq!(leaves["active"].as_array().unwrap().len(), 1);
        assert_eq!(leaves["removed"].as_array().unwrap().len(), 0);

        // The LEZ Sequencer produced block 4 with leaf 7 registered, not on Bedrock yet: the
        // membership answers from the preview alone, the summary shows the gap.
        let mut event = registered(7, 7);
        event.tx_hash = [5; 32];
        state
            .registry
            .write()
            .unwrap()
            .extend_preview(vec![block(4, vec![event])], 6);
        let Json(previewed) = merkle_proof(State(state.clone()), Path("7".into()))
            .await
            .unwrap();
        assert_eq!(previewed["active"], false);
        assert!(previewed["proof"].is_null());
        assert_eq!(previewed["leaf_index"], 7);
        assert_eq!(previewed["lez_preview"]["lez_block_id"], 4);
        assert_eq!(
            previewed["lez_preview"]["event"],
            crate::lez::Registered::SELECTOR_NAME
        );
        assert_eq!(previewed["lez_preview"]["tx_hash"], json!("05".repeat(32)));
        let Json(overview) = summary(State(state)).await;
        assert_eq!(overview["lez"]["sequencer_block_id"], 6);
        assert_eq!(overview["lez"]["not_on_bedrock"], 3);
    }
}
