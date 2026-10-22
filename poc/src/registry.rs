//! The registry the sequencer holds and the HTTP API serves: the tree at the LEZ head, the
//! changes not yet in a batch, every batch published with the status of its Mantle
//! Transaction on Bedrock, and the tree at each published batch so proof paths can be served
//! at the newest root in a Bedrock block (or the newest Finalized one on request).

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::{
    batch::{Batch, Change},
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
    /// In finalized chain history: a valid root for the PoC.
    Finalized { l1_slot: u64 },
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
    pub published_at_unix: u64,
    #[serde(flatten)]
    pub status: Status,
}

/// What survives a restart (`history.json`); the trees are rebuilt from LEZ.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct History {
    pub first_lez_block_id: u64,
    /// Last LEZ block a published batch covers.
    pub covered_to: u64,
    pub published: Vec<Published>,
}

impl History {
    pub fn load(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        Ok(Some(
            serde_json::from_str(&text).context("parse history.json")?,
        ))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        fs::write(path, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("write {}", path.display()))
    }
}

pub struct Registry {
    /// `snapshots/` under the data dir: the reader state at each published batch.
    snapshot_dir: PathBuf,
    pub history: History,
    /// The tree at the latest LEZ block applied.
    pub head: Reader,
    /// Changes of the LEZ blocks after `covered_to`, in derivation-rule order.
    pub pending_changes: Vec<Change>,
    /// The tree right after the last block of each published batch, by message id.
    snapshots: HashMap<[u8; 32], Reader>,
    /// The payload of each published batch, by message id, to publish it again after a reorg.
    payloads: HashMap<[u8; 32], Vec<u8>>,
}

impl Registry {
    pub fn new(tree_id: [u8; 32], history: History, snapshot_dir: PathBuf) -> Self {
        Self {
            snapshot_dir,
            head: Reader::new(tree_id, history.first_lez_block_id.saturating_sub(1)),
            history,
            pending_changes: vec![],
            snapshots: HashMap::new(),
            payloads: HashMap::new(),
        }
    }

    fn snapshot_path(&self, msg_id: &[u8; 32]) -> PathBuf {
        self.snapshot_dir
            .join(format!("{}.json", hex::encode(msg_id)))
    }

    /// Store the head as the snapshot of `msg_id`, in memory and on disk.
    fn take_snapshot(&mut self, msg_id: [u8; 32]) {
        self.snapshots.insert(msg_id, self.head.clone());
        let path = self.snapshot_path(&msg_id);
        let written = fs::create_dir_all(&self.snapshot_dir).and_then(|()| {
            fs::write(
                &path,
                serde_json::to_string(&self.head.snapshot()).unwrap_or_default(),
            )
        });
        if let Err(e) = written {
            println!("snapshot {} not written: {e}", path.display());
        }
    }

    /// Load the snapshots of the published batches from disk; the head becomes the newest
    /// one, so only the LEZ blocks after `covered_to` have to be read again. Returns the
    /// block the head is at, or `None` when nothing was loaded (a fresh start, or snapshots
    /// missing: then the blocks are replayed).
    pub fn load_snapshots(&mut self) -> Option<u64> {
        let mut newest: Option<(u64, Reader)> = None;
        for published in &self.history.published {
            if published.status == Status::Orphaned {
                continue;
            }
            let path = self.snapshot_path(&published.msg_id);
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let reader = serde_json::from_str(&text)
                .map_err(|e| e.to_string())
                .and_then(|snapshot| Reader::from_snapshot(&snapshot));
            match reader {
                Ok(reader) => {
                    if newest
                        .as_ref()
                        .is_none_or(|(at, _)| reader.applied_block_id > *at)
                    {
                        newest = Some((reader.applied_block_id, reader.clone()));
                    }
                    self.snapshots.insert(published.msg_id, reader);
                }
                Err(e) => println!("snapshot {} unusable: {e}", path.display()),
            }
        }
        let (at, head) = newest?;
        if at != self.history.covered_to {
            // The newest snapshot is older than the last published batch: replay instead.
            self.snapshots.clear();
            return None;
        }
        self.head = head;
        Some(at)
    }

    /// Apply closed LEZ blocks to the head tree; blocks up to `covered_to` are a replay after
    /// a restart and take the snapshots of the published batches again.
    pub fn apply_blocks(&mut self, blocks: &[Block]) {
        for block in blocks {
            let changes = self.head.apply_block(block);
            if block.block_id > self.history.covered_to {
                self.pending_changes.extend(changes);
            }
            let at_this_block: Vec<[u8; 32]> = self
                .history
                .published
                .iter()
                .filter(|p| p.to_lez_block_id == block.block_id && p.status != Status::Orphaned)
                .map(|p| p.msg_id)
                .collect();
            for msg_id in at_this_block {
                self.take_snapshot(msg_id);
            }
        }
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
        payload: Vec<u8>,
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
            published_at_unix: now_unix(),
            status: Status::Pending,
        });
        self.history.covered_to = batch.to_lez_block_id;
        self.pending_changes.clear();
        self.take_snapshot(msg_id);
        self.payloads.insert(msg_id, payload);
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
        if let Some(snapshot) = self.snapshots.remove(&old) {
            self.snapshots.insert(new_msg_id, snapshot);
            let _ = fs::rename(self.snapshot_path(&old), self.snapshot_path(&new_msg_id));
        }
        if let Some(payload) = self.payloads.remove(&old) {
            self.payloads.insert(new_msg_id, payload);
        }
    }

    pub fn payload(&self, msg_id: &[u8; 32]) -> Option<&Vec<u8>> {
        self.payloads.get(msg_id)
    }

    pub fn published_mut(&mut self, msg_id: &[u8; 32]) -> Option<&mut Published> {
        self.history
            .published
            .iter_mut()
            .find(|p| p.msg_id == *msg_id)
    }

    /// The newest batch in a Bedrock block (Adopted or Finalized): the RFC's reference for
    /// Zone indexers, finality being the later audit.
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

    /// The tree at the newest root in a Bedrock block, or at the newest Finalized root when
    /// `finalized_only`: what proof paths are served against.
    pub fn served(&self, finalized_only: bool) -> Option<(&Published, &Reader)> {
        let published = if finalized_only {
            self.newest_finalized()?
        } else {
            self.newest_adopted()?
        };
        let reader = self.snapshots.get(&published.msg_id)?;
        Some((published, reader))
    }

    /// The published batches newest first, at most `window_size`, Orphaned ones left out.
    pub fn roots(&self, window_size: usize) -> Vec<&Published> {
        self.history
            .published
            .iter()
            .rev()
            .filter(|p| p.status != Status::Orphaned)
            .take(window_size)
            .collect()
    }
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
