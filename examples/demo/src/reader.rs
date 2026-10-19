//! A reader (RFC "Derivation Rules"): holds the tree and applies the rules to every LEZ block.
//! The Zone sequencer, every Zone indexer and every Module are readers, so they all hold the
//! same tree and the same roots. This is what `rln-membership-core` will become.

use std::collections::HashMap;

use rln::prelude::{CanonicalDeserialize, CanonicalSerialize, Fr, Hasher, PoseidonHash};
use zerokit_utils::merkle_tree::{OptimalMerkleTree, ZerokitMerkleTree};

use crate::{
    batch::Change,
    lez::{Action, Block, MockLez},
};

pub const TREE_DEPTH: usize = 20;

/// 32-byte little-endian representation of a field element (RLN-API Appendix B form).
pub fn fr_to_bytes_le(fr: &Fr) -> [u8; 32] {
    let mut out = [0u8; 32];
    fr.serialize_compressed(&mut out[..])
        .expect("an Fr is 32 bytes");
    out
}

pub fn short(fr: &Fr) -> String {
    hex::encode(&fr_to_bytes_le(fr)[..4])
}

/// A live leaf as the reader tracks it; the expiry comes from LEZ, not from the batches.
#[derive(Debug, Clone, Copy)]
pub struct Leaf {
    pub leaf_index: u64,
    pub expiry_timestamp_ms: u64,
}

/// Why a leaf left the tree. Read from LEZ and used for status only; batches do not carry it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removed {
    Expired,
    Slashed,
}

/// What one LEZ block did to the tree: the changes it produced and the root after it.
#[derive(Debug, Clone)]
struct Applied {
    block_id: u64,
    root: [u8; 32],
    changes: Vec<Change>,
}

#[derive(Clone)]
pub struct Reader {
    tree: OptimalMerkleTree<PoseidonHash>,
    live: HashMap<[u8; 32], Leaf>,
    removed: HashMap<[u8; 32], Removed>,
    /// One entry per LEZ block applied, in order: the root history and the changes.
    history: Vec<Applied>,
    /// Last LEZ `block_id` applied.
    pub applied_block_id: u64,
    /// `timestamp` of the last LEZ block applied: the clock this reader's tree is at.
    pub applied_timestamp: u64,
}

impl Reader {
    pub fn new() -> Self {
        Self {
            tree: OptimalMerkleTree::default(TREE_DEPTH).expect("depth 20 tree"),
            live: HashMap::new(),
            removed: HashMap::new(),
            history: Vec::new(),
            applied_block_id: 0,
            applied_timestamp: 0,
        }
    }

    pub fn root(&self) -> Fr {
        self.tree.root()
    }

    /// The root this reader held right after applying `block_id`, if it has applied it.
    pub fn root_at(&self, block_id: u64) -> Option<[u8; 32]> {
        self.history
            .iter()
            .find(|applied| applied.block_id == block_id)
            .map(|applied| applied.root)
    }

    /// The changes the LEZ blocks `from..=to` produced, in order, if this reader has applied
    /// them all.
    pub fn changes_in(&self, from: u64, to: u64) -> Option<Vec<Change>> {
        if from == 0 || from > to || to > self.applied_block_id {
            return None;
        }
        Some(
            self.history
                .iter()
                .filter(|applied| (from..=to).contains(&applied.block_id))
                .flat_map(|applied| applied.changes.iter().cloned())
                .collect(),
        )
    }

    /// Apply the derivation rules to one LEZ block and return the changes, in order.
    pub fn apply_block(&mut self, block: &Block) -> Vec<Change> {
        let mut changes = vec![];
        for action in &block.actions {
            match action {
                Action::Register {
                    id_commitment,
                    rate_limit,
                    expiry_timestamp_ms,
                    leaf_index,
                    ..
                } => {
                    // The registration program assigned `leaf_index` and enforced the capacity.
                    // Leaf = rate commitment = poseidon(identity_commitment, rate_limit).
                    let leaf =
                        Hasher::<PoseidonHash>::hash_pair(*id_commitment, Fr::from(*rate_limit));
                    self.tree
                        .set(*leaf_index as usize, leaf)
                        .expect("Register caps leaf_index below 2^depth");
                    let key = fr_to_bytes_le(id_commitment);
                    self.live.insert(
                        key,
                        Leaf {
                            leaf_index: *leaf_index,
                            expiry_timestamp_ms: *expiry_timestamp_ms,
                        },
                    );
                    changes.push(Change::insert(*leaf_index, fr_to_bytes_le(&leaf).to_vec()));
                }
                Action::Extend {
                    id_commitment,
                    expiry_timestamp_ms,
                    ..
                } => {
                    // No change: the tree is untouched, only the expiry the reader tracks moves.
                    if let Some(leaf) = self.live.get_mut(&fr_to_bytes_le(id_commitment)) {
                        leaf.expiry_timestamp_ms = *expiry_timestamp_ms;
                    }
                }
                // Normally no change: `Erase` needs `now >= expiry + withdrawal delay`, and
                // the previous block, whose timestamp is `now`, already expired the leaf.
                // A still-live leaf means the program allowed an early `Erase`; remove it.
                Action::Erase { id_commitment, .. } => {
                    changes.extend(self.remove(fr_to_bytes_le(id_commitment), Removed::Expired));
                }
                Action::Slash {
                    identity_secret, ..
                } => {
                    let key =
                        fr_to_bytes_le(&Hasher::<PoseidonHash>::hash_single(*identity_secret));
                    match self.remove(key, Removed::Slashed) {
                        Some(change) => changes.push(change),
                        // Slashed during the withdrawal delay: the leaf already expired, so no
                        // change, but the membership is reported as slashed.
                        None => {
                            self.removed.insert(key, Removed::Slashed);
                        }
                    }
                }
            }
        }
        // Expiry after the block's actions, in leaf order, at most once per leaf.
        let mut expired: Vec<_> = self
            .live
            .iter()
            .filter(|(_, leaf)| leaf.expiry_timestamp_ms <= block.timestamp)
            .map(|(key, leaf)| (leaf.leaf_index, *key))
            .collect();
        expired.sort();
        for (_, key) in expired {
            changes.extend(self.remove(key, Removed::Expired));
        }
        self.applied_block_id = block.block_id;
        self.applied_timestamp = block.timestamp;
        self.history.push(Applied {
            block_id: block.block_id,
            root: fr_to_bytes_le(&self.tree.root()),
            changes: changes.clone(),
        });
        changes
    }

    fn remove(&mut self, key: [u8; 32], cause: Removed) -> Option<Change> {
        let leaf = self.live.remove(&key)?;
        self.tree
            .delete(leaf.leaf_index as usize)
            .expect("index in range");
        self.removed.insert(key, cause);
        Some(Change::remove(leaf.leaf_index))
    }

    /// Check a snapshot's leaves against LEZ (RFC "Module Requirements / Sync"). The root
    /// commits the leaves but not whose they are or when they expire, so every non-empty leaf
    /// must come with an identity commitment whose open membership account reproduces it
    /// (`leaf_index`, `poseidon(id_commitment, rate_limit)`), and its expiry is taken from that
    /// account.
    pub fn check_against_lez(&mut self, lez: &MockLez) -> Result<(), String> {
        for index in 0..self.tree.leaves_set() {
            let leaf = self.tree.get(index).map_err(|e| e.to_string())?;
            if leaf == Fr::default() {
                continue;
            }
            let (key, _) = self
                .live
                .iter()
                .find(|(_, tracked)| tracked.leaf_index == index as u64)
                .map(|(key, tracked)| (*key, *tracked))
                .ok_or(format!(
                    "leaf {index} has no identity commitment in the snapshot"
                ))?;
            let id_commitment = Fr::deserialize_compressed(&key[..]).map_err(|e| e.to_string())?;
            let membership = lez.membership(&id_commitment).ok_or(format!(
                "leaf {index} has no open membership account on LEZ"
            ))?;
            if membership.leaf_index != index as u64
                || Hasher::<PoseidonHash>::hash_pair(id_commitment, Fr::from(membership.rate_limit))
                    != leaf
            {
                return Err(format!(
                    "leaf {index} does not match its membership account"
                ));
            }
            if let Some(tracked) = self.live.get_mut(&key) {
                tracked.expiry_timestamp_ms = membership.expiry_timestamp_ms;
            }
        }
        Ok(())
    }

    pub fn leaf_index(&self, id_commitment: &Fr) -> Option<u64> {
        self.live_leaf(id_commitment).map(|l| l.leaf_index)
    }

    pub fn live_leaf(&self, id_commitment: &Fr) -> Option<Leaf> {
        self.live.get(&fr_to_bytes_le(id_commitment)).copied()
    }

    pub fn removed(&self, id_commitment: &Fr) -> Option<Removed> {
        self.removed.get(&fr_to_bytes_le(id_commitment)).copied()
    }
}

#[cfg(test)]
pub(crate) mod test {
    use super::*;
    use crate::lez::{
        MockLez,
        test::{new_keys, new_lez, register},
    };

    /// Apply every LEZ block to a fresh reader and return the changes per block.
    pub(crate) fn replay(lez: &MockLez) -> (Reader, Vec<Vec<Change>>) {
        let mut reader = Reader::new();
        let changes = lez
            .blocks(1, lez.last_block_id())
            .iter()
            .map(|block| reader.apply_block(block))
            .collect();
        (reader, changes)
    }

    #[test]
    fn test_expiry_removes_in_leaf_order() {
        let mut lez = new_lez();
        let (a, b) = (new_keys(), new_keys());
        lez.submit(register("alice", &a, 4_000)).unwrap();
        lez.submit(register("bob", &b, 4_000)).unwrap();
        lez.produce_block(2_000);
        lez.produce_block(2_000);
        let (_, changes) = replay(&lez);
        assert_eq!(changes[1], vec![Change::remove(0), Change::remove(1)]);
    }

    #[test]
    fn test_extend_produces_no_change() {
        let mut lez = new_lez();
        let keys = new_keys();
        lez.submit(register("alice", &keys, 8_000)).unwrap();
        lez.produce_block(4_000);
        lez.submit(Action::Extend {
            holder: "alice".into(),
            id_commitment: keys.id_commitment(),
            expiry_timestamp_ms: 12_000,
        })
        .unwrap();
        lez.produce_block(2_000);
        // At 10000 the old expiry (8000) has passed, the new one (12000) has not.
        lez.produce_block(4_000);
        let (reader, changes) = replay(&lez);
        assert!(changes[1].is_empty());
        assert!(changes[2].is_empty());
        assert_eq!(reader.leaf_index(&keys.id_commitment()), Some(0));
    }

    #[test]
    fn test_slash_after_expiry_produces_no_change() {
        let mut lez = new_lez();
        let keys = new_keys();
        lez.submit(register("alice", &keys, 4_000)).unwrap();
        lez.produce_block(2_000);
        lez.produce_block(2_000);
        // Expired at 4000; the account stays open until 4000 + withdrawal delay.
        lez.submit(Action::Slash {
            slasher: "carol".into(),
            identity_secret: *keys.identity_secret(),
        })
        .unwrap();
        lez.produce_block(2_000);
        let (reader, changes) = replay(&lez);
        assert_eq!(changes[1], vec![Change::remove(0)]);
        assert!(changes[2].is_empty());
        assert_eq!(
            reader.removed(&keys.id_commitment()),
            Some(Removed::Slashed)
        );
    }

    #[test]
    fn test_roots_are_tagged_by_block() {
        let mut lez = new_lez();
        lez.submit(register("alice", &new_keys(), 8_000)).unwrap();
        lez.produce_block(2_000);
        lez.produce_block(2_000);
        let (reader, _) = replay(&lez);
        assert_eq!(reader.root_at(1), reader.root_at(2));
        assert_eq!(reader.root_at(2), Some(fr_to_bytes_le(&reader.root())));
        assert_eq!(reader.root_at(3), None);
        assert_eq!(
            reader.changes_in(1, 2).map(|c| c.len()),
            Some(1),
            "one Insert over both blocks"
        );
        assert_eq!(reader.changes_in(1, 3), None);
    }
}
