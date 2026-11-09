//! The registry the sequencer holds and the HTTP API serves: the tree at the LEZ head, the
//! changes not yet in a batch, and every batch published with the status of its Mantle
//! Transaction on Bedrock. Everything is kept in the tree store (`data/tree/`): the tree and
//! the reader's state by the reader, the batch history (every batch with its changes, so the
//! inscription it carried can be built again), the pending changes and the Zone SDK checkpoint
//! as the reader's `extra`. What the channel holds, this store holds too.

use std::path::Path;

use anyhow::{Context as _, Result};
use prost::Message as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    batch::{Batch, Change, Kind},
    lez::Block,
    reader::Reader,
};

/// Where the Mantle Transaction carrying a batch stands, in the Zone SDK's words: it is
/// `pending` from `publish`, `adopted` or `orphaned` through `ChannelUpdate`, `finalized`
/// through `FinalizedTx`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum Status {
    /// Published, not yet seen in a Bedrock block.
    Pending,
    /// In a Bedrock block on the canonical branch, not yet final.
    Adopted,
    /// Dropped from the canonical branch by a reorg; published again as a new transaction.
    Orphaned,
    /// In finalized chain history.
    Finalized { l1_slot: u64 },
}

/// One tree change a batch carried, as the history keeps it and the API shows it.
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
    /// A change without a kind (prost's `None`) cannot come from this sequencer.
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

    pub fn to_change(&self) -> Option<Change> {
        Some(match self.kind {
            ChangeKind::Insert => Change::insert(
                self.leaf_index,
                hex::decode(self.rate_commitment.as_ref()?).ok()?,
            ),
            ChangeKind::Remove => Change::remove(self.leaf_index),
        })
    }
}

/// One batch this sequencer published, tracked by its inscription's message id: the Zone SDK
/// re-funds a transaction that stays unmined (`stale_refund_slots`) under a new transaction
/// hash but the same message id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Published {
    /// The inscription's message id in the channel.
    #[serde(with = "hex::serde")]
    pub msg_id: [u8; 32],
    /// Hash of the latest Mantle Transaction carrying it.
    #[serde(with = "hex::serde")]
    pub tx_hash: [u8; 32],
    pub from_lez_block_id: u64,
    pub to_lez_block_id: u64,
    #[serde(with = "hex::serde")]
    pub root_after: [u8; 32],
    pub change_count: usize,
    /// The changes the batch carried, in order; empty with `change_count` > 0 only for a batch
    /// published before the history kept them.
    #[serde(default)]
    pub changes: Vec<ChangeRecord>,
    pub published_at_unix: u64,
    /// The L1 slot at publish time: where the finality scan starts looking for it. `None` for
    /// a batch published before the slot was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at_slot: Option<u64>,
    #[serde(flatten)]
    pub status: Status,
}

/// The published batches.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct History {
    pub first_lez_block_id: u64,
    /// Last LEZ block a published batch covers.
    pub covered_to: u64,
    pub published: Vec<Published>,
    /// The L1 slot the finality scan has read the finalized blocks up to.
    #[serde(default)]
    pub finality_scanned_to: Option<u64>,
}

pub struct Registry {
    pub history: History,
    /// The tree at the latest LEZ block applied.
    pub head: Reader,
    /// Changes of the LEZ blocks after `covered_to`, in derivation-rule order.
    pub pending_changes: Vec<Change>,
    /// The Zone SDK checkpoint, as JSON.
    checkpoint: Option<Value>,
}

impl Registry {
    /// Open the tree store under `data_dir` and the batch history kept with it; a fresh store
    /// starts at `first_lez_block_id`. With `rebuild`, the tree and the reader's state are
    /// dropped and read again from LEZ while the batch history and the checkpoint are kept:
    /// the way out after a decoder that skipped transactions.
    pub fn open(
        data_dir: &Path,
        tree_id: [u8; 32],
        first_lez_block_id: u64,
        rebuild: bool,
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
        let history = match extra.get("history") {
            Some(h) => {
                serde_json::from_value(h.clone()).context("batch history in the tree store")?
            }
            None => History {
                first_lez_block_id,
                covered_to: first_lez_block_id.saturating_sub(1),
                published: vec![],
                finality_scanned_to: None,
            },
        };
        let checkpoint = extra.get("checkpoint").filter(|c| !c.is_null()).cloned();
        // The changes applied but not yet published survive a restart with the tree they
        // changed; a rebuild derives them again from LEZ.
        let pending_changes = match extra.get("pending_changes") {
            Some(encoded) if !rebuild => {
                let encoded: Vec<String> = serde_json::from_value(encoded.clone())
                    .context("pending changes in the tree store")?;
                encoded
                    .iter()
                    .map(|hex| {
                        let bytes = hex::decode(hex).context("pending change is not hex")?;
                        Change::decode(bytes.as_slice()).context("pending change")
                    })
                    .collect::<Result<_>>()?
            }
            _ => vec![],
        };
        Ok(Self {
            history,
            head,
            pending_changes,
            checkpoint,
        })
    }

    /// Write the reader's state, the history, the pending changes and the checkpoint into the
    /// tree store.
    pub fn save(&mut self) -> Result<()> {
        let pending_changes: Vec<String> = self
            .pending_changes
            .iter()
            .map(|change| hex::encode(change.encode_to_vec()))
            .collect();
        let extra = json!({
            "history": self.history,
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

    /// Apply closed LEZ blocks to the head tree; the changes of blocks after `covered_to` wait
    /// for the next batch.
    pub fn apply_blocks(&mut self, blocks: &[Block]) -> Result<()> {
        for block in blocks {
            let changes = self.head.apply_block(block)?;
            if block.block_id > self.history.covered_to {
                self.pending_changes.extend(changes);
            }
        }
        Ok(())
    }

    /// The next batch: every LEZ block after `covered_to` up to the head, with the pending
    /// changes and the root after them; `None` when the head is already covered.
    pub fn next_batch(&self) -> Option<Batch> {
        if self.head.applied_block_id <= self.history.covered_to {
            return None;
        }
        Some(Batch {
            from_lez_block_id: self.history.covered_to + 1,
            to_lez_block_id: self.head.applied_block_id,
            changes: self.pending_changes.clone(),
            root_after: self.head.root().to_vec(),
        })
    }

    /// Record a batch handed to the SDK.
    pub fn on_published(
        &mut self,
        msg_id: [u8; 32],
        tx_hash: [u8; 32],
        batch: &Batch,
        published_at_slot: Option<u64>,
    ) {
        let mut root_after = [0u8; 32];
        root_after.copy_from_slice(&batch.root_after);
        self.history.published.push(Published {
            msg_id,
            tx_hash,
            from_lez_block_id: batch.from_lez_block_id,
            to_lez_block_id: batch.to_lez_block_id,
            root_after,
            change_count: batch.changes.len(),
            changes: batch
                .changes
                .iter()
                .filter_map(ChangeRecord::from_change)
                .collect(),
            published_at_unix: now_unix(),
            published_at_slot,
            status: Status::Pending,
        });
        self.history.covered_to = batch.to_lez_block_id;
        self.pending_changes.clear();
    }

    /// A reorg dropped the message `old`: the same batch goes out again as `new_msg_id` (a
    /// new message, since its parent changed).
    pub fn on_republished(&mut self, old: [u8; 32], new_msg_id: [u8; 32], new_tx_hash: [u8; 32]) {
        let Some(entry) = self.published_mut(&old) else {
            return;
        };
        let mut again = entry.clone();
        entry.status = Status::Orphaned;
        again.msg_id = new_msg_id;
        again.tx_hash = new_tx_hash;
        again.status = Status::Pending;
        again.published_at_unix = now_unix();
        self.history.published.push(again);
    }

    /// The inscription the batch `msg_id` carried, built again from the history (the batch
    /// wire format is deterministic): to publish it again after a reorg and to recognise it
    /// when Bedrock finality reaches it. `None` for an unknown message id or a batch whose
    /// changes the history does not have.
    pub fn payload(&self, msg_id: &[u8; 32]) -> Option<Vec<u8>> {
        let p = self
            .history
            .published
            .iter()
            .find(|p| p.msg_id == *msg_id)?;
        if p.changes.len() != p.change_count {
            return None;
        }
        let changes = p
            .changes
            .iter()
            .map(ChangeRecord::to_change)
            .collect::<Option<Vec<_>>>()?;
        Some(
            Batch {
                from_lez_block_id: p.from_lez_block_id,
                to_lez_block_id: p.to_lez_block_id,
                changes,
                root_after: p.root_after.to_vec(),
            }
            .encode_to_vec(),
        )
    }

    /// A batch published before the history kept the changes learns them from its inscription
    /// as the chain holds it: `payload` must decode to a batch with the span and root on
    /// record. Returns whether the history changed.
    pub fn fill_changes(&mut self, msg_id: &[u8; 32], payload: &[u8]) -> bool {
        let Some(entry) = self.published_mut(msg_id) else {
            return false;
        };
        if entry.changes.len() == entry.change_count {
            return false;
        }
        let Ok(batch) = Batch::decode(payload) else {
            return false;
        };
        if batch.from_lez_block_id != entry.from_lez_block_id
            || batch.to_lez_block_id != entry.to_lez_block_id
            || batch.root_after[..] != entry.root_after[..]
            || batch.changes.len() != entry.change_count
        {
            return false;
        }
        entry.changes = batch
            .changes
            .iter()
            .filter_map(ChangeRecord::from_change)
            .collect();
        true
    }

    /// The finalized batches whose changes the history does not have, with the L1 slot of
    /// the block that holds their inscription: one block read each fills them.
    pub fn finalized_without_changes(&self) -> Vec<([u8; 32], u64)> {
        self.history
            .published
            .iter()
            .filter(|p| p.changes.len() != p.change_count)
            .filter_map(|p| match p.status {
                Status::Finalized { l1_slot } => Some((p.msg_id, l1_slot)),
                _ => None,
            })
            .collect()
    }

    /// Bedrock finality reached the batch `msg_id` at `l1_slot`, carrying `tx_hash` and
    /// `payload`: mark it, unless it is not ours, already final, or the payload on record (when
    /// there is one) differs. A batch whose changes the history lacks takes them from the
    /// payload. Returns the entry marked.
    pub fn on_finalized(
        &mut self,
        msg_id: &[u8; 32],
        tx_hash: [u8; 32],
        payload: &[u8],
        l1_slot: u64,
    ) -> Result<Option<&Published>, FinalizeRefused> {
        // The message id is ours (it is in the history, signed by our key); the payload on
        // record, when there is one, must be the one finalized.
        if self.payload(msg_id).is_some_and(|p| p[..] != payload[..]) {
            return Err(FinalizeRefused::PayloadDiffers);
        }
        self.fill_changes(msg_id, payload);
        let Some(entry) = self.published_mut(msg_id) else {
            return Err(FinalizeRefused::NotOurs);
        };
        if matches!(entry.status, Status::Finalized { .. }) {
            return Ok(None);
        }
        entry.status = Status::Finalized { l1_slot };
        entry.tx_hash = tx_hash;
        Ok(self.history.published.iter().find(|p| p.msg_id == *msg_id))
    }

    /// The L1 slot the finality scan starts at: after the last slot scanned, and no earlier
    /// than the oldest batch still waiting; `None` when nothing waits. A batch published
    /// before the slot was recorded borrows one from a finalized batch: slots are seconds, so
    /// its slot is about the finalized one's minus the seconds between the two publishes, with
    /// a margin; without any finalized batch such a batch is left to the SDK stream.
    pub fn finality_scan_from(&self) -> Option<u64> {
        let reference = self.history.published.iter().find_map(|p| match p.status {
            Status::Finalized { l1_slot } => Some((l1_slot, p.published_at_unix)),
            _ => None,
        });
        let oldest = self
            .history
            .published
            .iter()
            .filter(|p| !matches!(p.status, Status::Finalized { .. } | Status::Orphaned))
            .filter_map(|p| {
                p.published_at_slot.or_else(|| {
                    let (slot, unix) = reference?;
                    Some(
                        slot.saturating_sub(unix.saturating_sub(p.published_at_unix))
                            .saturating_sub(SLOT_ESTIMATE_MARGIN),
                    )
                })
            })
            .min()?;
        Some(match self.history.finality_scanned_to {
            Some(scanned) => oldest.max(scanned + 1),
            None => oldest,
        })
    }

    pub fn published_mut(&mut self, msg_id: &[u8; 32]) -> Option<&mut Published> {
        self.history
            .published
            .iter_mut()
            .find(|p| p.msg_id == *msg_id)
    }

    /// The newest batch in a Bedrock block (Adopted or Finalized).
    pub fn newest_adopted(&self) -> Option<&Published> {
        self.history
            .published
            .iter()
            .rev()
            .find(|p| matches!(p.status, Status::Adopted | Status::Finalized { .. }))
    }

    /// The newest Finalized batch, if any.
    pub fn newest_finalized(&self) -> Option<&Published> {
        self.history
            .published
            .iter()
            .rev()
            .find(|p| matches!(p.status, Status::Finalized { .. }))
    }

    /// The roots newest first, at most `window_size`: one entry per run of batches carrying
    /// the same root, the batch that produced it first (its changes are what made the root)
    /// and every batch of the run after it (heartbeats as a rule, carrying the root further).
    /// A root the tree comes back to later (a leaf inserted then removed empties its slot
    /// again) is a new run, so the window stays ordered by recency. Orphaned batches are left
    /// out.
    pub fn roots(&self, window_size: usize) -> Vec<RootEntry<'_>> {
        let mut entries: Vec<RootEntry<'_>> = vec![];
        for p in self
            .history
            .published
            .iter()
            .filter(|p| p.status != Status::Orphaned)
        {
            match entries.last_mut() {
                Some(entry) if entry.batches[0].root_after == p.root_after => entry.batches.push(p),
                _ => entries.push(RootEntry { batches: vec![p] }),
            }
        }
        entries.reverse();
        entries.truncate(window_size);
        entries
    }
}

/// Slots the finality scan starts early when a batch's slot is estimated from another's.
const SLOT_ESTIMATE_MARGIN: u64 = 600;

/// Why `Registry::on_finalized` left a batch as it was.
#[derive(Debug, PartialEq)]
pub enum FinalizeRefused {
    /// The message id is not in the history.
    NotOurs,
    /// The payload finalized is not the one published under that message id.
    PayloadDiffers,
}

/// A root of the valid-root window: the run of batches that carried it, the one that produced
/// it first.
pub struct RootEntry<'a> {
    pub batches: Vec<&'a Published>,
}

impl RootEntry<'_> {
    /// The batch whose changes made the root.
    pub fn produced_by(&self) -> &Published {
        self.batches[0]
    }

    /// `to_lez_block_id` of the newest batch carrying the root.
    pub fn carried_to_lez_block_id(&self) -> u64 {
        self.batches[self.batches.len() - 1].to_lez_block_id
    }
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::lez::Event;

    fn block(block_id: u64, events: Vec<Event>) -> Block {
        Block {
            block_id,
            timestamp_ms: 1_000,
            events,
            private_transaction_count: 0,
        }
    }

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

    #[test]
    fn test_roots_window_finality_marks_and_the_history_rebuilds_the_payloads() {
        let dir = std::env::temp_dir().join(format!("zone-roots-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut registry = Registry::open(&dir, [0; 32], 1, false).unwrap();
        let publish = |registry: &mut Registry, n: u8, root: u8| {
            let batch = Batch {
                from_lez_block_id: u64::from(n),
                to_lez_block_id: u64::from(n),
                changes: vec![],
                root_after: vec![root; 32],
            };
            registry.on_published([n; 32], [n; 32], &batch, Some(u64::from(n)));
        };
        // The inscription a published batch carried, as the history builds it again.
        let payload_of = |n: u8, root: u8| {
            Batch {
                from_lez_block_id: u64::from(n),
                to_lez_block_id: u64::from(n),
                changes: vec![],
                root_after: vec![root; 32],
            }
            .encode_to_vec()
        };
        // Root A, a heartbeat carrying A, root B, back to A (a leaf inserted then removed).
        publish(&mut registry, 1, 0xA);
        publish(&mut registry, 2, 0xA);
        publish(&mut registry, 3, 0xB);
        publish(&mut registry, 4, 0xA);
        let roots = registry.roots(10);
        let runs: Vec<(u8, u64, u64, usize)> = roots
            .iter()
            .map(|e| {
                (
                    e.produced_by().root_after[0],
                    e.produced_by().from_lez_block_id,
                    e.carried_to_lez_block_id(),
                    e.batches.len(),
                )
            })
            .collect();
        assert_eq!(runs, vec![(0xA, 4, 4, 1), (0xB, 3, 3, 1), (0xA, 1, 2, 2)]);
        assert_eq!(registry.roots(1)[0].produced_by().root_after[0], 0xA);
        // Finality: the oldest waiting batch is where the scan starts; a wrong payload is
        // refused; a batch marked final drops its payload and leaves the scan range.
        assert_eq!(registry.finality_scan_from(), Some(1));
        assert_eq!(
            registry.on_finalized(&[1; 32], [9; 32], &[0xB], 500).err(),
            Some(FinalizeRefused::PayloadDiffers)
        );
        assert_eq!(
            registry.on_finalized(&[7; 32], [9; 32], &[], 500).err(),
            Some(FinalizeRefused::NotOurs)
        );
        let marked = registry
            .on_finalized(&[1; 32], [9; 32], &payload_of(1, 0xA), 500)
            .unwrap()
            .unwrap();
        assert_eq!(marked.status, Status::Finalized { l1_slot: 500 });
        assert!(
            registry
                .on_finalized(&[1; 32], [9; 32], &payload_of(1, 0xA), 500)
                .unwrap()
                .is_none()
        );
        assert_eq!(registry.finality_scan_from(), Some(2));
        registry.history.finality_scanned_to = Some(600);
        assert_eq!(registry.finality_scan_from(), Some(601));
        // A batch without a slot borrows one from the finalized batch (slot 500, published
        // now): published 100 s earlier, its slot is about 400, the scan starts 600 early.
        registry.history.finality_scanned_to = None;
        let now = now_unix();
        registry.history.published[1].published_at_slot = None;
        registry.history.published[1].published_at_unix = now - 100;
        registry.history.published[0].published_at_unix = now;
        registry.history.published[2].published_at_slot = Some(5000);
        registry.history.published[3].published_at_slot = Some(5000);
        assert_eq!(registry.finality_scan_from(), Some(0));
        registry.history.published[0].status = Status::Finalized { l1_slot: 2000 };
        assert_eq!(registry.finality_scan_from(), Some(2000 - 100 - 600));
        registry.history.finality_scanned_to = Some(600);
        // A batch recorded without its changes learns them from the inscription on chain,
        // when span and root agree.
        registry.history.published[2].changes.clear();
        registry.history.published[2].change_count = 1;
        let on_chain = Batch {
            from_lez_block_id: 3,
            to_lez_block_id: 3,
            changes: vec![Change::insert(9, vec![0xC; 32])],
            root_after: vec![0xB; 32],
        }
        .encode_to_vec();
        assert_eq!(registry.payload(&[3; 32]), None);
        assert!(!registry.fill_changes(&[3; 32], &payload_of(3, 0xB)));
        assert!(registry.fill_changes(&[3; 32], &on_chain));
        assert_eq!(registry.history.published[2].changes[0].leaf_index, 9);
        assert_eq!(registry.payload(&[3; 32]), Some(on_chain));
        // The history survives a restart and still builds every batch's inscription.
        registry.save().unwrap();
        registry.close().unwrap();
        drop(registry);
        let registry = Registry::open(&dir, [0; 32], 1, false).unwrap();
        assert_eq!(registry.payload(&[1; 32]), Some(payload_of(1, 0xA)));
        assert_eq!(registry.payload(&[4; 32]), Some(payload_of(4, 0xA)));
        assert_eq!(registry.payload(&[7; 32]), None);
        assert_eq!(registry.history.finality_scanned_to, Some(600));
        drop(registry);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_reopen_keeps_the_pending_changes() {
        let dir = std::env::temp_dir().join(format!("zone-registry-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let batch = {
            let mut registry = Registry::open(&dir, [0; 32], 1, false).unwrap();
            registry
                .apply_blocks(&[
                    block(1, vec![registered(1, 0), registered(2, 1)]),
                    block(2, vec![registered(3, 2)]),
                ])
                .unwrap();
            registry.save().unwrap();
            let batch = registry.next_batch().unwrap();
            assert_eq!(batch.changes.len(), 3);
            registry.close().unwrap();
            batch
        };
        {
            let registry = Registry::open(&dir, [0; 32], 1, false).unwrap();
            assert_eq!(registry.next_batch().unwrap(), batch);
        }
        {
            let registry = Registry::open(&dir, [0; 32], 1, true).unwrap();
            assert!(registry.pending_changes.is_empty());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
