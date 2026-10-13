//! Zone indexer verification (RFC "Zone Indexer"): recompute every batch from LEZ and accept
//! it only if the batch range, the changes and the root all match. A rejected batch changes
//! nothing; the next batch must continue from the last verified one.

use prost::Message;

use crate::{
    batch::Batch,
    lez::MockLez,
    membership_set::{MembershipSet, fr_to_bytes_le},
};

pub struct ZoneIndexer {
    pub membership_set: MembershipSet,
    pub verified: u64,
    /// Rejected batches, kept as evidence.
    rejected: Vec<String>,
}

impl ZoneIndexer {
    pub fn new() -> Self {
        Self {
            membership_set: MembershipSet::new(),
            verified: 0,
            rejected: Vec::new(),
        }
    }

    pub fn rejected(&self) -> &[String] {
        &self.rejected
    }

    /// Verify one batch. A rejected batch is recorded and skipped; `to_lez_block_id` stays
    /// where the last verified batch left it.
    pub fn on_batch(&mut self, payload: &[u8], lez: &MockLez) -> Result<Batch, String> {
        let result = self.verify(payload, lez);
        if let Err(reason) = &result {
            self.rejected.push(reason.clone());
        }
        result
    }

    fn verify(&mut self, payload: &[u8], lez: &MockLez) -> Result<Batch, String> {
        let batch = Batch::decode(payload).map_err(|e| format!("undecodable batch: {e}"))?;

        // Check 1, range: contiguous with the previous batch, no gap, no overlap.
        let expected_from = self.membership_set.to_lez_block_id + 1;
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
        if batch.to_lez_block_id > lez.last_finalized_block_id() {
            return Err(format!(
                "covers LEZ block {} beyond the last block {}",
                batch.to_lez_block_id,
                lez.last_finalized_block_id()
            ));
        }

        // Check 2, changes: recompute from LEZ and compare.
        let mut next = self.membership_set.clone();
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

        // Check 3, root: the root after the changes.
        if batch.root_after != fr_to_bytes_le(&next.root()) {
            return Err("root_after does not match the replayed root".into());
        }

        self.membership_set = next;
        self.verified += 1;
        Ok(batch)
    }
}
