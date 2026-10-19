//! The Zone indexer (RFC "Zone Indexer"): a reader that follows LEZ like every other reader,
//! checks every batch on the Zone channel against what it derived (range, changes, root) and
//! keeps the verdicts. A Rejected batch changes nothing: the next batch must continue from the
//! last Verified one. It can also serve its tree as a snapshot to a Module that syncs.

use prost::Message;

use crate::{
    batch::Batch,
    lez::{Block, FIRST_LEZ_BLOCK_ID},
    reader::Reader,
};

pub struct ZoneIndexer {
    pub reader: Reader,
    /// `to_lez_block_id` of the last Verified batch: where the next batch must start.
    pub verified_to: u64,
    pub verified: u64,
    /// The tree at `verified_to`: what a snapshot may hand out, since a Module can check it.
    verified_reader: Reader,
    /// LEZ blocks applied after `verified_to`, to bring `verified_reader` forward.
    unverified_blocks: Vec<Block>,
    /// Rejected batches, kept as evidence.
    rejected: Vec<String>,
}

impl ZoneIndexer {
    pub fn new() -> Self {
        Self {
            reader: Reader::new(),
            verified_to: FIRST_LEZ_BLOCK_ID - 1,
            verified: 0,
            verified_reader: Reader::new(),
            unverified_blocks: Vec::new(),
            rejected: Vec::new(),
        }
    }

    pub fn rejected(&self) -> &[String] {
        &self.rejected
    }

    /// Apply a LEZ block, as every reader does, as soon as it is closed.
    pub fn apply_block(&mut self, block: &Block) {
        self.reader.apply_block(block);
        self.unverified_blocks.push(block.clone());
    }

    /// A snapshot for a syncing Module: the tree at the last Verified batch's
    /// `to_lez_block_id`, so the Module can check its root against that batch before applying
    /// the LEZ blocks since (RFC "Module Requirements / Sync").
    #[allow(dead_code)]
    pub fn snapshot(&self) -> (u64, Reader) {
        (self.verified_to, self.verified_reader.clone())
    }

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
        self.verified += 1;
        // Bring the snapshot tree up to the Verified batch.
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
mod test {
    use super::*;
    use crate::{
        batch::Change,
        lez::{
            Action, MockLez,
            test::{new_keys, new_lez, register},
        },
    };

    /// An indexer that has applied every LEZ block.
    fn synced(lez: &MockLez) -> ZoneIndexer {
        let mut indexer = ZoneIndexer::new();
        for block in lez.blocks(1, lez.last_block_id()) {
            indexer.apply_block(block);
        }
        indexer
    }

    fn batch(reader: &Reader, from: u64, to: u64, changes: Vec<Change>) -> Vec<u8> {
        Batch {
            from_lez_block_id: from,
            to_lez_block_id: to,
            changes,
            root_after: reader.root_at(to).unwrap().to_vec(),
        }
        .encode_to_vec()
    }

    #[test]
    fn test_rejected_batch_changes_nothing() {
        let mut lez = new_lez();
        let keys = new_keys();
        lez.submit(register("alice", &keys, 8_000)).unwrap();
        lez.produce_block(2_000);
        lez.submit(Action::Slash {
            slasher: "carol".into(),
            identity_secret: *keys.identity_secret(),
        })
        .unwrap();
        lez.produce_block(2_000);
        lez.produce_block(2_000);
        let mut indexer = synced(&lez);
        let reader = indexer.reader.clone();
        let honest = |from, to| batch(&reader, from, to, reader.changes_in(from, to).unwrap());
        // Block 1 Verified; the snapshot stops there even though the tree is at block 3.
        assert!(indexer.on_batch(&honest(1, 1)).is_ok());
        assert_eq!(indexer.verified_to, 1);
        let (tag, snapshot) = indexer.snapshot();
        assert_eq!((tag, snapshot.applied_block_id), (1, 1));
        assert_eq!(snapshot.root_at(1), reader.root_at(1));
        // Block 2 without the Remove: Rejected, the mark stays.
        let censored = batch(&reader, 2, 2, vec![]);
        let err = indexer.on_batch(&censored).unwrap_err();
        assert!(err.contains("Remove leaf 0 omitted"), "{err}");
        assert_eq!(indexer.verified_to, 1);
        // A batch continuing from the Rejected one is Rejected too.
        assert!(indexer.on_batch(&honest(3, 3)).is_err());
        // The correct block 2 batch is Verified.
        assert!(indexer.on_batch(&honest(2, 2)).is_ok());
        assert_eq!((indexer.verified_to, indexer.verified), (2, 2));
        assert_eq!(indexer.snapshot().1.applied_block_id, 2);
        assert_eq!(indexer.rejected().len(), 2);
    }
}
