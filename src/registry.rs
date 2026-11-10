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

use rln::prelude::{Fr, PoseidonHash};
use zerokit_utils::merkle_tree::{OptimalMerkleConfig, OptimalMerkleTree, ZerokitMerkleTree};

use crate::{
    batch::{Batch, Change, Kind},
    lez::Block,
    reader::{EMPTY_ROOT, Reader, TREE_DEPTH, fr_from_bytes_le, fr_to_bytes_le},
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
    /// The changes the batch carried, in order. Empty while the root moved only for a batch
    /// published before the history kept them (`History::lacks_changes`).
    #[serde(default)]
    pub changes: Vec<ChangeRecord>,
    pub published_at_unix: u64,
    /// The sequencer's L1 slot at publish time: where the finality scan starts looking for
    /// it. `None` for a batch published before the slot was recorded, and for an indexer's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at_slot: Option<u64>,
    #[serde(flatten)]
    pub status: Status,
    /// What an indexer found checking the batch against its own derivation; `None` for the
    /// sequencer's own batches.
    #[serde(default)]
    pub verification: Option<Verification>,
}

/// An indexer's verdict on a batch read from the channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verification {
    /// Contiguous with the previous batch, its changes replay to its root, and that root is
    /// the indexer's own at `to_lez_block_id`.
    Verified,
    /// A check failed; the reason says which. The batch is kept, so the history stays a
    /// faithful copy of the channel, but it did not move the replay tree.
    Rejected(String),
    /// The indexer has not read LEZ up to `to_lez_block_id` yet; checked again then.
    Pending,
}

/// The published batches.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct History {
    pub first_lez_block_id: u64,
    pub published: Vec<Published>,
    /// The L1 slot the finality scan has read the finalized blocks up to.
    #[serde(default)]
    pub finality_scanned_to: Option<u64>,
}

impl History {
    /// Last LEZ block a batch covers: the newest published (the sequencer; an orphaned one
    /// still covers, it goes out again) or verified (the indexer); the block before the first
    /// one while there is none.
    pub fn covered_to(&self) -> u64 {
        self.published
            .iter()
            .rev()
            .find(|p| {
                p.verification
                    .as_ref()
                    .is_none_or(|v| *v == Verification::Verified)
            })
            .map_or(self.first_lez_block_id.saturating_sub(1), |p| {
                p.to_lez_block_id
            })
    }

    /// Whether the batch at `at` moved the root without the history having its changes: one
    /// published before the history kept them (a batch without changes keeps the root).
    pub fn lacks_changes(&self, at: usize) -> bool {
        let p = &self.published[at];
        p.changes.is_empty() && p.root_after != self.root_before(at)
    }

    /// The root the batch at `at` started from: the newest batch's before it that counts, the
    /// empty tree's for the first.
    fn root_before(&self, at: usize) -> [u8; 32] {
        self.published[..at]
            .iter()
            .rev()
            .find(|p| p.counts())
            .map_or(*EMPTY_ROOT, |p| p.root_after)
    }
}

impl Published {
    /// Whether the batch is part of the tree's history: not orphaned, and verified when an
    /// indexer judged it.
    fn counts(&self) -> bool {
        self.status != Status::Orphaned
            && self
                .verification
                .as_ref()
                .is_none_or(|v| *v == Verification::Verified)
    }
}

pub struct Registry {
    pub history: History,
    /// The tree at the latest LEZ block applied.
    pub head: Reader,
    /// Changes of the LEZ blocks after `covered_to`, in derivation-rule order, each with the
    /// block that made it: the next batch's content for the sequencer, the head's lead over
    /// the channel for the indexer; trimmed as `covered_to` advances.
    pub pending_changes: Vec<(u64, Change)>,
    /// The Zone SDK checkpoint, as JSON.
    checkpoint: Option<Value>,
    /// Indexer: the tree the verified batches' changes replay to, in memory, rebuilt from the
    /// history at start. Its root after a batch must be the batch's `root_after`.
    replay: Option<OptimalMerkleTree<PoseidonHash>>,
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
                published: vec![],
                finality_scanned_to: None,
            },
        };
        let checkpoint = extra.get("checkpoint").filter(|c| !c.is_null()).cloned();
        // The changes applied but not yet published survive a restart with the tree they
        // changed; a rebuild derives them again from LEZ.
        let pending_changes = match extra.get("pending_changes") {
            Some(encoded) if !rebuild => {
                let encoded: Vec<PendingChangeRecord> = serde_json::from_value(encoded.clone())
                    .context("pending changes in the tree store")?;
                encoded
                    .iter()
                    .map(|record| {
                        let bytes =
                            hex::decode(record.change()).context("pending change is not hex")?;
                        let change = Change::decode(bytes.as_slice()).context("pending change")?;
                        // A record written before the block was kept counts as just after
                        // `covered_to`, the only thing known about it.
                        Ok((record.block().unwrap_or(history.covered_to() + 1), change))
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
            replay: None,
        })
    }

    /// Turn this registry into an indexer's: a replay tree rebuilt from the verified batches
    /// of the history, so `observe_batch` can check the next ones.
    pub fn start_replay(&mut self) -> Result<()> {
        let mut tree = OptimalMerkleTree::<PoseidonHash>::new(
            TREE_DEPTH,
            Fr::default(),
            OptimalMerkleConfig::default(),
        )
        .context("replay tree")?;
        for p in self
            .history
            .published
            .iter()
            .filter(|p| p.verification == Some(Verification::Verified))
        {
            apply_changes(&mut tree, &p.changes)?;
        }
        self.replay = Some(tree);
        Ok(())
    }

    /// Indexer: a batch read from the channel (`payload` its inscription, finalized at
    /// `l1_slot`), checked and kept. A message id already on record is checked again only
    /// while `Pending`. Returns the entry as kept.
    pub fn observe_batch(
        &mut self,
        msg_id: [u8; 32],
        tx_hash: [u8; 32],
        payload: &[u8],
        l1_slot: u64,
    ) -> Result<&Published> {
        let batch = Batch::decode(payload).context("the inscription is not a batch")?;
        let changes: Vec<ChangeRecord> = batch
            .changes
            .iter()
            .filter_map(ChangeRecord::from_change)
            .collect();
        let mut root_after = [0u8; 32];
        if batch.root_after.len() == 32 {
            root_after.copy_from_slice(&batch.root_after);
        }
        let position = self
            .history
            .published
            .iter()
            .position(|p| p.msg_id == msg_id);
        if let Some(at) = position
            && self.history.published[at].verification != Some(Verification::Pending)
        {
            return Ok(&self.history.published[at]);
        }
        let verification = self.verify(&batch, &changes, root_after)?;
        if verification == Verification::Verified {
            self.trim_pending(batch.to_lez_block_id);
        }
        let entry = Published {
            msg_id,
            tx_hash,
            from_lez_block_id: batch.from_lez_block_id,
            to_lez_block_id: batch.to_lez_block_id,
            root_after,
            changes,
            published_at_unix: now_unix(),
            published_at_slot: None,
            status: Status::Finalized { l1_slot },
            verification: Some(verification),
        };
        match position {
            Some(at) => {
                self.history.published[at] = entry;
                Ok(&self.history.published[at])
            }
            None => {
                self.history.published.push(entry);
                Ok(self.history.published.last().expect("just pushed"))
            }
        }
    }

    /// The three checks of a batch: contiguous with what was verified so far, its changes
    /// replay to its root, and that root is this indexer's own tree at `to_lez_block_id`.
    /// Only a verified batch moves the replay tree.
    fn verify(
        &mut self,
        batch: &Batch,
        changes: &[ChangeRecord],
        root_after: [u8; 32],
    ) -> Result<Verification> {
        let Some(own_root) = self.head.root_at(batch.to_lez_block_id) else {
            return Ok(Verification::Pending);
        };
        let covered_to = self.history.covered_to();
        if batch.from_lez_block_id != covered_to + 1 {
            return Ok(Verification::Rejected(format!(
                "covers LEZ blocks from {}, the previous batch covered to {covered_to}",
                batch.from_lez_block_id
            )));
        }
        if changes.len() != batch.changes.len() {
            return Ok(Verification::Rejected("a change has no kind".into()));
        }
        let replay = self
            .replay
            .as_mut()
            .context("indexer replay tree not started")?;
        let mut trial = replay.clone();
        apply_changes(&mut trial, changes)?;
        let replayed = fr_to_bytes_le(&trial.root());
        if replayed != root_after {
            return Ok(Verification::Rejected(format!(
                "the changes replay to root {}, the batch claims {}",
                hex::encode(replayed),
                hex::encode(root_after)
            )));
        }
        if own_root != root_after {
            return Ok(Verification::Rejected(format!(
                "the tree derived from LEZ is {} at block {}, the batch claims {}",
                hex::encode(own_root),
                batch.to_lez_block_id,
                hex::encode(root_after)
            )));
        }
        *replay = trial;
        Ok(Verification::Verified)
    }

    /// Indexer: check again, in channel order, the batches left `Pending` because LEZ had not
    /// been read far enough; stops at the first still pending (the later ones chain on it).
    /// Returns how many got a verdict.
    pub fn recheck_pending(&mut self) -> Result<usize> {
        let mut decided = 0;
        let pending: Vec<usize> = self
            .history
            .published
            .iter()
            .enumerate()
            .filter(|(_, p)| p.verification == Some(Verification::Pending))
            .map(|(at, _)| at)
            .collect();
        for at in pending {
            let p = &self.history.published[at];
            let Some(payload) = self.payload(&p.msg_id) else {
                continue;
            };
            let batch = Batch::decode(payload.as_slice()).context("a batch on record")?;
            let changes = p.changes.clone();
            let root_after = p.root_after;
            let verification = self.verify(&batch, &changes, root_after)?;
            if verification == Verification::Pending {
                break;
            }
            self.history.published[at].verification = Some(verification);
            self.trim_pending(self.history.covered_to());
            decided += 1;
        }
        Ok(decided)
    }

    /// Indexer: the root the replay tree is at, the last verified batch's.
    pub fn replay_root(&self) -> Option<[u8; 32]> {
        self.replay
            .as_ref()
            .map(|tree| fr_to_bytes_le(&tree.root()))
    }

    /// Write the reader's state, the history, the pending changes and the checkpoint into the
    /// tree store.
    pub fn save(&mut self) -> Result<()> {
        let pending_changes: Vec<PendingChangeRecord> = self
            .pending_changes
            .iter()
            .map(|(block, change)| PendingChangeRecord::WithBlock {
                block: *block,
                change: hex::encode(change.encode_to_vec()),
            })
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
        let covered_to = self.history.covered_to();
        for block in blocks {
            let changes = self.head.apply_block(block)?;
            if block.block_id > covered_to {
                self.pending_changes
                    .extend(changes.into_iter().map(|change| (block.block_id, change)));
            }
        }
        Ok(())
    }

    /// The next batch: every LEZ block after `covered_to` up to the head, with the pending
    /// changes and the root after them; `None` when the head is already covered.
    pub fn next_batch(&self) -> Option<Batch> {
        let covered_to = self.history.covered_to();
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
            changes: batch
                .changes
                .iter()
                .filter_map(ChangeRecord::from_change)
                .collect(),
            published_at_unix: now_unix(),
            published_at_slot,
            status: Status::Pending,
            verification: None,
        });
        self.trim_pending(batch.to_lez_block_id);
    }

    /// A batch covers the LEZ blocks up to `to`: the changes of those blocks are no longer
    /// pending.
    fn trim_pending(&mut self, to: u64) {
        self.pending_changes.retain(|(block, _)| *block > to);
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
        let at = self
            .history
            .published
            .iter()
            .position(|p| p.msg_id == *msg_id)?;
        if self.history.lacks_changes(at) {
            return None;
        }
        let p = &self.history.published[at];
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
        let Some(at) = self
            .history
            .published
            .iter()
            .position(|p| p.msg_id == *msg_id)
        else {
            return false;
        };
        if !self.history.lacks_changes(at) {
            return false;
        }
        let Ok(batch) = Batch::decode(payload) else {
            return false;
        };
        let entry = &mut self.history.published[at];
        if batch.from_lez_block_id != entry.from_lez_block_id
            || batch.to_lez_block_id != entry.to_lez_block_id
            || batch.root_after[..] != entry.root_after[..]
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
            .enumerate()
            .filter(|(at, _)| self.history.lacks_changes(*at))
            .filter_map(|(_, p)| match p.status {
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

    /// The newest batch published, whatever its status but orphaned.
    pub fn newest_published(&self) -> Option<&Published> {
        self.history
            .published
            .iter()
            .rev()
            .find(|p| p.status != Status::Orphaned)
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

/// A pending change as the tree store keeps it: the LEZ block that made it and the change,
/// prost in hex. A store written before the block was kept holds the bare hex string.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum PendingChangeRecord {
    WithBlock { block: u64, change: String },
    Bare(String),
}

impl PendingChangeRecord {
    fn block(&self) -> Option<u64> {
        match self {
            Self::WithBlock { block, .. } => Some(*block),
            Self::Bare(_) => None,
        }
    }

    fn change(&self) -> &str {
        match self {
            Self::WithBlock { change, .. } | Self::Bare(change) => change,
        }
    }
}

/// Apply a batch's changes to a tree: an insert sets the leaf, a removal empties it.
fn apply_changes(
    tree: &mut OptimalMerkleTree<PoseidonHash>,
    changes: &[ChangeRecord],
) -> Result<()> {
    for change in changes {
        let leaf_index = change.leaf_index as usize;
        match change.kind {
            ChangeKind::Insert => {
                let hex = change
                    .rate_commitment
                    .as_deref()
                    .context("an insert without a leaf")?;
                let bytes: [u8; 32] = hex::decode(hex)
                    .ok()
                    .and_then(|v| v.try_into().ok())
                    .context("a leaf that is not 32 bytes of hex")?;
                let leaf = fr_from_bytes_le(&bytes).map_err(|e| anyhow::anyhow!(e))?;
                tree.set(leaf_index, leaf)
                    .with_context(|| format!("insert leaf {leaf_index}"))?;
            }
            ChangeKind::Remove => tree
                .delete(leaf_index)
                .with_context(|| format!("remove leaf {leaf_index}"))?,
        }
    }
    Ok(())
}

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
        // Batch `n` covers block `n` with one change (the roots here are stand-ins).
        let batch_of = |n: u8, root: u8| Batch {
            from_lez_block_id: u64::from(n),
            to_lez_block_id: u64::from(n),
            changes: vec![Change::insert(u64::from(n), vec![root; 32])],
            root_after: vec![root; 32],
        };
        let publish = |registry: &mut Registry, n: u8, root: u8| {
            registry.on_published([n; 32], [n; 32], &batch_of(n, root), Some(u64::from(n)));
        };
        // The inscription a published batch carried, as the history builds it again.
        let payload_of = |n: u8, root: u8| batch_of(n, root).encode_to_vec();
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
        let on_chain = Batch {
            from_lez_block_id: 3,
            to_lez_block_id: 3,
            changes: vec![Change::insert(9, vec![0xC; 32])],
            root_after: vec![0xB; 32],
        }
        .encode_to_vec();
        assert_eq!(registry.payload(&[3; 32]), None);
        assert!(!registry.fill_changes(&[3; 32], &payload_of(3, 0xC)));
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
    fn test_indexer_checks_the_batches_against_its_own_derivation() {
        let dir = std::env::temp_dir().join(format!("zone-indexer-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut registry = Registry::open(&dir, [0; 32], 1, false).unwrap();
        registry.start_replay().unwrap();
        // The indexer has read LEZ block 1 (two registrations).
        registry
            .apply_blocks(&[block(1, vec![registered(1, 0), registered(2, 1)])])
            .unwrap();
        let root_1 = registry.head.root();
        let batch = |from: u64, to: u64, changes: Vec<Change>, root: [u8; 32]| {
            Batch {
                from_lez_block_id: from,
                to_lez_block_id: to,
                changes,
                root_after: root.to_vec(),
            }
            .encode_to_vec()
        };
        let changes_1: Vec<Change> = registry
            .pending_changes
            .iter()
            .map(|(_, c)| c.clone())
            .collect();
        assert_eq!(changes_1.len(), 2);

        // A batch claiming the right root with the right changes: verified.
        let ok = registry
            .observe_batch(
                [1; 32],
                [1; 32],
                &batch(1, 1, changes_1.clone(), root_1),
                100,
            )
            .unwrap();
        assert_eq!(ok.verification, Some(Verification::Verified));
        assert_eq!(registry.history.covered_to(), 1);
        assert_eq!(registry.replay_root(), Some(root_1));
        // The changes the batch covered are no longer the head's lead over the channel.
        assert!(registry.pending_changes.is_empty());

        // A batch for blocks the indexer has not read yet: pending, then checked.
        let pending = registry
            .observe_batch(
                [2; 32],
                [2; 32],
                &batch(2, 3, vec![Change::remove(0)], [7; 32]),
                200,
            )
            .unwrap();
        assert_eq!(pending.verification, Some(Verification::Pending));
        registry
            .apply_blocks(&[block(2, vec![]), block(3, vec![registered(3, 2)])])
            .unwrap();
        assert_eq!(registry.recheck_pending().unwrap(), 1);
        let rejected = &registry.history.published[1];
        assert!(matches!(
            rejected.verification,
            Some(Verification::Rejected(ref reason)) if reason.contains("replay to root")
        ));
        // A rejected batch moves nothing: the next honest batch still chains on block 1.
        assert_eq!(registry.history.covered_to(), 1);
        assert_eq!(registry.replay_root(), Some(root_1));
        let root_3 = registry.head.root();
        assert_eq!(registry.pending_changes.len(), 1);
        let changes_3: Vec<Change> = registry
            .pending_changes
            .iter()
            .map(|(_, c)| c.clone())
            .collect();
        let ok = registry
            .observe_batch([3; 32], [3; 32], &batch(2, 3, changes_3, root_3), 300)
            .unwrap();
        assert_eq!(ok.verification, Some(Verification::Verified));
        assert_eq!(registry.replay_root(), Some(root_3));
        // Seen again, a decided batch is left as it is.
        let again = registry
            .observe_batch([3; 32], [3; 32], &batch(2, 3, vec![], [9; 32]), 300)
            .unwrap();
        assert_eq!(again.verification, Some(Verification::Verified));
        registry.close().unwrap();
        drop(registry);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_pending_change_records_read_the_bare_format_too() {
        let records: Vec<PendingChangeRecord> =
            serde_json::from_str(r#"["0a00", {"block": 7, "change": "1200"}]"#).unwrap();
        assert_eq!(records[0].block(), None);
        assert_eq!(records[0].change(), "0a00");
        assert_eq!(records[1].block(), Some(7));
        assert_eq!(records[1].change(), "1200");
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
