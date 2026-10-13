//! The Module (RFC "Module Requirements"): a reader that owns an identity credential, reports
//! `MembershipStatus` from its membership account on LEZ and its own tree, and compares its
//! roots with the batches on Bedrock. The demo Modules start at the registration program's
//! first LEZ block, so they need no snapshot to sync.

use rln::prelude::{Fr, IdentityKeys};

use crate::{
    batch::Batch,
    lez::{Block, MembershipAccount, MockLez},
    reader::{Reader, Removed},
};

pub struct Module {
    pub name: &'static str,
    pub keys: IdentityKeys,
    pub reader: Reader,
}

impl Module {
    pub fn new(name: &'static str, keys: IdentityKeys) -> Self {
        Self {
            name,
            keys,
            reader: Reader::new(),
        }
    }

    pub fn id_commitment(&self) -> Fr {
        self.keys.id_commitment()
    }

    /// Apply a LEZ block to the Module's own tree.
    pub fn apply_block(&mut self, block: &Block) {
        self.reader.apply_block(block);
    }

    /// Compare: the Module's root at `to_lez_block_id` against the batch's `root_after`.
    /// `Ok(None)` when the Module has not applied that block yet.
    pub fn compare(&self, batch: &Batch) -> Result<Option<()>, String> {
        match self.reader.root_at(batch.to_lez_block_id) {
            None => Ok(None),
            Some(root) if root.as_slice() == batch.root_after => Ok(Some(())),
            Some(root) => Err(format!(
                "root at block {} is {}.., the batch says {}..",
                batch.to_lez_block_id,
                hex::encode(&root[..4]),
                hex::encode(&batch.root_after[..4])
            )),
        }
    }

    /// RLN-API `MembershipStatus` from the membership account on LEZ and the leaf in the
    /// Module's tree (RFC "Module Requirements / Membership status"). The LEZ side is read at
    /// its head, which may be ahead of the block the Module has applied.
    pub fn status(&self, lez: &MockLez) -> &'static str {
        let id_commitment = self.id_commitment();
        let now = lez.now();
        let account = lez.membership_account(&id_commitment);
        if let Some(leaf) = self.reader.live_leaf(&id_commitment) {
            return match account {
                // Closed on LEZ while the Module has not applied that block yet.
                MembershipAccount::ClosedBySlash => "MEMBERSHIP_SLASHED",
                MembershipAccount::ClosedByErase => "MEMBERSHIP_ERASED",
                // Expired on the LEZ clock, block not yet applied.
                _ if now >= leaf.expiry_timestamp_ms => "MEMBERSHIP_EXPIRED",
                // In the grace period.
                _ if now + lez.config.grace_period_duration_ms >= leaf.expiry_timestamp_ms => {
                    "MEMBERSHIP_GRACE_PERIOD"
                }
                // Active.
                _ => "MEMBERSHIP_ACTIVE",
            };
        }
        match (self.reader.removed(&id_commitment), account) {
            // Slashed.
            (Some(Removed::Slashed), _) => "MEMBERSHIP_SLASHED",
            // Expired, before or after the withdrawal delay.
            (Some(_), MembershipAccount::Open(membership)) => {
                if now >= membership.expiry_timestamp_ms + lez.config.withdrawal_delay_ms {
                    "MEMBERSHIP_ERASED_AWAITS_WITHDRAWAL"
                } else {
                    "MEMBERSHIP_EXPIRED"
                }
            }
            // Erased.
            (Some(_), _) => "MEMBERSHIP_ERASED",
            // Registered on LEZ, block not yet applied.
            (None, MembershipAccount::Open(_)) => "MEMBERSHIP_PENDING",
            // Unknown.
            (None, _) => "MEMBERSHIP_UNKNOWN",
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{
        lez::{
            Action,
            test::{new_keys, new_lez, register},
        },
        reader::Reader,
    };

    /// Replay every LEZ block into a fresh tree of `module`.
    fn resync(module: &mut Module, lez: &MockLez) {
        module.reader = Reader::new();
        for block in lez.blocks(1, lez.last_block_id()) {
            module.apply_block(block);
        }
    }

    #[test]
    fn test_status_follows_the_membership_status_cases() {
        let mut lez = new_lez();
        let mut module = Module::new("alice", new_keys());
        let status = |module: &mut Module, lez: &MockLez| {
            resync(module, lez);
            module.status(lez)
        };
        // Unknown.
        assert_eq!(status(&mut module, &lez), "MEMBERSHIP_UNKNOWN");
        lez.submit(register("alice", &module.keys, 10_000)).unwrap();
        lez.produce_block(2_000);
        // Active at 2000: grace period is [6000, 10000).
        assert_eq!(status(&mut module, &lez), "MEMBERSHIP_ACTIVE");
        lez.produce_block(4_000);
        // Grace period at 6000.
        assert_eq!(status(&mut module, &lez), "MEMBERSHIP_GRACE_PERIOD");
        lez.produce_block(4_000);
        // Expired at 10000: removed as expired, withdrawal delay runs until 16000.
        assert_eq!(status(&mut module, &lez), "MEMBERSHIP_EXPIRED");
        lez.produce_block(6_000);
        // Awaits withdrawal at 16000.
        assert_eq!(
            status(&mut module, &lez),
            "MEMBERSHIP_ERASED_AWAITS_WITHDRAWAL"
        );
        lez.submit(Action::Erase {
            holder: "alice".into(),
            id_commitment: module.id_commitment(),
        })
        .unwrap();
        lez.produce_block(2_000);
        // Erased.
        assert_eq!(status(&mut module, &lez), "MEMBERSHIP_ERASED");
    }

    #[test]
    fn test_status_slashed_after_expiry() {
        let mut lez = new_lez();
        let mut module = Module::new("alice", new_keys());
        lez.submit(register("alice", &module.keys, 4_000)).unwrap();
        lez.produce_block(2_000);
        lez.produce_block(2_000);
        lez.submit(Action::Slash {
            slasher: "carol".into(),
            identity_secret: *module.keys.identity_secret(),
        })
        .unwrap();
        lez.produce_block(2_000);
        resync(&mut module, &lez);
        assert_eq!(module.status(&lez), "MEMBERSHIP_SLASHED");
    }

    #[test]
    fn test_status_before_the_block_is_applied() {
        let mut lez = new_lez();
        let mut module = Module::new("alice", new_keys());
        lez.submit(register("alice", &module.keys, 4_000)).unwrap();
        lez.produce_block(2_000);
        // The Module stops at block 1, where the leaf is live.
        resync(&mut module, &lez);
        lez.produce_block(2_000);
        // Expired on the LEZ clock (4000), block not yet applied.
        assert_eq!(module.status(&lez), "MEMBERSHIP_EXPIRED");
        // Withdrawal delay until 10000, then Erase: closed by Erase, block not yet applied.
        for _ in 0..3 {
            lez.produce_block(2_000);
        }
        lez.submit(Action::Erase {
            holder: "alice".into(),
            id_commitment: module.id_commitment(),
        })
        .unwrap();
        lez.produce_block(2_000);
        assert_eq!(module.status(&lez), "MEMBERSHIP_ERASED");
    }

    #[test]
    fn test_compare_matches_an_honest_batch() {
        let mut lez = new_lez();
        let mut module = Module::new("alice", new_keys());
        lez.submit(register("alice", &module.keys, 8_000)).unwrap();
        lez.produce_block(2_000);
        resync(&mut module, &lez);
        let honest = Batch {
            from_lez_block_id: 1,
            to_lez_block_id: 1,
            changes: vec![],
            root_after: module.reader.root_at(1).unwrap().to_vec(),
        };
        assert_eq!(module.compare(&honest), Ok(Some(())));
        let wrong_root = Batch {
            root_after: vec![0; 32],
            ..honest.clone()
        };
        assert!(module.compare(&wrong_root).is_err());
        let future = Batch {
            to_lez_block_id: 2,
            ..honest
        };
        assert_eq!(module.compare(&future), Ok(None));
    }
}
