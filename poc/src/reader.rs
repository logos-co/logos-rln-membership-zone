//! A reader (RFC "Derivation Rules"): holds the tree and applies the rules to the registration
//! program's events of every LEZ block. The Zone sequencer is a reader; so is every Zone
//! indexer, which is why they hold the same roots.

use std::collections::HashMap;

use rln::prelude::{CanonicalDeserialize, CanonicalSerialize, Fr, Hasher, PoseidonHash};
use zerokit_utils::merkle_tree::{OptimalMerkleTree, ZerokitMerkleProof, ZerokitMerkleTree};

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
}

/// Why a leaf left the tree. Read from the events and used for status only; batches do not
/// carry it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Removed {
    Expired,
    Slashed,
}

/// A Merkle proof path at one root, in RLN-API form.
pub struct ProofPath {
    pub leaf_index: u64,
    pub rate_commitment: [u8; 32],
    pub path_elements: Vec<[u8; 32]>,
    pub identity_path_index: Vec<u8>,
}

#[derive(Clone)]
pub struct Reader {
    /// The registry this reader follows; events of other registries are ignored.
    tree_id: [u8; 32],
    tree: OptimalMerkleTree<PoseidonHash>,
    /// Live leaves by identity commitment.
    live: HashMap<[u8; 32], Leaf>,
    removed: HashMap<[u8; 32], Removed>,
    /// Last LEZ `block_id` applied.
    pub applied_block_id: u64,
    /// `timestamp_ms` of the last LEZ block applied: the clock this reader's tree is at.
    pub applied_timestamp_ms: u64,
}

impl Reader {
    pub fn new(tree_id: [u8; 32], applied_block_id: u64) -> Self {
        Self {
            tree_id,
            tree: OptimalMerkleTree::default(TREE_DEPTH).expect("depth 20 tree"),
            live: HashMap::new(),
            removed: HashMap::new(),
            applied_block_id,
            applied_timestamp_ms: 0,
        }
    }

    pub fn root(&self) -> [u8; 32] {
        fr_to_bytes_le(&self.tree.root())
    }

    pub fn live_count(&self) -> usize {
        self.live.len()
    }

    /// Apply the derivation rules to the events of one LEZ block and return the changes, in
    /// order.
    pub fn apply_block(&mut self, block: &Block) -> Vec<Change> {
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
                    // Leaf = rate commitment = poseidon(identity_commitment, rate_limit).
                    let leaf = rate_commitment(id_commitment, *rate_limit);
                    self.tree
                        .set(*leaf_index as usize, leaf)
                        .expect("Register caps leaf_index below 2^depth");
                    self.live.insert(
                        *id_commitment,
                        Leaf {
                            leaf_index: *leaf_index,
                            expiry_timestamp_ms: *expiry_timestamp_ms,
                        },
                    );
                    changes.push(Change::insert(*leaf_index, fr_to_bytes_le(&leaf).to_vec()));
                }
                Event::Extended {
                    tree_id,
                    id_commitment,
                    expiry_timestamp_ms,
                } if *tree_id == self.tree_id => {
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
                    changes.extend(self.remove(*id_commitment, Removed::Expired));
                }
                Event::Slashed {
                    tree_id,
                    id_commitment,
                } if *tree_id == self.tree_id => {
                    match self.remove(*id_commitment, Removed::Slashed) {
                        Some(change) => changes.push(change),
                        // Slashed during the withdrawal delay: the leaf already expired, so no
                        // change, but the membership is reported as slashed.
                        None => {
                            self.removed.insert(*id_commitment, Removed::Slashed);
                        }
                    }
                }
                _ => {}
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
            changes.extend(self.remove(key, Removed::Expired));
        }
        self.applied_block_id = block.block_id;
        self.applied_timestamp_ms = block.timestamp_ms;
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

/// `poseidon(id_commitment, rate_limit)`, the leaf of a membership.
fn rate_commitment(id_commitment: &[u8; 32], rate_limit: u64) -> Fr {
    let id_commitment = fr_from_bytes_le(id_commitment).expect("the program emits field elements");
    Hasher::<PoseidonHash>::hash_pair(id_commitment, Fr::from(rate_limit))
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
        let mut reader = Reader::new([0; 32], 0);
        let empty = reader.root();
        let changes = reader.apply_block(&block(1, 1_000, vec![registered(1, 0, 5_000)]));
        assert_eq!(changes.len(), 1);
        assert_ne!(reader.root(), empty);
        let path = reader.proof_path(0).unwrap();
        assert_eq!(path.path_elements.len(), TREE_DEPTH);
        assert_eq!(path.identity_path_index.len(), TREE_DEPTH);
        assert!(reader.proof_path(1).is_none());
        // Expired at the block whose timestamp reaches the expiry: removed, root back to empty.
        let changes = reader.apply_block(&block(2, 5_000, vec![]));
        assert_eq!(changes.len(), 1);
        assert_eq!(reader.root(), empty);
        assert!(reader.proof_path(0).is_none());
    }

    #[test]
    fn test_other_registries_are_ignored() {
        let mut reader = Reader::new([7; 32], 0);
        let changes = reader.apply_block(&block(1, 1_000, vec![registered(1, 0, 5_000)]));
        assert!(changes.is_empty());
        assert_eq!(reader.live_count(), 0);
    }
}
