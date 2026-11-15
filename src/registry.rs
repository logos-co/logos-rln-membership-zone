//! One node's state: the tree derived from LEZ (`Reader`), the Zone channel as the Zone SDK
//! reports it (`ChainState`), the changes no batch carries yet, and the SDK checkpoint. All of
//! it lives in the tree store's metadata, so a restart opens the store and continues.

use std::path::Path;

use anyhow::{Context as _, Result};
use lb_core::mantle::ops::channel::MsgId;
use prost::Message as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    batch::{Batch, Change, Kind},
    chain::{
        AcceptOutcome, BatchIngestError, ChainState, ChannelEntry, FinalEntry, Judge,
        PublishRecord, StallReason, Tip,
    },
    lez::Block,
    reader::{EMPTY_ROOT, Reader},
};

impl Judge for Reader {
    fn root_at(&self, to_lez_block_id: u64) -> Option<[u8; 32]> {
        Reader::root_at(self, to_lez_block_id)
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

/// A channel entry as the API shows it: `finalized` (in the final tier, with the L1 slot that
/// finalized it) or in the view.
#[derive(Debug, Clone, Serialize)]
pub struct EntryView {
    pub msg_id: String,
    pub parent_msg: String,
    /// `None` for an entry that carries no batch.
    pub from_lez_block_id: Option<u64>,
    pub to_lez_block_id: Option<u64>,
    pub root: Option<String>,
    pub changes: Vec<ChangeRecord>,
    #[serde(rename = "finalized")]
    pub is_finalized: bool,
    /// The L1 slot of the block that finalized it; `null` before.
    pub l1_slot: Option<u64>,
    /// The fold's verdict: `applied`, `parked` or `retryable_failure` (LEZ `AcceptOutcome`).
    pub outcome: &'static str,
    pub error: Option<BatchIngestError>,
}

impl EntryView {
    fn new(
        msg: [u8; 32],
        parent: [u8; 32],
        batch: Option<&Batch>,
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
            is_finalized: l1_slot.is_some(),
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
    /// The tree at the latest LEZ block applied.
    pub head: Reader,
    /// The channel: final tier and view.
    pub chain: ChainState,
    pub first_lez_block_id: u64,
    /// Changes of the LEZ blocks after the head tip, in derivation-rule order, each with the
    /// block that made it: the next batch's content for the sequencer, the head's lead over
    /// the channel for the indexer; trimmed as the tip advances.
    pub pending_changes: Vec<(u64, Change)>,
    /// Our own publishes not seen in a Bedrock block yet, in publish order.
    own_pending: Vec<[u8; 32]>,
    anchor: Option<ZoneAnchorRecord>,
    /// The Zone SDK checkpoint, as JSON.
    checkpoint: Option<Value>,
    /// The indexer judges every batch against its tree; the sequencer trusts its own.
    indexer: bool,
}

impl Registry {
    /// Open the tree store under `data_dir` and the Zone channel state kept with it; a fresh store
    /// starts at `first_lez_block_id`. With `rebuild`, the tree and the reader's state are
    /// dropped and read again from LEZ while the Zone channel state and the checkpoint are kept.
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
        let (mut head, extra) = Reader::open(&dir, tree_id, first_lez_block_id.saturating_sub(1))?;
        let extra = match kept {
            Some(kept) => {
                head.persist(kept.clone())?;
                kept
            }
            None => extra,
        };
        Self::from_parts(head, &extra, first_lez_block_id, rebuild, indexer)
    }

    /// A throwaway store, for tests.
    #[cfg(test)]
    pub fn open_temporary(first_lez_block_id: u64, indexer: bool) -> Result<Self> {
        let (head, extra) = Reader::open_temporary([0; 32], first_lez_block_id.saturating_sub(1))?;
        Self::from_parts(head, &extra, first_lez_block_id, false, indexer)
    }

    fn from_parts(
        head: Reader,
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
        let judge = indexer.then_some(&head as &dyn Judge);
        let chain = ChainState::restore(final_entries, stall, &view, judge)?;
        let first_lez_block_id = get("first_lez_block_id")
            .and_then(|v| v.as_u64())
            .unwrap_or(first_lez_block_id);
        let own_pending: Vec<String> = get("own_pending")
            .map(serde_json::from_value)
            .transpose()
            .context("own pending publishes in the tree store")?
            .unwrap_or_default();
        let own_pending = own_pending
            .iter()
            .map(|h| crate::reader::parse_32(h))
            .collect::<Result<_>>()?;
        let anchor = get("anchor")
            .map(serde_json::from_value)
            .transpose()
            .context("anchor in the tree store")?;
        let checkpoint = get("checkpoint");
        // The changes applied but not yet in a batch survive a restart with the tree they
        // changed; a rebuild derives them again from LEZ.
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
                    .collect::<Result<_>>()?
            }
            _ => vec![],
        };
        Ok(Self {
            head,
            chain,
            first_lez_block_id,
            pending_changes,
            own_pending,
            anchor,
            checkpoint,
            indexer,
        })
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
            "view": hex::encode(self.chain.encode_view()),
            "final": self.chain.final_entries(),
            "stall": self.chain.stall(),
            "anchor": self.anchor,
            "own_pending": self.own_pending.iter().map(hex::encode).collect::<Vec<_>>(),
            "pending_changes": pending_changes,
            "checkpoint": self.checkpoint,
        });
        self.head.persist(extra)
    }

    pub fn close(&mut self) -> Result<()> {
        self.head.close()
    }

    pub fn checkpoint(&self) -> Option<&Value> {
        self.checkpoint.as_ref()
    }

    pub fn set_checkpoint(&mut self, checkpoint: Value) {
        self.checkpoint = Some(checkpoint);
    }

    pub fn anchor(&self) -> Option<ZoneAnchorRecord> {
        self.anchor
    }

    /// The LEZ block the head tip covers to: the block before the first while no batch does.
    pub fn covered_to(&self) -> u64 {
        self.chain
            .head_tip()
            .map_or(self.first_lez_block_id.saturating_sub(1), |tip| {
                tip.to_lez_block_id
            })
    }

    /// Apply LEZ blocks in order (`Reader::apply_block`); the changes of the blocks after the
    /// head tip become pending.
    pub fn apply_blocks(&mut self, blocks: &[Block]) -> Result<()> {
        self.head.check_chain(blocks)?;
        let covered_to = self.covered_to();
        for block in blocks {
            let changes = self.head.apply_block(block)?;
            if block.block_id() > covered_to {
                self.pending_changes
                    .extend(changes.into_iter().map(|change| (block.block_id(), change)));
            }
        }
        Ok(())
    }

    /// LEZ rewrote its blocks after `block_id` (`BrokenChainLink`): forget them and the
    /// pending changes they made so they are read again.
    pub fn rewind_to(&mut self, block_id: u64) -> Result<()> {
        self.head.rewind_to(block_id)?;
        self.pending_changes.retain(|(block, _)| *block <= block_id);
        Ok(())
    }

    /// The batches whose root is not what the tree derived from LEZ says at their
    /// `to_lez_block_id`: statements LEZ has since contradicted. An indexer parks on them.
    pub fn contradicted(&self) -> Vec<EntryView> {
        self.entries()
            .into_iter()
            .filter(|e| match (e.to_lez_block_id, &e.root) {
                (Some(to), Some(root)) => self
                    .head
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
        if self.head.lez_block_id <= covered_to {
            return None;
        }
        Some(Batch {
            from_lez_block_id: covered_to + 1,
            to_lez_block_id: self.head.lez_block_id,
            changes: self
                .pending_changes
                .iter()
                .map(|(_, change)| change.clone())
                .collect(),
            root_after: self.head.root().to_vec(),
        })
    }

    /// A batch of ours handed to the SDK: into the view on the pin it was built on.
    pub fn record_publish(&mut self, msg: MsgId, parent: MsgId, batch: &Batch) -> PublishRecord {
        let entry = ChannelEntry {
            msg,
            parent,
            batch: Some(batch.clone()),
        };
        let judge = self.indexer.then_some(&self.head as &dyn Judge);
        let record = self.chain.record_publish(entry, judge);
        if record != PublishRecord::AlreadyInView {
            self.own_pending.push(msg.into());
        }
        self.trim_pending();
        record
    }

    /// Entries that entered the view (`ChannelUpdate::Extension`).
    pub fn apply_extension(&mut self, adopted: Vec<ChannelEntry>) {
        let judge = self.indexer.then_some(&self.head as &dyn Judge);
        self.chain.apply_extension(adopted, judge);
        self.trim_pending();
    }

    /// The whole view at the new tip (`ChannelUpdate::Conflict`); returns the entries of ours
    /// that left it, to publish again.
    pub fn apply_conflict(&mut self, canonical: Vec<ChannelEntry>) -> Vec<ChannelEntry> {
        let judge = self.indexer.then_some(&self.head as &dyn Judge);
        let dropped = self.chain.apply_conflict(canonical, judge);
        for entry in &dropped {
            let msg: [u8; 32] = entry.msg.into();
            self.own_pending.retain(|m| *m != msg);
        }
        self.trim_pending();
        dropped
    }

    /// A finalized inscription of the channel (the SDK stream or the finality scan).
    pub fn apply_finalized(&mut self, entry: ChannelEntry, l1_slot: u64) -> Option<AcceptOutcome> {
        let msg: [u8; 32] = entry.msg.into();
        let judge = self.indexer.then_some(&self.head as &dyn Judge);
        let outcome = self.chain.apply_finalized(entry, l1_slot, judge);
        self.own_pending.retain(|m| *m != msg);
        let tip = self.chain.final_tip();
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

    /// The indexer folds again what the judge could not decide: LEZ read further.
    pub fn retry_folds(&mut self) -> usize {
        let decided = self.chain.retry_folds(Some(&self.head));
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
            .chain
            .final_entries()
            .iter()
            .map(|f| {
                EntryView::new(
                    f.msg,
                    f.parent,
                    f.batch().as_ref(),
                    Some(f.l1_slot),
                    f.error.as_ref(),
                )
            })
            .collect();
        for (entry, outcome) in self.chain.view_outcomes() {
            out.push(EntryView::new(
                entry.msg.into(),
                entry.parent.into(),
                entry.batch.as_ref(),
                None,
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
        self.entries()
            .into_iter()
            .rev()
            .find(|e| e.outcome == "applied" && (e.is_finalized || !finalized_only))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::lez::Event;

    fn registered(n: u8, leaf_index: u64) -> Event {
        let mut id_commitment = [0u8; 32];
        id_commitment[0] = n;
        Event::Registered {
            id_commitment,
            rate_limit: 100,
            expiry_timestamp_ms: 9_000,
            leaf_index,
        }
    }

    fn block(block_id: u64, events: Vec<Event>) -> Block {
        Block {
            header: crate::lez::BlockHeader {
                block_id,
                prev_block_hash: common::HashType([0; 32]),
                hash: common::HashType([0; 32]),
                timestamp: 1_000,
            },
            bedrock_status: crate::lez::BedrockStatus::Finalized,
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
            .apply_blocks(&[block(1, vec![registered(1, 0)]), block(2, vec![])])
            .unwrap();
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
        assert!(!registry.entries()[0].is_finalized);
        // The channel tip is our batch: adopted.
        assert_eq!(registry.mark_adopted_up_to([1; 32]), vec![[1; 32]]);
        assert_eq!(registry.in_flight(), None);
        assert!(!registry.entries()[0].is_finalized);
        // Finality: into the final tier, the anchor moves.
        let entry = ChannelEntry {
            msg: msg(1),
            parent: MsgId::root(),
            batch: Some(batch.clone()),
        };
        assert!(matches!(
            registry.apply_finalized(entry, 500),
            Some(AcceptOutcome::Applied(_))
        ));
        assert!(registry.entries()[0].is_finalized);
        assert_eq!(registry.entries()[0].l1_slot, Some(500));
        assert_eq!(registry.anchor().unwrap().slot, 500);
        assert_eq!(registry.anchor().unwrap().to_lez_block_id, 2);
        assert_eq!(registry.roots(5).len(), 1);
        assert_eq!(registry.newest(true).unwrap().msg_id, hex::encode([1; 32]));
        // A heartbeat carries the root on: same run.
        registry.apply_blocks(&[block(3, vec![])]).unwrap();
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
            registry
                .apply_blocks(&[block(1, vec![registered(1, 0)])])
                .unwrap();
            batch = registry.next_batch().unwrap();
            registry.record_publish(msg(1), MsgId::root(), &batch);
            registry
                .apply_blocks(&[block(2, vec![registered(2, 1)])])
                .unwrap();
            assert_eq!(registry.pending_changes.len(), 1);
            registry.set_checkpoint(json!({"lib": 7}));
            registry.save().unwrap();
            registry.close().unwrap();
        }
        {
            let mut registry = Registry::open(&dir, [0; 32], 1, false, false).unwrap();
            assert_eq!(registry.chain.view().len(), 1);
            assert_eq!(registry.in_flight(), Some([1; 32]));
            assert_eq!(registry.covered_to(), 1);
            assert_eq!(registry.pending_changes.len(), 1);
            assert_eq!(registry.checkpoint(), Some(&json!({"lib": 7})));
            // The next batch continues from the view's tip.
            let next = registry.next_batch().unwrap();
            assert_eq!((next.from_lez_block_id, next.to_lez_block_id), (2, 2));
            // Finalize the first: reopen keeps the final tier.
            registry.apply_finalized(
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
            assert_eq!(registry.chain.final_entries().len(), 1);
            assert_eq!(registry.chain.final_tip().unwrap().to_lez_block_id, 1);
            assert_eq!(registry.anchor().unwrap().slot, 100);
            assert!(registry.chain.view().is_empty());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_indexer_judges_batches_against_its_own_tree() {
        let mut registry = Registry::open_temporary(1, true).unwrap();
        registry
            .apply_blocks(&[block(1, vec![registered(1, 0), registered(2, 1)])])
            .unwrap();
        let root_1 = registry.head.root();
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
        registry.apply_extension(vec![ChannelEntry {
            msg: msg(1),
            parent: MsgId::root(),
            batch: Some(honest.clone()),
        }]);
        assert_eq!(registry.entries()[0].outcome, "applied");
        assert!(!registry.entries()[0].is_finalized);
        assert!(registry.pending_changes.is_empty());
        // A batch for blocks the indexer has not read yet: it replays (a heartbeat keeps the
        // root), so only the judge is missing; retryable, decided once LEZ is read.
        let early = Batch {
            from_lez_block_id: 2,
            to_lez_block_id: 3,
            changes: vec![],
            root_after: root_1.to_vec(),
        };
        registry.apply_extension(vec![ChannelEntry {
            msg: msg(2),
            parent: msg(1),
            batch: Some(early),
        }]);
        assert_eq!(registry.entries()[1].outcome, "retryable_failure");
        registry
            .apply_blocks(&[block(2, vec![]), block(3, vec![registered(3, 2)])])
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
        let root_3 = registry.head.root();
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
        let dropped = registry.apply_conflict(vec![
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
        assert_eq!(registry.entries()[1].outcome, "applied");
        assert_eq!(registry.covered_to(), 3);
        assert_eq!(registry.chain.head_root(), root_3);
    }
}
