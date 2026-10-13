//! The Zone indexer (RFC "Zone Indexer"): a reader that checks every batch on the Zone channel
//! against LEZ (range, changes, root) and keeps the verdicts. A Rejected batch changes nothing;
//! the next batch must continue from the last Verified one. It can also serve its tree as a
//! snapshot to a Module that syncs.

use prost::Message;

use crate::{
    batch::Batch,
    lez::MockLez,
    reader::{Reader, fr_to_bytes_le},
};

pub struct ZoneIndexer {
    pub reader: Reader,
    pub verified: u64,
    /// Rejected batches, kept as evidence.
    rejected: Vec<String>,
}

impl ZoneIndexer {
    pub fn new() -> Self {
        Self {
            reader: Reader::new(),
            verified: 0,
            rejected: Vec::new(),
        }
    }

    pub fn rejected(&self) -> &[String] {
        &self.rejected
    }

    /// A snapshot for a syncing Module: the tree after the last Verified batch. The Module
    /// checks it against that batch's `root_after` (RFC "Module Requirements / Sync").
    #[allow(dead_code)]
    pub fn snapshot(&self) -> Reader {
        self.reader.clone()
    }

    /// Check one batch. A Rejected batch is recorded and skipped; the reader stays where the
    /// last Verified batch left it.
    pub fn on_batch(&mut self, payload: &[u8], lez: &MockLez) -> Result<Batch, String> {
        let result = self.check(payload, lez);
        if let Err(reason) = &result {
            self.rejected.push(reason.clone());
        }
        result
    }

    fn check(&mut self, payload: &[u8], lez: &MockLez) -> Result<Batch, String> {
        let batch = Batch::decode(payload).map_err(|e| format!("undecodable batch: {e}"))?;

        // Range: continues the last Verified batch, no gap, no overlap.
        let expected_from = self.reader.applied_block_id + 1;
        if batch.from_lez_block_id != expected_from
            || batch.to_lez_block_id < batch.from_lez_block_id
        {
            return Err(format!(
                "range [{}..{}] is not contiguous, expected to start at {expected_from}",
                batch.from_lez_block_id, batch.to_lez_block_id
            ));
        }
        // A LEZ block the indexer cannot read yet would only delay the check; the mock serves
        // every produced block, so here it is an error.
        if batch.to_lez_block_id > lez.last_block_id() {
            return Err(format!(
                "covers LEZ block {} beyond the last block {}",
                batch.to_lez_block_id,
                lez.last_block_id()
            ));
        }

        // Changes: recompute from LEZ and compare.
        let mut next = self.reader.clone();
        let mut expected = vec![];
        for block in lez.blocks(batch.from_lez_block_id, batch.to_lez_block_id) {
            expected.extend(next.apply_block(block));
        }
        if expected != batch.changes {
            let missing = expected
                .iter()
                .find(|e| !batch.changes.contains(e))
                .map_or_else(|| "extra changes in batch".into(), |e| e.describe());
            return Err(format!(
                "changes differ from LEZ (provable censorship or forgery): {missing}"
            ));
        }

        // Root: the root after the changes.
        if batch.root_after != fr_to_bytes_le(&next.root()) {
            return Err("root_after does not match the replayed root".into());
        }

        self.reader = next;
        self.verified += 1;
        Ok(batch)
    }
}
