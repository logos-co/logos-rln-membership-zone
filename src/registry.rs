//! One node's state: the LEZ chain as the Zone SDK reports it (`LezChain`: the finalized tree
//! on disk, the adopted blocks folded over it), the Zone channel as the SDK reports it
//! (`ZoneChain`), the changes no batch carries yet, and the two SDK checkpoints. All of it
//! lives in the tree store's metadata, so a restart opens the store and continues.

use std::path::Path;

use anyhow::{Context as _, Result};
use lb_core::mantle::ops::channel::MsgId;
use prost::Message as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    batch::{Batch, Change, Kind},
    lez::{Block, StoredBlock},
    lez_chain::{Adopted, Finalized, LezChain},
    reader::{BlockIngestError, EMPTY_ROOT, Reader},
    zone_chain::{
        AcceptOutcome, BatchIngestError, ChannelEntry, FinalEntry, Judge, PublishRecord,
        StallReason, Tip, ZoneChain,
    },
};

/// The indexer judges a batch against the tree of the finalized LEZ blocks only.
impl Judge for LezChain {
    fn root_at(&self, to_lez_block_id: u64) -> Option<[u8; 32]> {
        self.final_reader().root_at(to_lez_block_id)
    }
}

/// Where the final tier was last read up to on L1 (LEZ `ZoneAnchorRecord`): the slot of the
/// newest finalized entry and the final tip there.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ZoneAnchorRecord {
    pub slot: u64,
    pub to_lez_block_id: u64,
    #[serde(with = "hex::serde")]
    pub root: [u8; 32],
}

/// One tree change a batch carried, as the API shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangeRecord {
    pub kind: ChangeKind,
    pub leaf_index: u64,
    /// The leaf inserted, 32 bytes little-endian in hex; `None` for a removal.
    pub rate_commitment: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Insert,
    Remove,
}

impl ChangeRecord {
    pub fn from_change(change: &Change) -> Option<Self> {
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

/// A channel entry as the API shows it. Where it stands on Bedrock is the Zone SDK's word:
/// `pending` (a publish of ours no Bedrock block holds yet), `adopted` (in a Bedrock block on
/// the current branch: the view) or `finalized` (the final tier, with the L1 slot of the block
/// that finalized it). The fold's outcome and error stay internal (LEZ `AcceptOutcome` names):
/// the API serves the applied entries and the `stall`.
#[derive(Debug, Clone, Serialize)]
pub struct EntryView {
    pub msg_id: String,
    pub parent_msg: String,
    /// `None` for an entry that carries no batch.
    pub from_lez_block_id: Option<u64>,
    pub to_lez_block_id: Option<u64>,
    pub root: Option<String>,
    pub changes: Vec<ChangeRecord>,
    pub bedrock_status: &'static str,
    /// The L1 slot of the block that finalized it; `null` before.
    pub l1_slot: Option<u64>,
    #[serde(skip)]
    pub outcome: &'static str,
    #[serde(skip)]
    pub error: Option<BatchIngestError>,
}

impl EntryView {
    fn new(
        msg: [u8; 32],
        parent: [u8; 32],
        batch: Option<&Batch>,
        l1_slot: Option<u64>,
        pending: bool,
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
            bedrock_status: match (l1_slot, pending) {
                (Some(_), _) => "finalized",
                (None, true) => "pending",
                (None, false) => "adopted",
            },
            l1_slot,
            outcome: match error {
                None if batch.is_some() => "applied",
                None => "skipped",
                Some(e) if e.is_retryable() => "retryable_failure",
                Some(_) => "parked",
            },
            error: error.cloned(),
        }
    }
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

/// A pending change as the tree store keeps it: the LEZ block that made it and the change,
/// prost in hex.
#[derive(Serialize, Deserialize)]
struct PendingChangeRecord {
    block: u64,
    change: String,
}

pub struct Registry {
    /// The LEZ chain: the finalized tree on disk, the adopted blocks folded over it.
    pub lez: LezChain,
    /// The channel: final tier and view.
    pub zone: ZoneChain,
    pub first_lez_block_id: u64,
    /// Changes of the LEZ blocks after the head tip, in derivation-rule order, each with the
    /// block that made it: the next batch's content for the sequencer, the head's lead over
    /// the channel for the indexer; trimmed as the tip advances.
    pub pending_changes: Vec<(u64, Change)>,
    /// Our own publishes not seen in a Bedrock block yet, in publish order.
    own_pending: Vec<[u8; 32]>,
    /// Publishes of ours a Bedrock reorg dropped (the SDK's `orphaned`), published again.
    orphaned: u64,
    anchor: Option<ZoneAnchorRecord>,
    /// The Zone SDK checkpoint on the Zone channel, as JSON.
    checkpoint: Option<Value>,
    /// The Zone SDK checkpoint on the LEZ channel, as JSON.
    lez_checkpoint: Option<Value>,
    /// The indexer judges every batch against its tree; the sequencer trusts its own.
    indexer: bool,
}

impl Registry {
    /// Open the tree store under `data_dir` and the Zone channel state kept with it; a fresh store
    /// starts at `first_lez_block_id`. With `rebuild`, the tree and the reader's state are
    /// dropped and read again from LEZ (the LEZ channel checkpoint with them) while the Zone
    /// channel state and its checkpoint are kept.
    pub fn open(
        data_dir: &Path,
        tree_id: [u8; 32],
        first_lez_block_id: u64,
        rebuild: bool,
        indexer: bool,
    ) -> Result<Self> {
        let dir = data_dir.join("tree");
        let mut kept = None;
        if rebuild && dir.exists() {
            let extra = Reader::take_extra(&dir)?;
            std::fs::remove_dir_all(&dir).context("drop the tree store")?;
            kept = Some(extra);
            println!("tree store dropped; the tree is read again from LEZ");
        }
        let (mut final_reader, extra) =
            Reader::open(&dir, tree_id, first_lez_block_id.saturating_sub(1))?;
        let extra = match kept {
            Some(kept) => {
                final_reader.persist(kept.clone())?;
                kept
            }
            None => extra,
        };
        Self::from_parts(final_reader, &extra, first_lez_block_id, rebuild, indexer)
    }

    /// A throwaway store, for tests.
    #[cfg(test)]
    pub fn open_temporary(first_lez_block_id: u64, indexer: bool) -> Result<Self> {
        let (final_reader, extra) =
            Reader::open_temporary([0; 32], first_lez_block_id.saturating_sub(1))?;
        Self::from_parts(final_reader, &extra, first_lez_block_id, false, indexer)
    }

    fn from_parts(
        final_reader: Reader,
        extra: &Value,
        first_lez_block_id: u64,
        rebuild: bool,
        indexer: bool,
    ) -> Result<Self> {
        let get = |key: &str| extra.get(key).filter(|v| !v.is_null()).cloned();
        let final_entries: Vec<FinalEntry> = get("final")
            .map(serde_json::from_value)
            .transpose()
            .context("final entries in the tree store")?
            .unwrap_or_default();
        let stall: Option<StallReason> = get("stall")
            .map(serde_json::from_value)
            .transpose()
            .context("stall record in the tree store")?;
        let view = get("view")
            .and_then(|v| v.as_str().map(hex::decode))
            .transpose()
            .context("view in the tree store")?
            .unwrap_or_default();
        let first_lez_block_id = get("first_lez_block_id")
            .and_then(|v| v.as_u64())
            .unwrap_or(first_lez_block_id);
        let mut lez = LezChain::new(final_reader)?;
        // The adopted LEZ blocks of the last run: the SDK does not echo the entries it already
        // holds, so the view comes back from the store, its changes pending again below.
        let mut restored_view = vec![];
        if !rebuild && let Some(stored) = get("lez_view") {
            let stored: Vec<StoredBlock> =
                serde_json::from_value(stored).context("adopted LEZ blocks in the tree store")?;
            let blocks = stored
                .iter()
                .map(Block::from_stored)
                .collect::<Result<Vec<_>>>()?;
            restored_view = lez.restore_view(&blocks)?;
        }
        let judge = indexer.then_some(&lez as &dyn Judge);
        let zone = ZoneChain::restore(first_lez_block_id, final_entries, stall, &view, judge)?;
        let own_pending: Vec<String> = get("own_pending")
            .map(serde_json::from_value)
            .transpose()
            .context("own pending publishes in the tree store")?
            .unwrap_or_default();
        let own_pending = own_pending
            .iter()
            .map(|h| crate::reader::parse_32(h))
            .collect::<Result<_>>()?;
        let orphaned = get("orphaned").and_then(|v| v.as_u64()).unwrap_or(0);
        let anchor = get("anchor")
            .map(serde_json::from_value)
            .transpose()
            .context("anchor in the tree store")?;
        let checkpoint = get("checkpoint");
        // A rebuild reads the finalized LEZ blocks again from the start: the LEZ channel is
        // read again from the LIB too.
        let lez_checkpoint = if rebuild { None } else { get("lez_checkpoint") };
        // The changes applied but not yet in a batch survive a restart with the tree they
        // changed, as far as the final tier goes (the adopted blocks come back from the SDK and
        // bring theirs again); a rebuild derives them again from LEZ.
        let final_block_id = lez.final_reader().lez_block_id;
        let pending_changes = match get("pending_changes") {
            Some(encoded) if !rebuild => {
                let encoded: Vec<PendingChangeRecord> =
                    serde_json::from_value(encoded).context("pending changes in the tree store")?;
                encoded
                    .iter()
                    .map(|record| {
                        let bytes =
                            hex::decode(&record.change).context("pending change is not hex")?;
                        let change = Change::decode(bytes.as_slice()).context("pending change")?;
                        Ok((record.block, change))
                    })
                    .filter(|r| !r.as_ref().is_ok_and(|(block, _)| *block > final_block_id))
                    .collect::<Result<_>>()?
            }
            _ => vec![],
        };
        let mut registry = Self {
            lez,
            zone,
            first_lez_block_id,
            pending_changes,
            own_pending,
            orphaned,
            anchor,
            checkpoint,
            lez_checkpoint,
            indexer,
        };
        for (block_id, changes) in restored_view {
            registry.add_pending(block_id, changes);
        }
        Ok(registry)
    }

    /// Write everything into the tree store.
    pub fn save(&mut self) -> Result<()> {
        let pending_changes: Vec<PendingChangeRecord> = self
            .pending_changes
            .iter()
            .map(|(block, change)| PendingChangeRecord {
                block: *block,
                change: hex::encode(change.encode_to_vec()),
            })
            .collect();
        let extra = json!({
            "first_lez_block_id": self.first_lez_block_id,
            "view": hex::encode(self.zone.encode_view()),
            "lez_view": self.lez.view().iter().map(Block::to_stored).collect::<Vec<_>>(),
            "final": self.zone.final_entries(),
            "stall": self.zone.stall(),
            "anchor": self.anchor,
            "own_pending": self.own_pending.iter().map(hex::encode).collect::<Vec<_>>(),
            "orphaned": self.orphaned,
            "pending_changes": pending_changes,
            "checkpoint": self.checkpoint,
            "lez_checkpoint": self.lez_checkpoint,
        });
        self.lez.persist(extra)
    }

    pub fn close(&mut self) -> Result<()> {
        self.lez.close()
    }

    pub fn checkpoint(&self) -> Option<&Value> {
        self.checkpoint.as_ref()
    }

    pub fn set_checkpoint(&mut self, checkpoint: Value) {
        self.checkpoint = Some(checkpoint);
    }

    pub fn lez_checkpoint(&self) -> Option<&Value> {
        self.lez_checkpoint.as_ref()
    }

    pub fn set_lez_checkpoint(&mut self, checkpoint: Value) {
        self.lez_checkpoint = Some(checkpoint);
    }

    /// The tree at the newest adopted LEZ block: what the API serves and the sequencer batches.
    pub fn head(&self) -> &Reader {
        self.lez.head()
    }

    pub fn anchor(&self) -> Option<ZoneAnchorRecord> {
        self.anchor
    }

    /// The LEZ block the head tip covers to: the block before the first while no batch does.
    pub fn covered_to(&self) -> u64 {
        self.zone
            .head_tip()
            .map_or(self.first_lez_block_id.saturating_sub(1), |tip| {
                tip.to_lez_block_id
            })
    }

    fn add_pending(&mut self, block_id: u64, changes: Vec<Change>) {
        if block_id > self.covered_to() {
            self.pending_changes
                .extend(changes.into_iter().map(|change| (block_id, change)));
        }
    }

    /// A LEZ block adopted on Bedrock (the SDK's `Extension` on the LEZ channel): onto the
    /// head; its changes after the head tip become pending. `Ok(Some(expected))` when blocks
    /// are missing before it (`expected..block_id`), to be filled first; `Err` when it does
    /// not chain for another reason.
    pub fn apply_lez_adopted(&mut self, block: &Block) -> Result<Option<u64>> {
        match self.lez.apply_adopted(block)? {
            Adopted::Applied(changes) => {
                self.add_pending(block.block_id(), changes);
                Ok(None)
            }
            Adopted::AlreadyApplied => Ok(None),
            Adopted::DoesNotChain(BlockIngestError::UnexpectedBlockId { expected, got })
                if got > expected =>
            {
                Ok(Some(expected))
            }
            Adopted::DoesNotChain(e) => Err(anyhow::anyhow!(
                "adopted LEZ block {} does not chain on the head: {e}",
                block.block_id()
            )),
        }
    }

    /// A Bedrock reorg on the LEZ channel (the SDK's `Conflict`): the head is the finalized
    /// tree again, the pending changes of the adopted blocks go with it; the caller adopts the
    /// canonical lineage again.
    pub fn apply_lez_conflict(&mut self) -> Result<()> {
        let final_block_id = self.lez.final_reader().lez_block_id;
        self.pending_changes
            .retain(|(block, _)| *block <= final_block_id);
        self.lez.reset_to_final()
    }

    /// A LEZ block Bedrock finalized (the SDK's `finalized` on the LEZ channel, or the RPC
    /// filling a gap under a finalized block): onto the finalized tree. `Ok(Some(expected))`
    /// when blocks are missing before it (`expected..block_id`), to be filled first.
    pub fn apply_lez_finalized(&mut self, block: &Block) -> Result<Option<u64>> {
        match self.lez.apply_finalized(block)? {
            Finalized::Applied {
                changes,
                new_to_head,
            } => {
                if new_to_head {
                    self.add_pending(block.block_id(), changes);
                }
                Ok(None)
            }
            Finalized::AlreadyApplied => Ok(None),
            Finalized::Gap { expected } => Ok(Some(expected)),
        }
    }

    /// The batches whose root is not what the tree derived from LEZ says at their
    /// `to_lez_block_id`: statements LEZ has since contradicted. An indexer parks on them.
    pub fn contradicted(&self) -> Vec<EntryView> {
        self.entries()
            .into_iter()
            .filter(|e| match (e.to_lez_block_id, &e.root) {
                (Some(to), Some(root)) => self
                    .head()
                    .root_at(to)
                    .is_some_and(|derived| hex::encode(derived) != *root),
                _ => false,
            })
            .collect()
    }

    /// The next batch: every LEZ block after the head tip up to the head, with the pending
    /// changes and the root after them; `None` when the head is already covered.
    pub fn next_batch(&self) -> Option<Batch> {
        let covered_to = self.covered_to();
        let head = self.head();
        if head.lez_block_id <= covered_to {
            return None;
        }
        Some(Batch {
            from_lez_block_id: covered_to + 1,
            to_lez_block_id: head.lez_block_id,
            changes: self
                .pending_changes
                .iter()
                .map(|(_, change)| change.clone())
                .collect(),
            root_after: head.root().to_vec(),
        })
    }

    /// Whether `batch` would apply on the head of the channel: checked before publishing, so
    /// a batch this node would park never reaches Bedrock.
    pub fn would_apply(&self, batch: &Batch) -> Result<(), BatchIngestError> {
        let judge = self.indexer.then_some(&self.lez as &dyn Judge);
        self.zone.would_apply(batch, judge)
    }

    /// A batch of ours handed to the SDK: into the view on the pin it was built on.
    pub fn record_publish(&mut self, msg: MsgId, parent: MsgId, batch: &Batch) -> PublishRecord {
        let entry = ChannelEntry {
            msg,
            parent,
            batch: Some(batch.clone()),
        };
        let judge = self.indexer.then_some(&self.lez as &dyn Judge);
        let record = self.zone.record_publish(entry, judge);
        if record != PublishRecord::AlreadyInView {
            self.own_pending.push(msg.into());
        }
        self.trim_pending();
        record
    }

    /// Entries that entered the view (`ChannelUpdate::Extension`).
    pub fn apply_zone_extension(&mut self, adopted: Vec<ChannelEntry>) {
        let judge = self.indexer.then_some(&self.lez as &dyn Judge);
        self.zone.apply_extension(adopted, judge);
        self.trim_pending();
    }

    /// The whole view at the new tip (`ChannelUpdate::Conflict`); returns the entries of ours
    /// that left it, to publish again.
    pub fn apply_zone_conflict(&mut self, canonical: Vec<ChannelEntry>) -> Vec<ChannelEntry> {
        let judge = self.indexer.then_some(&self.lez as &dyn Judge);
        let dropped = self.zone.apply_conflict(canonical, judge);
        self.orphaned += dropped.len() as u64;
        for entry in &dropped {
            let msg: [u8; 32] = entry.msg.into();
            self.own_pending.retain(|m| *m != msg);
        }
        self.trim_pending();
        dropped
    }

    /// A finalized inscription of the channel (the SDK stream or the finality scan).
    pub fn apply_zone_finalized(
        &mut self,
        entry: ChannelEntry,
        l1_slot: u64,
    ) -> Option<AcceptOutcome> {
        let msg: [u8; 32] = entry.msg.into();
        let judge = self.indexer.then_some(&self.lez as &dyn Judge);
        let outcome = self.zone.apply_finalized(entry, l1_slot, judge);
        self.own_pending.retain(|m| *m != msg);
        let tip = self.zone.final_tip();
        self.anchor = Some(ZoneAnchorRecord {
            slot: l1_slot.max(self.anchor.map_or(0, |a| a.slot)),
            to_lez_block_id: tip.map_or(self.first_lez_block_id.saturating_sub(1), |t| {
                t.to_lez_block_id
            }),
            root: tip.map_or(*EMPTY_ROOT, |t| t.root),
        });
        self.trim_pending();
        outcome
    }

    /// The channel tip is one of our batches: it and every pending publish before it are in a
    /// Bedrock block. Returns the ones that just landed.
    pub fn mark_adopted_up_to(&mut self, tip: [u8; 32]) -> Vec<[u8; 32]> {
        let Some(at) = self.own_pending.iter().position(|m| *m == tip) else {
            return vec![];
        };
        self.own_pending.drain(..=at).collect()
    }

    /// Our newest publish not seen in a Bedrock block yet.
    pub fn in_flight(&self) -> Option<[u8; 32]> {
        self.own_pending.last().copied()
    }

    /// How many publishes of ours a Bedrock reorg dropped (published again each time).
    pub fn orphaned_count(&self) -> u64 {
        self.orphaned
    }

    /// The indexer folds again what the judge could not decide: LEZ read further.
    pub fn retry_folds(&mut self) -> usize {
        let decided = self.zone.retry_folds(Some(&self.lez));
        self.trim_pending();
        decided
    }

    fn trim_pending(&mut self) {
        let to = self.covered_to();
        self.pending_changes.retain(|(block, _)| *block > to);
    }

    /// Every entry of the channel, final tier first then the view, as the API shows them.
    pub fn entries(&self) -> Vec<EntryView> {
        let mut out: Vec<EntryView> = self
            .zone
            .final_entries()
            .iter()
            .map(|f| {
                EntryView::new(
                    f.msg,
                    f.parent,
                    f.batch().as_ref(),
                    Some(f.l1_slot),
                    false,
                    f.error.as_ref(),
                )
            })
            .collect();
        for (entry, outcome) in self.zone.view_outcomes() {
            let msg: [u8; 32] = entry.msg.into();
            out.push(EntryView::new(
                msg,
                entry.parent.into(),
                entry.batch.as_ref(),
                None,
                self.own_pending.contains(&msg),
                outcome.as_ref().err(),
            ));
        }
        out
    }

    /// The roots newest first, at most `window_size`: one entry per run of applied batches
    /// carrying the same root, the batch that produced it first and every batch of the run
    /// after it (heartbeats as a rule). A root the tree comes back to later is a new run.
    pub fn roots(&self, window_size: usize) -> Vec<RootEntry> {
        let mut runs: Vec<RootEntry> = vec![];
        for entry in self
            .entries()
            .into_iter()
            .filter(|e| e.outcome == "applied")
        {
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
    pub fn newest(&self, finalized_only: bool) -> Option<EntryView> {
        self.entries().into_iter().rev().find(|e| {
            e.outcome == "applied" && (e.bedrock_status == "finalized" || !finalized_only)
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::lez::{Registered, RlnEvent};

    fn registered(n: u8, leaf_index: u64) -> RlnEvent {
        let mut id_commitment = [0u8; 32];
        id_commitment[0] = n;
        RlnEvent::Registered(Registered {
            tree_id: [0; 32],
            id_commitment,
            rate_limit: 100,
            expiry_timestamp_ms: 9_000,
            leaf_index,
        })
    }

    fn block(block_id: u64, events: Vec<RlnEvent>) -> Block {
        Block {
            header: crate::lez::BlockHeader {
                block_id,
                prev_block_hash: common::HashType([0; 32]),
                hash: common::HashType([0; 32]),
                timestamp: 1_000,
            },
            events,
            private_transaction_count: 0,
        }
    }

    fn msg(n: u8) -> MsgId {
        MsgId::from([n; 32])
    }

    #[test]
    fn test_sequencer_builds_publishes_and_sees_its_batches_land() {
        let mut registry = Registry::open_temporary(1, false).unwrap();
        registry
            .apply_lez_adopted(&block(1, vec![registered(1, 0)]))
            .unwrap();
        registry.apply_lez_adopted(&block(2, vec![])).unwrap();
        assert_eq!(registry.pending_changes.len(), 1);
        let batch = registry.next_batch().unwrap();
        assert_eq!((batch.from_lez_block_id, batch.to_lez_block_id), (1, 2));
        assert_eq!(batch.changes.len(), 1);
        assert_eq!(
            registry.record_publish(msg(1), MsgId::root(), &batch),
            PublishRecord::Extended
        );
        // Published: the changes are no longer pending, the head is covered, one in flight.
        assert!(registry.pending_changes.is_empty());
        assert_eq!(registry.covered_to(), 2);
        assert_eq!(registry.next_batch(), None);
        assert_eq!(registry.in_flight(), Some([1; 32]));
        assert_eq!(registry.entries()[0].bedrock_status, "pending");
        // The channel tip is our batch: adopted.
        assert_eq!(registry.mark_adopted_up_to([1; 32]), vec![[1; 32]]);
        assert_eq!(registry.in_flight(), None);
        assert_eq!(registry.entries()[0].bedrock_status, "adopted");
        // Finality: into the final tier, the anchor moves.
        let entry = ChannelEntry {
            msg: msg(1),
            parent: MsgId::root(),
            batch: Some(batch.clone()),
        };
        assert!(matches!(
            registry.apply_zone_finalized(entry, 500),
            Some(AcceptOutcome::Applied(_))
        ));
        assert_eq!(registry.entries()[0].bedrock_status, "finalized");
        assert_eq!(registry.entries()[0].l1_slot, Some(500));
        assert_eq!(registry.anchor().unwrap().slot, 500);
        assert_eq!(registry.anchor().unwrap().to_lez_block_id, 2);
        assert_eq!(registry.roots(5).len(), 1);
        assert_eq!(registry.newest(true).unwrap().msg_id, hex::encode([1; 32]));
        // A heartbeat carries the root on: same run.
        registry.apply_lez_adopted(&block(3, vec![])).unwrap();
        let heartbeat = registry.next_batch().unwrap();
        assert!(heartbeat.changes.is_empty());
        registry.record_publish(msg(2), msg(1), &heartbeat);
        assert_eq!(registry.roots(5).len(), 1);
        assert_eq!(registry.roots(5)[0].entries.len(), 2);
        assert_eq!(registry.roots(5)[0].carried_to_lez_block_id(), 3);
        // Nothing LEZ contradicts.
        assert!(registry.contradicted().is_empty());
    }

    #[test]
    fn test_reopen_keeps_the_channel_state_and_the_pending_changes() {
        let dir = std::env::temp_dir().join(format!("zone-registry-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let batch;
        {
            let mut registry = Registry::open(&dir, [0; 32], 1, false, false).unwrap();
            // Finalized blocks persist with the store; adopted ones persist as the view and
            // are folded again on the next start (the SDK does not echo them).
            assert_eq!(
                registry
                    .apply_lez_finalized(&block(1, vec![registered(1, 0)]))
                    .unwrap(),
                None
            );
            batch = registry.next_batch().unwrap();
            registry.record_publish(msg(1), MsgId::root(), &batch);
            assert_eq!(
                registry
                    .apply_lez_finalized(&block(2, vec![registered(2, 1)]))
                    .unwrap(),
                None
            );
            assert_eq!(registry.pending_changes.len(), 1);
            assert_eq!(
                registry
                    .apply_lez_adopted(&block(3, vec![registered(3, 2)]))
                    .unwrap(),
                None
            );
            // Adopted block 5 with 4 missing: the caller fills the gap first.
            assert_eq!(
                registry.apply_lez_adopted(&block(5, vec![])).unwrap(),
                Some(4)
            );
            assert_eq!(registry.head().lez_block_id, 3);
            assert_eq!(registry.pending_changes.len(), 2);
            registry.set_checkpoint(json!({"lib": 7}));
            registry.set_lez_checkpoint(json!({"lib": 8}));
            registry.save().unwrap();
            registry.close().unwrap();
        }
        {
            let mut registry = Registry::open(&dir, [0; 32], 1, false, false).unwrap();
            assert_eq!(registry.zone.view().len(), 1);
            assert_eq!(registry.in_flight(), Some([1; 32]));
            assert_eq!(registry.covered_to(), 1);
            // The finalized tree and the adopted block 3 over it, with the changes of both.
            assert_eq!(registry.lez.final_reader().lez_block_id, 2);
            assert_eq!(registry.head().lez_block_id, 3);
            assert_eq!(registry.lez.view().len(), 1);
            assert_eq!(registry.pending_changes.len(), 2);
            assert_eq!(registry.checkpoint(), Some(&json!({"lib": 7})));
            assert_eq!(registry.lez_checkpoint(), Some(&json!({"lib": 8})));
            // The next batch continues from the view's tip up to the head.
            let next = registry.next_batch().unwrap();
            assert_eq!((next.from_lez_block_id, next.to_lez_block_id), (2, 3));
            // A Bedrock reorg on the LEZ channel: back to the finalized tree, the adopted
            // block's changes leave the pending ones until it is adopted again.
            registry.apply_lez_conflict().unwrap();
            assert_eq!(registry.head().lez_block_id, 2);
            assert_eq!(registry.pending_changes.len(), 1);
            registry
                .apply_lez_adopted(&block(3, vec![registered(3, 2)]))
                .unwrap();
            assert_eq!(registry.pending_changes.len(), 2);
            // Finalize the first: reopen keeps the final tier.
            registry.apply_zone_finalized(
                ChannelEntry {
                    msg: msg(1),
                    parent: MsgId::root(),
                    batch: Some(batch.clone()),
                },
                100,
            );
            registry.save().unwrap();
            registry.close().unwrap();
        }
        {
            let registry = Registry::open(&dir, [0; 32], 1, false, false).unwrap();
            assert_eq!(registry.zone.final_entries().len(), 1);
            assert_eq!(registry.zone.final_tip().unwrap().to_lez_block_id, 1);
            assert_eq!(registry.anchor().unwrap().slot, 100);
            assert!(registry.zone.view().is_empty());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_indexer_judges_batches_against_its_own_tree() {
        let mut registry = Registry::open_temporary(1, true).unwrap();
        // The indexer judges over the finalized tree: the blocks arrive finalized here.
        registry
            .apply_lez_finalized(&block(1, vec![registered(1, 0), registered(2, 1)]))
            .unwrap();
        let root_1 = registry.head().root();
        let changes: Vec<Change> = registry
            .pending_changes
            .iter()
            .map(|(_, c)| c.clone())
            .collect();
        let honest = Batch {
            from_lez_block_id: 1,
            to_lez_block_id: 1,
            changes: changes.clone(),
            root_after: root_1.to_vec(),
        };
        // Adopted on the channel: applied, the pending changes are the head's lead no more.
        registry.apply_zone_extension(vec![ChannelEntry {
            msg: msg(1),
            parent: MsgId::root(),
            batch: Some(honest.clone()),
        }]);
        assert_eq!(registry.entries()[0].outcome, "applied");
        assert_eq!(registry.entries()[0].bedrock_status, "adopted");
        assert!(registry.pending_changes.is_empty());
        // A batch for blocks the indexer has not read yet: it replays (a heartbeat keeps the
        // root), so only the judge is missing; retryable, decided once LEZ is read.
        let early = Batch {
            from_lez_block_id: 2,
            to_lez_block_id: 3,
            changes: vec![],
            root_after: root_1.to_vec(),
        };
        registry.apply_zone_extension(vec![ChannelEntry {
            msg: msg(2),
            parent: msg(1),
            batch: Some(early),
        }]);
        assert_eq!(registry.entries()[1].outcome, "retryable_failure");
        // Adopted, not finalized: the judge waits.
        registry.apply_lez_adopted(&block(2, vec![])).unwrap();
        registry
            .apply_lez_adopted(&block(3, vec![registered(3, 2)]))
            .unwrap();
        assert_eq!(registry.retry_folds(), 0);
        registry.apply_lez_finalized(&block(2, vec![])).unwrap();
        registry
            .apply_lez_finalized(&block(3, vec![registered(3, 2)]))
            .unwrap();
        assert_eq!(registry.retry_folds(), 1);
        // Decided: block 3 registered someone, the tree derived from LEZ moved on.
        assert_eq!(registry.entries()[1].outcome, "parked");
        assert!(matches!(
            registry.entries()[1].error,
            Some(BatchIngestError::DerivationMismatch { .. })
        ));
        // The head tip stays at the last applied batch.
        assert_eq!(registry.covered_to(), 1);
        // A conflict replaces the parked entry by an honest one for the same blocks.
        let root_3 = registry.head().root();
        let changes_3: Vec<Change> = registry
            .pending_changes
            .iter()
            .map(|(_, c)| c.clone())
            .collect();
        let honest_3 = Batch {
            from_lez_block_id: 2,
            to_lez_block_id: 3,
            changes: changes_3,
            root_after: root_3.to_vec(),
        };
        let dropped = registry.apply_zone_conflict(vec![
            ChannelEntry {
                msg: msg(1),
                parent: MsgId::root(),
                batch: Some(honest),
            },
            ChannelEntry {
                msg: msg(3),
                parent: msg(1),
                batch: Some(honest_3),
            },
        ]);
        assert_eq!(dropped.len(), 1);
        assert_eq!(registry.orphaned_count(), 1);
        assert_eq!(registry.entries()[1].outcome, "applied");
        assert_eq!(registry.covered_to(), 3);
        assert_eq!(registry.zone.head_root(), root_3);
    }
}
