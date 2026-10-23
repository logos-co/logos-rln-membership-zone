//! A reader (RFC "Derivation Rules"): holds the tree and applies the rules to the registration
//! program's events of every LEZ block. The Zone sequencer is a reader; so is every Zone
//! indexer, which is why they hold the same roots. The tree is a sled-backed Zerokit
//! `PmTree`; the reader's state beside it (live leaves, removed leaves, counters, the block it
//! is at) and whatever its owner wants kept with it live in the tree store's metadata, so a
//! restart opens the store and continues.

use std::{collections::HashMap, path::Path};

use anyhow::{Context as _, Result, anyhow};
use rln::prelude::{
    CanonicalDeserialize, CanonicalSerialize, Fr, Hasher, PmTree, PmTreeSledConfig, PoseidonHash,
    SledDB,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zerokit_utils::merkle_tree::{ZerokitMerkleProof, ZerokitMerkleTree};

use crate::{
    batch::Change,
    lez::{Block, Event},
};

pub const TREE_DEPTH: usize = 20;

/// 32-byte little-endian representation of a field element (RLN-API Appendix B form).
pub fn fr_to_bytes_le(fr: &Fr) -> [u8; 32] {
    let mut out = [0u8; 32];
    fr.serialize_compressed(&mut out[..])
        .expect("an Fr is 32 bytes");
    out
}

/// The field element 32 little-endian bytes encode, if they are below the modulus.
pub fn fr_from_bytes_le(bytes: &[u8; 32]) -> Result<Fr, String> {
    Fr::deserialize_compressed(&bytes[..]).map_err(|e| format!("not a field element: {e}"))
}

/// A live leaf as the reader tracks it; the expiry comes from the events, not from the batches.
#[derive(Debug, Clone, Copy)]
pub struct Leaf {
    pub leaf_index: u64,
    pub expiry_timestamp_ms: u64,
    /// The leaf value, `poseidon(id_commitment, rate_limit)`.
    pub rate_commitment: [u8; 32],
}

/// Why a leaf left the tree. Read from the events and used for status only; batches do not
/// carry it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Removed {
    Expired,
    Slashed,
}

/// Counts of what the derivation rules did so far, for monitoring.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Stats {
    pub registered: u64,
    pub extended: u64,
    pub erased: u64,
    pub slashed: u64,
    /// Leaves removed because their expiry passed on the LEZ clock.
    pub removed_expired: u64,
    /// Leaves removed by a slash while live.
    pub removed_slashed: u64,
    /// Events of other registries (another `tree_id`), ignored.
    pub other_tree: u64,
}

/// A Merkle proof path at one root, in RLN-API form.
pub struct ProofPath {
    pub leaf_index: u64,
    pub rate_commitment: [u8; 32],
    pub path_elements: Vec<[u8; 32]>,
    pub identity_path_index: Vec<u8>,
}

/// What the tree store's metadata holds: the reader's state and its owner's `extra`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Meta {
    tree_id: String,
    applied_block_id: u64,
    applied_timestamp_ms: u64,
    stats: Stats,
    /// `(id_commitment, leaf_index, expiry_timestamp_ms, rate_commitment)`, hex.
    live: Vec<(String, u64, u64, String)>,
    /// `(id_commitment, cause)`.
    removed: Vec<(String, Removed)>,
    #[serde(default)]
    extra: Value,
}

pub struct Reader {
    /// The registry this reader follows; events of other registries are ignored.
    tree_id: [u8; 32],
    tree: PmTree<SledDB, PoseidonHash>,
    /// Live leaves by identity commitment.
    live: HashMap<[u8; 32], Leaf>,
    removed: HashMap<[u8; 32], Removed>,
    pub stats: Stats,
    /// Last LEZ `block_id` applied.
    pub applied_block_id: u64,
    /// `timestamp_ms` of the last LEZ block applied: the clock this reader's tree is at.
    pub applied_timestamp_ms: u64,
}

impl Reader {
    /// Open the tree store at `dir` (created when missing). `applied_block_id` is where a
    /// fresh reader starts; a store that already holds a reader keeps its own. Returns the
    /// `extra` its owner persisted last.
    pub fn open(dir: &Path, tree_id: [u8; 32], applied_block_id: u64) -> Result<(Self, Value)> {
        let config = PmTreeSledConfig::new()
            .path(dir.to_path_buf())
            .temporary(false)
            .build()
            .with_context(|| format!("tree store at {}", dir.display()))?;
        Self::open_with(config, tree_id, applied_block_id)
    }

    /// A throwaway store, for tests.
    #[cfg(test)]
    pub fn open_temporary(tree_id: [u8; 32], applied_block_id: u64) -> Result<(Self, Value)> {
        let config = PmTreeSledConfig::new().temporary(true).build()?;
        Self::open_with(config, tree_id, applied_block_id)
    }

    fn open_with(
        config: PmTreeSledConfig,
        tree_id: [u8; 32],
        applied_block_id: u64,
    ) -> Result<(Self, Value)> {
        let tree = <PmTree<SledDB, PoseidonHash> as ZerokitMerkleTree>::new(
            TREE_DEPTH,
            Fr::default(),
            config,
        )
        .context("open the tree store")?;
        let mut reader = Self {
            tree_id,
            tree,
            live: HashMap::new(),
            removed: HashMap::new(),
            stats: Stats::default(),
            applied_block_id,
            applied_timestamp_ms: 0,
        };
        let bytes = reader.tree.metadata().unwrap_or_default();
        if bytes.is_empty() {
            return Ok((reader, Value::Null));
        }
        let meta: Meta = serde_json::from_slice(&bytes).context("tree store metadata")?;
        if meta.tree_id != hex::encode(tree_id) {
            return Err(anyhow!(
                "the tree store follows registry {}, not {}",
                meta.tree_id,
                hex::encode(tree_id)
            ));
        }
        for (id, leaf_index, expiry_timestamp_ms, rate_commitment) in &meta.live {
            reader.live.insert(
                parse_32(id)?,
                Leaf {
                    leaf_index: *leaf_index,
                    expiry_timestamp_ms: *expiry_timestamp_ms,
                    rate_commitment: parse_32(rate_commitment)?,
                },
            );
        }
        for (id, cause) in &meta.removed {
            reader.removed.insert(parse_32(id)?, *cause);
        }
        reader.stats = meta.stats;
        reader.applied_block_id = meta.applied_block_id;
        reader.applied_timestamp_ms = meta.applied_timestamp_ms;
        Ok((reader, meta.extra))
    }

    /// Write the reader's state and its owner's `extra` into the tree store.
    pub fn persist(&mut self, extra: Value) -> Result<()> {
        let meta = Meta {
            tree_id: hex::encode(self.tree_id),
            applied_block_id: self.applied_block_id,
            applied_timestamp_ms: self.applied_timestamp_ms,
            stats: self.stats,
            live: self
                .live_leaves()
                .into_iter()
                .map(|(index, id, expiry)| {
                    let leaf = self.live[&id];
                    (
                        hex::encode(id),
                        index,
                        expiry,
                        hex::encode(leaf.rate_commitment),
                    )
                })
                .collect(),
            removed: self
                .removed_leaves()
                .into_iter()
                .map(|(id, cause)| (hex::encode(id), cause))
                .collect(),
            extra,
        };
        self.tree
            .set_metadata(&serde_json::to_vec(&meta)?)
            .context("write the tree store metadata")
    }

    /// Flush the store to disk.
    pub fn close(&mut self) -> Result<()> {
        self.tree.close().context("close the tree store")
    }

    pub fn root(&self) -> [u8; 32] {
        fr_to_bytes_le(&self.tree.root())
    }

    pub fn live_count(&self) -> usize {
        self.live.len()
    }

    /// Live leaves by `leaf_index`: `(leaf_index, id_commitment, expiry_timestamp_ms)`.
    pub fn live_leaves(&self) -> Vec<(u64, [u8; 32], u64)> {
        let mut leaves: Vec<_> = self
            .live
            .iter()
            .map(|(id, leaf)| (leaf.leaf_index, *id, leaf.expiry_timestamp_ms))
            .collect();
        leaves.sort();
        leaves
    }

    /// Removed leaves: `(id_commitment, cause)`.
    pub fn removed_leaves(&self) -> Vec<([u8; 32], Removed)> {
        let mut removed: Vec<_> = self.removed.iter().map(|(id, c)| (*id, *c)).collect();
        removed.sort_by_key(|(id, _)| *id);
        removed
    }

    /// Apply the derivation rules to the events of one LEZ block and return the changes, in
    /// order.
    pub fn apply_block(&mut self, block: &Block) -> Result<Vec<Change>> {
        let mut changes = vec![];
        for event in &block.events {
            match event {
                Event::Registered {
                    tree_id,
                    id_commitment,
                    rate_limit,
                    expiry_timestamp_ms,
                    leaf_index,
                } if *tree_id == self.tree_id => {
                    self.stats.registered += 1;
                    // Leaf = rate commitment = poseidon(identity_commitment, rate_limit).
                    let leaf = rate_commitment(id_commitment, *rate_limit)?;
                    self.tree
                        .set(*leaf_index as usize, leaf)
                        .with_context(|| format!("set leaf {leaf_index}"))?;
                    self.live.insert(
                        *id_commitment,
                        Leaf {
                            leaf_index: *leaf_index,
                            expiry_timestamp_ms: *expiry_timestamp_ms,
                            rate_commitment: fr_to_bytes_le(&leaf),
                        },
                    );
                    changes.push(Change::insert(*leaf_index, fr_to_bytes_le(&leaf).to_vec()));
                }
                Event::Extended {
                    tree_id,
                    id_commitment,
                    expiry_timestamp_ms,
                } if *tree_id == self.tree_id => {
                    self.stats.extended += 1;
                    // No change: the tree is untouched, only the expiry the reader tracks moves.
                    if let Some(leaf) = self.live.get_mut(id_commitment) {
                        leaf.expiry_timestamp_ms = *expiry_timestamp_ms;
                    }
                }
                // Normally no change: `Erase` needs `now >= expiry + withdrawal delay`, and
                // the previous block, whose timestamp is `now`, already expired the leaf.
                // A still-live leaf means the program allowed an early `Erase`; remove it.
                Event::Erased {
                    tree_id,
                    id_commitment,
                } if *tree_id == self.tree_id => {
                    self.stats.erased += 1;
                    changes.extend(self.remove(*id_commitment, Removed::Expired)?);
                }
                Event::Slashed {
                    tree_id,
                    id_commitment,
                } if *tree_id == self.tree_id => {
                    self.stats.slashed += 1;
                    match self.remove(*id_commitment, Removed::Slashed)? {
                        Some(change) => changes.push(change),
                        // Slashed during the withdrawal delay: the leaf already expired, so no
                        // change, but the membership is reported as slashed.
                        None => {
                            self.removed.insert(*id_commitment, Removed::Slashed);
                        }
                    }
                }
                _ => self.stats.other_tree += 1,
            }
        }
        // Expiry after the block's events, in leaf order, at most once per leaf.
        let mut expired: Vec<_> = self
            .live
            .iter()
            .filter(|(_, leaf)| leaf.expiry_timestamp_ms <= block.timestamp_ms)
            .map(|(key, leaf)| (leaf.leaf_index, *key))
            .collect();
        expired.sort();
        for (_, key) in expired {
            changes.extend(self.remove(key, Removed::Expired)?);
        }
        self.applied_block_id = block.block_id;
        self.applied_timestamp_ms = block.timestamp_ms;
        Ok(changes)
    }

    fn remove(&mut self, key: [u8; 32], cause: Removed) -> Result<Option<Change>> {
        let Some(leaf) = self.live.remove(&key) else {
            return Ok(None);
        };
        self.tree
            .delete(leaf.leaf_index as usize)
            .with_context(|| format!("delete leaf {}", leaf.leaf_index))?;
        match cause {
            Removed::Expired => self.stats.removed_expired += 1,
            Removed::Slashed => self.stats.removed_slashed += 1,
        }
        self.removed.insert(key, cause);
        Ok(Some(Change::remove(leaf.leaf_index)))
    }

    /// The Merkle proof path of the leaf at `leaf_index` at this reader's root, if the leaf is
    /// set: the sibling elements from the leaf up, and the side of each.
    pub fn proof_path(&self, leaf_index: u64) -> Option<ProofPath> {
        let index = usize::try_from(leaf_index).ok()?;
        let leaf = self.tree.get(index).ok()?;
        if leaf == Fr::default() {
            return None;
        }
        let proof = self.tree.proof(index).ok()?;
        Some(ProofPath {
            leaf_index,
            rate_commitment: fr_to_bytes_le(&leaf),
            path_elements: proof
                .get_path_elements()
                .iter()
                .map(fr_to_bytes_le)
                .collect(),
            identity_path_index: proof.get_path_index(),
        })
    }

    pub fn live_leaf(&self, id_commitment: &[u8; 32]) -> Option<Leaf> {
        self.live.get(id_commitment).copied()
    }

    pub fn removed(&self, id_commitment: &[u8; 32]) -> Option<Removed> {
        self.removed.get(id_commitment).copied()
    }
}

fn parse_32(hex_str: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(hex_str).context("tree store metadata: hex")?;
    <[u8; 32]>::try_from(bytes).map_err(|_| anyhow!("tree store metadata: not 32 bytes"))
}

/// `poseidon(id_commitment, rate_limit)`, the leaf of a membership.
fn rate_commitment(id_commitment: &[u8; 32], rate_limit: u64) -> Result<Fr> {
    let id_commitment =
        fr_from_bytes_le(id_commitment).map_err(|e| anyhow!("id_commitment: {e}"))?;
    Ok(Hasher::<PoseidonHash>::hash_pair(
        id_commitment,
        Fr::from(rate_limit),
    ))
}

#[cfg(test)]
mod test {
    use super::*;

    fn registered(id: u8, leaf_index: u64, expiry_timestamp_ms: u64) -> Event {
        let mut id_commitment = [0u8; 32];
        id_commitment[0] = id;
        Event::Registered {
            tree_id: [0; 32],
            id_commitment,
            rate_limit: 100,
            expiry_timestamp_ms,
            leaf_index,
        }
    }

    fn block(block_id: u64, timestamp_ms: u64, events: Vec<Event>) -> Block {
        Block {
            block_id,
            timestamp_ms,
            events,
        }
    }

    #[test]
    fn test_register_expire_and_proof_path() {
        let (mut reader, _) = Reader::open_temporary([0; 32], 0).unwrap();
        let empty = reader.root();
        let changes = reader
            .apply_block(&block(1, 1_000, vec![registered(1, 0, 5_000)]))
            .unwrap();
        assert_eq!(changes.len(), 1);
        assert_ne!(reader.root(), empty);
        let path = reader.proof_path(0).unwrap();
        assert_eq!(path.path_elements.len(), TREE_DEPTH);
        assert_eq!(path.identity_path_index.len(), TREE_DEPTH);
        assert!(reader.proof_path(1).is_none());
        // Expired at the block whose timestamp reaches the expiry: removed, root back to empty.
        let changes = reader.apply_block(&block(2, 5_000, vec![])).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(reader.root(), empty);
        assert!(reader.proof_path(0).is_none());
    }

    #[test]
    fn test_reopen_continues_from_the_store() {
        let dir = std::env::temp_dir().join(format!("zone-sequencer-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = {
            let (mut reader, extra) = Reader::open(&dir, [0; 32], 0).unwrap();
            assert!(extra.is_null());
            reader
                .apply_block(&block(
                    1,
                    1_000,
                    vec![registered(1, 0, 5_000), registered(2, 1, 9_000)],
                ))
                .unwrap();
            reader.apply_block(&block(2, 5_000, vec![])).unwrap();
            reader.persist(serde_json::json!({ "note": 7 })).unwrap();
            reader.close().unwrap();
            reader.root()
        };
        let (reader, extra) = Reader::open(&dir, [0; 32], 0).unwrap();
        assert_eq!(reader.root(), root);
        assert_eq!(reader.applied_block_id, 2);
        assert_eq!(reader.live_count(), 1);
        assert_eq!(reader.removed_leaves().len(), 1);
        assert!(reader.proof_path(1).is_some());
        assert_eq!(extra["note"], 7);
        assert!(Reader::open(&dir, [9; 32], 0).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_other_registries_are_ignored() {
        let (mut reader, _) = Reader::open_temporary([7; 32], 0).unwrap();
        let changes = reader
            .apply_block(&block(1, 1_000, vec![registered(1, 0, 5_000)]))
            .unwrap();
        assert!(changes.is_empty());
        assert_eq!(reader.live_count(), 0);
    }
}
