//! The registry the sequencer holds and the HTTP API serves: the tree at the LEZ head, the
//! changes not yet in a batch, every batch published with the status of its Mantle
//! Transaction on Bedrock, and the tree at each published batch so proof paths can be served
//! at the newest Finalized root.

use std::{collections::HashMap, fs, path::Path};

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

/// One batch this sequencer published, tracked by the hash of its Mantle Transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Published {
    #[serde(with = "hex::serde")]
    pub tx_hash: [u8; 32],
    /// The inscription's message id in the channel.
    #[serde(with = "hex::serde")]
    pub msg_id: [u8; 32],
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
    pub history: History,
    /// The tree at the latest LEZ block applied.
    pub head: Reader,
    /// Changes of the LEZ blocks after `covered_to`, in derivation-rule order.
    pub pending_changes: Vec<Change>,
    /// The tree right after the last block of each published batch, by transaction hash.
    snapshots: HashMap<[u8; 32], Reader>,
    /// The payload of each published batch, to publish it again after a reorg.
    payloads: HashMap<[u8; 32], Vec<u8>>,
}

impl Registry {
    pub fn new(tree_id: [u8; 32], history: History) -> Self {
        Self {
            head: Reader::new(tree_id, history.first_lez_block_id.saturating_sub(1)),
            history,
            pending_changes: vec![],
            snapshots: HashMap::new(),
            payloads: HashMap::new(),
        }
    }

    /// Apply closed LEZ blocks to the head tree; blocks up to `covered_to` are a replay after
    /// a restart and take the snapshots of the published batches again.
    pub fn apply_blocks(&mut self, blocks: &[Block]) {
        for block in blocks {
            let changes = self.head.apply_block(block);
            if block.block_id > self.history.covered_to {
                self.pending_changes.extend(changes);
            }
            for published in &self.history.published {
                if published.to_lez_block_id == block.block_id
                    && published.status != Status::Orphaned
                {
                    self.snapshots.insert(published.tx_hash, self.head.clone());
                }
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
        tx_hash: [u8; 32],
        msg_id: [u8; 32],
        batch: &Batch,
        payload: Vec<u8>,
    ) {
        let mut root_after = [0u8; 32];
        root_after.copy_from_slice(&batch.root_after);
        self.history.published.push(Published {
            tx_hash,
            msg_id,
            from_lez_block_id: batch.from_lez_block_id,
            to_lez_block_id: batch.to_lez_block_id,
            root_after,
            change_count: batch.changes.len(),
            published_at_unix: now_unix(),
            status: Status::Pending,
        });
        self.history.covered_to = batch.to_lez_block_id;
        self.pending_changes.clear();
        self.snapshots.insert(tx_hash, self.head.clone());
        self.payloads.insert(tx_hash, payload);
    }

    /// A reorg dropped the transaction `old`: the same batch goes out again as `new`.
    pub fn on_republished(&mut self, old: [u8; 32], new_tx_hash: [u8; 32], new_msg_id: [u8; 32]) {
        let Some(entry) = self.published_mut(&old) else {
            return;
        };
        let mut again = entry.clone();
        entry.status = Status::Orphaned;
        again.tx_hash = new_tx_hash;
        again.msg_id = new_msg_id;
        again.status = Status::Pending;
        again.published_at_unix = now_unix();
        self.history.published.push(again);
        if let Some(snapshot) = self.snapshots.remove(&old) {
            self.snapshots.insert(new_tx_hash, snapshot);
        }
        if let Some(payload) = self.payloads.remove(&old) {
            self.payloads.insert(new_tx_hash, payload);
        }
    }

    pub fn payload(&self, tx_hash: &[u8; 32]) -> Option<&Vec<u8>> {
        self.payloads.get(tx_hash)
    }

    pub fn published_mut(&mut self, tx_hash: &[u8; 32]) -> Option<&mut Published> {
        self.history
            .published
            .iter_mut()
            .find(|p| p.tx_hash == *tx_hash)
    }

    /// The newest Finalized batch, if any.
    pub fn newest_finalized(&self) -> Option<&Published> {
        self.history
            .published
            .iter()
            .rev()
            .find(|p| matches!(p.status, Status::Finalized { .. }))
    }

    /// The tree at the newest Finalized root: what proof paths are served against.
    pub fn served(&self) -> Option<(&Published, &Reader)> {
        let published = self.newest_finalized()?;
        let reader = self.snapshots.get(&published.tx_hash)?;
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
