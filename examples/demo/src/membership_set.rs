//! The membership set and the derivation rules (RFC "Derivation rules"), shared by the Zone
//! sequencer and every Zone indexer. This is what `rln-membership-core` will become.

use std::collections::HashMap;

use rln::prelude::{CanonicalSerialize, Fr, Hasher, PoseidonHash};
use zerokit_utils::merkle_tree::{OptimalMerkleTree, ZerokitMerkleTree};

use crate::{
    batch::{Cause, Change},
    lez::{Block, Instruction, MembershipAccount, MockLez},
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

#[derive(Debug, Clone, Copy)]
struct Leaf {
    index: u64,
    expiry_timestamp_ms: u64,
}

#[derive(Clone)]
pub struct MembershipSet {
    tree: OptimalMerkleTree<PoseidonHash>,
    live: HashMap<[u8; 32], Leaf>,
    removed: HashMap<[u8; 32], Cause>,
    /// Last LEZ `block_id` applied.
    pub lez_to: u64,
}

impl MembershipSet {
    pub fn new() -> Self {
        Self {
            tree: OptimalMerkleTree::default(TREE_DEPTH).expect("depth 20 tree"),
            live: HashMap::new(),
            removed: HashMap::new(),
            lez_to: 0,
        }
    }

    pub fn root(&self) -> Fr {
        self.tree.root()
    }

    /// Apply the derivation rules to one finalized LEZ block and return the changes, in order.
    pub fn apply_block(&mut self, block: &Block) -> Vec<Change> {
        let mut changes = vec![];
        for instruction in &block.transactions {
            match instruction {
                Instruction::Register {
                    id_commitment,
                    rate_limit,
                    expiry_timestamp_ms,
                    leaf_index,
                    ..
                } => {
                    // The registration program assigned the index and enforced the capacity.
                    let index = *leaf_index;
                    // Leaf = rate commitment = poseidon(identity_commitment, rate_limit).
                    let leaf =
                        Hasher::<PoseidonHash>::hash_pair(*id_commitment, Fr::from(*rate_limit));
                    self.tree
                        .set(index as usize, leaf)
                        .expect("Register caps leaf_index below 2^depth");
                    let key = fr_to_bytes_le(id_commitment);
                    self.live.insert(
                        key,
                        Leaf {
                            index,
                            expiry_timestamp_ms: *expiry_timestamp_ms,
                        },
                    );
                    changes.push(Change::insert(
                        key.to_vec(),
                        *rate_limit,
                        *expiry_timestamp_ms,
                    ));
                }
                Instruction::Extend {
                    id_commitment,
                    expiry_timestamp_ms,
                    ..
                } => {
                    if let Some(leaf) = self.live.get_mut(&fr_to_bytes_le(id_commitment)) {
                        leaf.expiry_timestamp_ms = *expiry_timestamp_ms;
                        changes.push(Change::update(leaf.index, *expiry_timestamp_ms));
                    }
                }
                Instruction::Erase { id_commitment, .. } => {
                    changes.extend(self.remove(fr_to_bytes_le(id_commitment), Cause::Erased));
                }
                Instruction::Slash {
                    identity_secret, ..
                } => {
                    let key =
                        fr_to_bytes_le(&Hasher::<PoseidonHash>::hash_single(*identity_secret));
                    match self.remove(key, Cause::Slashed) {
                        Some(change) => changes.push(change),
                        // Slashed during the withdrawal delay: the leaf already expired, so no
                        // change, but the membership is reported as slashed.
                        None => {
                            self.removed.insert(key, Cause::Slashed);
                        }
                    }
                }
            }
        }
        // Expiry after the block's transactions, in leaf order, at most once per leaf.
        let mut expired: Vec<_> = self
            .live
            .iter()
            .filter(|(_, leaf)| leaf.expiry_timestamp_ms <= block.timestamp)
            .map(|(key, leaf)| (leaf.index, *key))
            .collect();
        expired.sort();
        for (_, key) in expired {
            changes.extend(self.remove(key, Cause::Expired));
        }
        self.lez_to = block.block_id;
        changes
    }

    fn remove(&mut self, key: [u8; 32], cause: Cause) -> Option<Change> {
        let leaf = self.live.remove(&key)?;
        self.tree
            .delete(leaf.index as usize)
            .expect("index in range");
        self.removed.insert(key, cause);
        Some(Change::remove(leaf.index, cause))
    }

    pub fn leaf_index(&self, id_commitment: &Fr) -> Option<u64> {
        self.live
            .get(&fr_to_bytes_le(id_commitment))
            .map(|l| l.index)
    }

    /// RLN-API `MembershipStatus` as a Module would report it from this view and the
    /// membership account on LEZ (RFC "Registry View for Modules / Membership status").
    pub fn status(&self, id_commitment: &Fr, lez: &MockLez) -> &'static str {
        let key = fr_to_bytes_le(id_commitment);
        let now = lez.now();
        let account = lez.membership_account(id_commitment);
        if let Some(leaf) = self.live.get(&key) {
            return match account {
                // S11: emptied on LEZ while the removal batch is still pending.
                MembershipAccount::Emptied => "MEMBERSHIP_SLASHED",
                // S7.
                _ if now >= leaf.expiry_timestamp_ms => "MEMBERSHIP_EXPIRED",
                // S6.
                _ if now + lez.config.grace_period_duration_ms >= leaf.expiry_timestamp_ms => {
                    "MEMBERSHIP_GRACE_PERIOD"
                }
                // S5.
                _ => "MEMBERSHIP_ACTIVE",
            };
        }
        match (self.removed.get(&key), account) {
            // S11.
            (Some(Cause::Slashed), _) => "MEMBERSHIP_SLASHED",
            // S8 and S9.
            (Some(_), MembershipAccount::Open(membership)) => {
                if now >= membership.expiry_timestamp_ms + lez.config.withdrawal_delay_ms {
                    "MEMBERSHIP_ERASED_AWAITS_WITHDRAWAL"
                } else {
                    "MEMBERSHIP_EXPIRED"
                }
            }
            // S10.
            (Some(_), _) => "MEMBERSHIP_ERASED",
            // S3.
            (None, MembershipAccount::Open(_)) => "MEMBERSHIP_PENDING",
            // S1.
            (None, _) => "MEMBERSHIP_UNKNOWN",
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::lez::test::{new_keys, new_lez, register};

    /// Apply every LEZ block to a fresh membership set and return the changes per block.
    fn replay(lez: &MockLez) -> (MembershipSet, Vec<Vec<Change>>) {
        let mut set = MembershipSet::new();
        let changes = lez
            .blocks(1, lez.last_finalized_block_id())
            .iter()
            .map(|block| set.apply_block(block))
            .collect();
        (set, changes)
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
        assert_eq!(
            changes[1],
            vec![
                Change::remove(0, Cause::Expired),
                Change::remove(1, Cause::Expired)
            ]
        );
    }

    #[test]
    fn test_extend_produces_update() {
        let mut lez = new_lez();
        let keys = new_keys();
        lez.submit(register("alice", &keys, 8_000)).unwrap();
        lez.produce_block(4_000);
        lez.submit(Instruction::Extend {
            holder: "alice".into(),
            id_commitment: keys.id_commitment(),
            expiry_timestamp_ms: 12_000,
        })
        .unwrap();
        lez.produce_block(2_000);
        let (set, changes) = replay(&lez);
        assert_eq!(changes[1], vec![Change::update(0, 12_000)]);
        assert_eq!(set.leaf_index(&keys.id_commitment()), Some(0));
    }

    #[test]
    fn test_slash_after_expiry_reports_slashed() {
        let mut lez = new_lez();
        let keys = new_keys();
        lez.submit(register("alice", &keys, 4_000)).unwrap();
        lez.produce_block(2_000);
        lez.produce_block(2_000);
        // Expired at 4000; the account stays open until 4000 + withdrawal delay.
        lez.submit(Instruction::Slash {
            slasher: "carol".into(),
            identity_secret: *keys.identity_secret(),
        })
        .unwrap();
        lez.produce_block(2_000);
        let (set, changes) = replay(&lez);
        assert_eq!(changes[1], vec![Change::remove(0, Cause::Expired)]);
        assert!(changes[2].is_empty());
        assert_eq!(
            set.status(&keys.id_commitment(), &lez),
            "MEMBERSHIP_SLASHED"
        );
    }

    #[test]
    fn test_status_follows_the_membership_status_table() {
        let mut lez = new_lez();
        let keys = new_keys();
        let id_commitment = keys.id_commitment();
        let status = |lez: &MockLez| replay(lez).0.status(&id_commitment, lez);
        // S1.
        assert_eq!(status(&lez), "MEMBERSHIP_UNKNOWN");
        lez.submit(register("alice", &keys, 10_000)).unwrap();
        lez.produce_block(2_000);
        // S5 at 2000: grace period is [6000, 10000).
        assert_eq!(status(&lez), "MEMBERSHIP_ACTIVE");
        lez.produce_block(4_000);
        // S6 at 6000.
        assert_eq!(status(&lez), "MEMBERSHIP_GRACE_PERIOD");
        lez.produce_block(4_000);
        // S8 at 10000: removed as expired, withdrawal delay runs until 16000.
        assert_eq!(status(&lez), "MEMBERSHIP_EXPIRED");
        lez.produce_block(6_000);
        // S9 at 16000.
        assert_eq!(status(&lez), "MEMBERSHIP_ERASED_AWAITS_WITHDRAWAL");
        lez.submit(Instruction::Erase {
            holder: "alice".into(),
            id_commitment,
        })
        .unwrap();
        lez.produce_block(2_000);
        // S10.
        assert_eq!(status(&lez), "MEMBERSHIP_ERASED");
    }

    #[test]
    fn test_status_expired_before_the_removal_batch() {
        let mut lez = new_lez();
        let keys = new_keys();
        lez.submit(register("alice", &keys, 4_000)).unwrap();
        lez.produce_block(2_000);
        // The Zone view stops at block 1, where the leaf is live.
        let (set, _) = replay(&lez);
        lez.produce_block(2_000);
        // S7: LEZ is at 4000, the removal is not in the view yet.
        assert_eq!(
            set.status(&keys.id_commitment(), &lez),
            "MEMBERSHIP_EXPIRED"
        );
    }
}
