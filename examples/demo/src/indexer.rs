//! The Zone indexer (RFC "Zone Indexer"): a reader that follows LEZ, checks every batch on the
//! Zone channel against what it derived (range, changes, root), keeps the verdicts, and serves
//! the registry to Modules as their registry access (RFC "Serving"): the valid roots, a leaf's
//! Merkle proof path at the newest valid root, and the membership state and clock it reads
//! from LEZ. A Rejected batch changes nothing and its root is never a valid root.

use prost::Message;
use rln::prelude::Fr;

use crate::{
    batch::{Batch, Kind},
    lez::{Block, FIRST_LEZ_BLOCK_ID, MembershipAccount, MockLez},
    reader::Reader,
};

/// Valid-root window length in LEZ blocks (RFC Q3): how far a root may trail the newest one
/// and still be accepted, and how far a Module may trail LEZ and still be ready.
pub const WINDOW_LENGTH_BLOCKS: u64 = 3;

/// A Verified batch as the root history keeps it.
#[derive(Debug, Clone)]
pub struct VerifiedBatch {
    pub to_lez_block_id: u64,
    pub root_after: [u8; 32],
    /// Whether the batch holds a `Remove`: the valid-root window resets at it.
    pub removes: bool,
}

pub struct ZoneIndexer {
    pub reader: Reader,
    /// `to_lez_block_id` of the last Verified batch: where the next batch must start.
    pub verified_to: u64,
    /// The Verified batches, in Bedrock order. The mock Bedrock has no reorgs, so every
    /// Verified batch is at the reference depth at once.
    pub history: Vec<VerifiedBatch>,
    /// The tree at `verified_to`: the newest valid root, where proof paths are served from.
    verified_reader: Reader,
    /// LEZ blocks applied after `verified_to`, to bring `verified_reader` forward.
    unverified_blocks: Vec<Block>,
    /// Rejected batches, kept as evidence.
    rejected: Vec<String>,
}

impl ZoneIndexer {
    pub fn new(tree_id: [u8; 32]) -> Self {
        Self {
            reader: Reader::new(tree_id),
            verified_to: FIRST_LEZ_BLOCK_ID - 1,
            history: Vec::new(),
            verified_reader: Reader::new(tree_id),
            unverified_blocks: Vec::new(),
            rejected: Vec::new(),
        }
    }

    pub fn rejected(&self) -> &[String] {
        &self.rejected
    }

    pub fn verified(&self) -> u64 {
        self.history.len() as u64
    }

    /// Apply a LEZ block, as every reader does, as soon as it is closed.
    pub fn apply_block(&mut self, block: &Block) {
        self.reader.apply_block(block);
        self.unverified_blocks.push(block.clone());
    }

    // Serving (RFC "Zone Indexer / Serving").

    /// The valid roots, newest last: the Verified batches from the last one holding a
    /// `Remove` on, no older in LEZ blocks than the window length.
    pub fn valid_roots(&self) -> Vec<VerifiedBatch> {
        let Some(newest) = self.history.last() else {
            return vec![];
        };
        let start = self
            .history
            .iter()
            .rposition(|batch| batch.removes)
            .unwrap_or(0);
        self.history[start..]
            .iter()
            .filter(|batch| batch.to_lez_block_id + WINDOW_LENGTH_BLOCKS >= newest.to_lez_block_id)
            .cloned()
            .collect()
    }

    pub fn newest_valid_root(&self) -> Option<&VerifiedBatch> {
        self.history.last()
    }

    /// A leaf's Merkle proof path at the newest valid root, if the leaf is live in it.
    pub fn proof_path(&self, id_commitment: &[u8; 32]) -> Option<(Vec<Fr>, Vec<u8>)> {
        self.verified_reader.proof_path(id_commitment)
    }

    /// The tree at the newest valid root, for what a Module asks about its own leaf.
    pub fn served_tree(&self) -> &Reader {
        &self.verified_reader
    }

    /// The membership state the indexer reads from LEZ.
    pub fn membership_state<'a>(
        &self,
        lez: &'a MockLez,
        id_commitment: &[u8; 32],
    ) -> MembershipAccount<'a> {
        lez.membership_account(id_commitment)
    }

    /// The latest LEZ `timestamp` the indexer has read.
    pub fn clock(&self, lez: &MockLez) -> u64 {
        lez.now()
    }

    /// A snapshot for another Zone indexer that starts: the tree at the last Verified batch's
    /// `to_lez_block_id`, to be checked against that batch and against LEZ before use.
    pub fn snapshot(&self) -> (u64, Reader) {
        (self.verified_to, self.verified_reader.clone())
    }

    /// Start from another Zone indexer's snapshot instead of replaying LEZ: the snapshot is
    /// tagged with the `to_lez_block_id` of `batch`, its root must equal `root_after`, and its
    /// leaves must match LEZ; the snapshot is trusted for nothing else. The LEZ blocks since
    /// are applied as usual afterwards.
    pub fn sync(
        &mut self,
        snapshot: (u64, Reader),
        batch: &Batch,
        lez: &MockLez,
    ) -> Result<(), String> {
        let (tag, mut reader) = snapshot;
        if tag != batch.to_lez_block_id || reader.applied_block_id != tag {
            return Err(format!(
                "snapshot at LEZ block {tag} is not the batch's block {}",
                batch.to_lez_block_id
            ));
        }
        let root_after: [u8; 32] = batch
            .root_after
            .as_slice()
            .try_into()
            .map_err(|_| "root_after is not 32 bytes")?;
        if reader.root() != root_after {
            return Err("snapshot root differs from the batch".into());
        }
        reader.check_against_lez(lez)?;
        self.verified_to = tag;
        self.history = vec![VerifiedBatch {
            to_lez_block_id: tag,
            root_after,
            removes: batch
                .changes
                .iter()
                .any(|c| matches!(c.kind, Some(Kind::Remove(_)))),
        }];
        self.verified_reader = reader.clone();
        self.reader = reader;
        self.unverified_blocks.clear();
        Ok(())
    }

    // Checking (RFC "Zone Indexer").

    /// Check one batch. A Rejected batch is recorded and skipped; `verified_to` stays where the
    /// last Verified batch left it.
    pub fn on_batch(&mut self, payload: &[u8]) -> Result<Batch, String> {
        let result = self.check(payload);
        if let Err(reason) = &result {
            self.rejected.push(reason.clone());
        }
        result
    }

    fn check(&mut self, payload: &[u8]) -> Result<Batch, String> {
        let batch = Batch::decode(payload).map_err(|e| format!("undecodable batch: {e}"))?;

        // Range: continues the last Verified batch, no gap, no overlap.
        let expected_from = self.verified_to + 1;
        if batch.from_lez_block_id != expected_from
            || batch.to_lez_block_id < batch.from_lez_block_id
        {
            return Err(format!(
                "range [{}..{}] is not contiguous, expected to start at {expected_from}",
                batch.from_lez_block_id, batch.to_lez_block_id
            ));
        }

        // Changes: what the derivation rules gave for that range. A LEZ block the indexer
        // cannot read yet would only delay the check; the mock serves every closed block, so
        // here it is an error.
        let Some(expected) = self
            .reader
            .changes_in(batch.from_lez_block_id, batch.to_lez_block_id)
        else {
            return Err(format!(
                "covers LEZ block {} beyond the last block {}",
                batch.to_lez_block_id, self.reader.applied_block_id
            ));
        };
        if expected != batch.changes {
            // An omission is provable censorship (RFC "Zone Indexer", check 2).
            let reason = expected
                .iter()
                .find(|e| !batch.changes.contains(e))
                .map_or_else(
                    || "extra changes in batch".into(),
                    |e| format!("{} omitted (provable censorship)", e.describe()),
                );
            return Err(format!("changes differ from LEZ: {reason}"));
        }

        // Root: the root after the changes.
        let root_after: [u8; 32] = batch
            .root_after
            .as_slice()
            .try_into()
            .map_err(|_| "root_after is not 32 bytes")?;
        if self.reader.root_at(batch.to_lez_block_id) != Some(root_after) {
            return Err("root_after does not match the derived root".into());
        }

        self.verified_to = batch.to_lez_block_id;
        self.history.push(VerifiedBatch {
            to_lez_block_id: batch.to_lez_block_id,
            root_after,
            removes: batch
                .changes
                .iter()
                .any(|c| matches!(c.kind, Some(Kind::Remove(_)))),
        });
        // Bring the served tree up to the Verified batch.
        let still_unverified = self
            .unverified_blocks
            .iter()
            .position(|b| b.block_id > self.verified_to)
            .unwrap_or(self.unverified_blocks.len());
        for block in self.unverified_blocks.drain(..still_unverified) {
            self.verified_reader.apply_block(&block);
        }
        Ok(batch)
    }
}

#[cfg(test)]
pub(crate) mod test {
    use super::*;
    use crate::{
        batch::Change,
        lez::{
            Event,
            test::{TREE_ID, id_commitment, new_keys, new_lez, register, slash},
        },
    };

    /// An indexer that has applied every LEZ block.
    pub(crate) fn synced(lez: &MockLez) -> ZoneIndexer {
        let mut indexer = ZoneIndexer::new(lez.config.tree_id);
        for block in lez.blocks(1, lez.last_block_id()) {
            indexer.apply_block(block);
        }
        indexer
    }

    pub(crate) fn batch(reader: &Reader, from: u64, to: u64, changes: Vec<Change>) -> Vec<u8> {
        Batch {
            from_lez_block_id: from,
            to_lez_block_id: to,
            changes,
            root_after: reader.root_at(to).unwrap().to_vec(),
        }
        .encode_to_vec()
    }

    /// Verify honest batches, one per LEZ block, up to `to`.
    pub(crate) fn verify_through(indexer: &mut ZoneIndexer, to: u64) {
        let reader = indexer.reader.clone();
        for block_id in indexer.verified_to + 1..=to {
            let changes = reader.changes_in(block_id, block_id).unwrap();
            indexer
                .on_batch(&batch(&reader, block_id, block_id, changes))
                .unwrap();
        }
    }

    #[test]
    fn test_rejected_batch_changes_nothing() {
        let mut lez = new_lez();
        let keys = new_keys();
        lez.submit(register("alice", &keys, 8_000)).unwrap();
        lez.produce_block(2_000);
        lez.submit(slash("carol", &keys)).unwrap();
        lez.produce_block(2_000);
        lez.produce_block(2_000);
        let mut indexer = synced(&lez);
        let reader = indexer.reader.clone();
        let honest = |from, to| batch(&reader, from, to, reader.changes_in(from, to).unwrap());
        // Block 1 Verified; the served tree stops there even though the reader is at block 3.
        assert!(indexer.on_batch(&honest(1, 1)).is_ok());
        assert_eq!(indexer.verified_to, 1);
        assert_eq!(indexer.served_tree().applied_block_id, 1);
        assert!(indexer.proof_path(&id_commitment(&keys)).is_some());
        // Block 2 without the Remove: Rejected, the mark stays, no new valid root.
        let censored = batch(&reader, 2, 2, vec![]);
        let err = indexer.on_batch(&censored).unwrap_err();
        assert!(err.contains("Remove leaf 0 omitted"), "{err}");
        assert_eq!((indexer.verified_to, indexer.valid_roots().len()), (1, 1));
        // A batch continuing from the Rejected one is Rejected too.
        assert!(indexer.on_batch(&honest(3, 3)).is_err());
        // The correct block 2 batch is Verified: the window resets at its Remove.
        assert!(indexer.on_batch(&honest(2, 2)).is_ok());
        assert_eq!((indexer.verified_to, indexer.verified()), (2, 2));
        assert_eq!(indexer.valid_roots().len(), 1);
        assert!(indexer.proof_path(&id_commitment(&keys)).is_none());
        assert_eq!(indexer.rejected().len(), 2);
    }

    #[test]
    fn test_sync_checks_the_snapshot_against_the_batch_and_lez() {
        let mut lez = new_lez();
        let (alice, bob) = (new_keys(), new_keys());
        lez.submit(register("alice", &alice, 20_000)).unwrap();
        lez.submit(register("bob", &bob, 20_000)).unwrap();
        lez.produce_block(2_000);
        lez.produce_block(2_000);
        let mut service = synced(&lez);
        verify_through(&mut service, 2);
        let batch = Batch::decode(
            batch(
                &service.reader,
                2,
                2,
                service.reader.changes_in(2, 2).unwrap(),
            )
            .as_slice(),
        )
        .unwrap();
        // A new indexer syncs from the snapshot, then applies block 3 like any reader.
        lez.produce_block(2_000);
        let mut own = ZoneIndexer::new(TREE_ID);
        own.sync(service.snapshot(), &batch, &lez).unwrap();
        for block in lez.blocks(3, 3) {
            own.apply_block(block);
        }
        assert_eq!((own.verified_to, own.reader.applied_block_id), (2, 3));
        assert_eq!(
            own.proof_path(&id_commitment(&bob)).map(|(p, _)| p.len()),
            Some(20)
        );
        // A snapshot whose expiry for alice is wrong (same tree, same root): LEZ corrects it.
        let (tag, mut tampered) = service.snapshot();
        tampered.apply_block(&Block {
            block_id: tag,
            timestamp: 4_000,
            events: vec![Event::Extended {
                tree_id: TREE_ID,
                id_commitment: id_commitment(&alice),
                expiry_timestamp_ms: 99_000,
            }],
        });
        own.sync((tag, tampered), &batch, &lez).unwrap();
        assert_eq!(
            own.served_tree()
                .live_leaf(&id_commitment(&alice))
                .unwrap()
                .expiry_timestamp_ms,
            20_000
        );
        // A snapshot with another tree: the root check fails.
        let (tag, mut other) = service.snapshot();
        other.apply_block(&Block {
            block_id: tag,
            timestamp: 4_000,
            events: vec![Event::Slashed {
                tree_id: TREE_ID,
                id_commitment: id_commitment(&bob),
            }],
        });
        assert!(own.sync((tag, other), &batch, &lez).is_err());
    }

    #[test]
    fn test_valid_roots_are_bounded_by_the_window_length() {
        let mut lez = new_lez();
        lez.submit(register("alice", &new_keys(), 60_000)).unwrap();
        for _ in 0..6 {
            lez.produce_block(2_000);
        }
        let mut indexer = synced(&lez);
        verify_through(&mut indexer, 6);
        // Six Verified batches, no Remove: only those within 3 LEZ blocks of the newest.
        let roots: Vec<_> = indexer
            .valid_roots()
            .iter()
            .map(|b| b.to_lez_block_id)
            .collect();
        assert_eq!(roots, vec![3, 4, 5, 6]);
    }
}
