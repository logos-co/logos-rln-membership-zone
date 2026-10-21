//! The Module (RFC "Module Requirements"): holds no tree. It owns an identity credential,
//! writes to LEZ, and reads everything else from its registry access, a Zone indexer: the
//! valid roots, its Merkle proof path, its membership state and the LEZ clock. As proof
//! generator it proves against the newest valid root; as validator it accepts any root in the
//! valid-root window.

use rln::prelude::IdentityKeys;

use crate::{
    indexer::{WINDOW_LENGTH_BLOCKS, ZoneIndexer},
    lez::{MembershipAccount, MockLez},
    reader::{Removed, fr_to_bytes_le},
};

pub struct Module {
    pub name: &'static str,
    pub keys: IdentityKeys,
    /// Which Zone indexer this Module reads (an index into the demo's indexers): a shared
    /// service, or the node's own.
    pub access: usize,
}

impl Module {
    pub fn new(name: &'static str, keys: IdentityKeys, access: usize) -> Self {
        Self { name, keys, access }
    }

    pub fn id_commitment(&self) -> [u8; 32] {
        fr_to_bytes_le(&self.keys.id_commitment())
    }

    /// Ready while the registry access serves a valid root that trails the latest LEZ block
    /// by no more than the window length; otherwise `RLN_ERR_NOT_READY`.
    pub fn is_ready(&self, access: &ZoneIndexer, lez: &MockLez) -> bool {
        access.newest_valid_root().is_some_and(|newest| {
            newest.to_lez_block_id + WINDOW_LENGTH_BLOCKS >= lez.last_block_id()
        })
    }

    /// As proof generator: the root to prove against and the Merkle proof path, if the Module
    /// is ready and its leaf is live at the newest valid root.
    pub fn generate(&self, access: &ZoneIndexer, lez: &MockLez) -> Option<([u8; 32], usize)> {
        if !self.is_ready(access, lez) {
            return None;
        }
        let (path, _) = access.proof_path(&self.id_commitment())?;
        Some((access.newest_valid_root()?.root_after, path.len()))
    }

    /// As validator: accept a proof against `root` only if the root is in the valid-root
    /// window the registry access serves.
    pub fn validate(&self, access: &ZoneIndexer, lez: &MockLez, root: &[u8; 32]) -> bool {
        self.is_ready(access, lez)
            && access
                .valid_roots()
                .iter()
                .any(|batch| batch.root_after == *root)
    }

    /// RLN-API `MembershipStatus` from the membership state and the valid roots the registry
    /// access serves (RFC "Module Requirements / Membership status"). A leaf is usable only
    /// from the first valid root that holds it, and LEZ moves ahead of that.
    pub fn status(&self, access: &ZoneIndexer, lez: &MockLez) -> &'static str {
        let id_commitment = self.id_commitment();
        let now = access.clock(lez);
        let account = access.membership_state(lez, &id_commitment);
        let tree = access.served_tree();
        if tree.live_leaf(&id_commitment).is_some() {
            return match account {
                // Closed on LEZ, removal not yet in a valid root.
                MembershipAccount::ClosedBySlash => "MEMBERSHIP_SLASHED",
                MembershipAccount::ClosedByErase => "MEMBERSHIP_ERASED",
                // Expired on the LEZ clock, removal not yet in a valid root.
                MembershipAccount::Open(m) if now >= m.expiry_timestamp_ms => "MEMBERSHIP_EXPIRED",
                // In the membership's grace period.
                MembershipAccount::Open(m)
                    if now + m.grace_period_duration_ms >= m.expiry_timestamp_ms =>
                {
                    "MEMBERSHIP_GRACE_PERIOD"
                }
                _ => "MEMBERSHIP_ACTIVE",
            };
        }
        match (tree.removed(&id_commitment), account) {
            (Some(Removed::Slashed), _) | (None, MembershipAccount::ClosedBySlash) => {
                "MEMBERSHIP_SLASHED"
            }
            // Expired; past the withdrawal delay on the latest LEZ timestamp served, not before.
            (Some(_), MembershipAccount::Open(m)) => {
                if now >= m.expiry_timestamp_ms + m.withdrawal_delay_ms {
                    "MEMBERSHIP_ERASED_AWAITS_WITHDRAWAL"
                } else {
                    "MEMBERSHIP_EXPIRED"
                }
            }
            (Some(_), _) | (None, MembershipAccount::ClosedByErase) => "MEMBERSHIP_ERASED",
            // Registered on LEZ, no valid root holds the leaf yet.
            (None, MembershipAccount::Open(_)) => "MEMBERSHIP_PENDING",
            (None, MembershipAccount::Default) => "MEMBERSHIP_UNKNOWN",
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{
        indexer::test::{synced, verify_through},
        lez::test::{erase, new_keys, new_lez, register, slash},
    };

    /// A registry access that has read every LEZ block and Verified a batch per block.
    fn access(lez: &MockLez) -> ZoneIndexer {
        let mut indexer = synced(lez);
        verify_through(&mut indexer, lez.last_block_id());
        indexer
    }

    #[test]
    fn test_status_follows_the_membership_status_cases() {
        let mut lez = new_lez();
        let module = Module::new("alice", new_keys(), 0);
        // Unknown.
        assert_eq!(module.status(&access(&lez), &lez), "MEMBERSHIP_UNKNOWN");
        lez.submit(register("alice", &module.keys, 10_000)).unwrap();
        // Registered on LEZ, no valid root yet: pending.
        assert_eq!(module.status(&access(&lez), &lez), "MEMBERSHIP_PENDING");
        lez.produce_block(2_000);
        // Active at 2000 once the batch with the Insert is Verified; grace is [6000, 10000).
        assert_eq!(module.status(&access(&lez), &lez), "MEMBERSHIP_ACTIVE");
        lez.produce_block(4_000);
        assert_eq!(
            module.status(&access(&lez), &lez),
            "MEMBERSHIP_GRACE_PERIOD"
        );
        lez.produce_block(4_000);
        // Expired at 10000; withdrawal delay runs until 16000.
        assert_eq!(module.status(&access(&lez), &lez), "MEMBERSHIP_EXPIRED");
        lez.produce_block(6_000);
        assert_eq!(
            module.status(&access(&lez), &lez),
            "MEMBERSHIP_ERASED_AWAITS_WITHDRAWAL"
        );
        lez.submit(erase("alice", &module.keys)).unwrap();
        lez.produce_block(2_000);
        assert_eq!(module.status(&access(&lez), &lez), "MEMBERSHIP_ERASED");
    }

    #[test]
    fn test_status_ahead_of_the_valid_roots() {
        let mut lez = new_lez();
        let module = Module::new("alice", new_keys(), 0);
        lez.submit(register("alice", &module.keys, 4_000)).unwrap();
        lez.produce_block(2_000);
        // The registry access stops at block 1: the leaf is live in its newest valid root.
        let stale = access(&lez);
        lez.produce_block(2_000);
        // Expired on the LEZ clock (4000), removal not yet in a valid root.
        assert_eq!(module.status(&stale, &lez), "MEMBERSHIP_EXPIRED");
        // Slashed on LEZ during the withdrawal delay: slashed at once.
        lez.submit(slash("carol", &module.keys)).unwrap();
        lez.produce_block(2_000);
        assert_eq!(module.status(&stale, &lez), "MEMBERSHIP_SLASHED");
        assert_eq!(module.status(&access(&lez), &lez), "MEMBERSHIP_SLASHED");
    }

    #[test]
    fn test_generate_and_validate_against_the_window() {
        let mut lez = new_lez();
        let alice = Module::new("alice", new_keys(), 0);
        let bob = Module::new("bob", new_keys(), 0);
        lez.submit(register("alice", &alice.keys, 60_000)).unwrap();
        lez.produce_block(2_000);
        let mut indexer = access(&lez);
        let (root, path_len) = alice.generate(&indexer, &lez).unwrap();
        assert_eq!(path_len, 20);
        assert!(bob.validate(&indexer, &lez, &root));
        // The indexer trails LEZ by more than the window: not ready, nothing validates.
        for _ in 0..WINDOW_LENGTH_BLOCKS + 1 {
            lez.produce_block(2_000);
        }
        assert!(!bob.is_ready(&indexer, &lez));
        assert!(!bob.validate(&indexer, &lez, &root));
        assert!(alice.generate(&indexer, &lez).is_none());
        // Caught up: the old root is still in the window (no Remove since), so it validates.
        for block in lez.blocks(indexer.reader.applied_block_id + 1, lez.last_block_id()) {
            indexer.apply_block(block);
        }
        verify_through(&mut indexer, lez.last_block_id());
        assert!(bob.validate(&indexer, &lez, &root));
        // A removal resets the window: the old root no longer validates.
        lez.submit(slash("carol", &alice.keys)).unwrap();
        let block = lez.produce_block(2_000).clone();
        indexer.apply_block(&block);
        verify_through(&mut indexer, lez.last_block_id());
        assert!(!bob.validate(&indexer, &lez, &root));
        assert!(alice.generate(&indexer, &lez).is_none());
    }
}
