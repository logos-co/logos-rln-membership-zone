//! Zone indexer verification (RFC "Zone Indexer Verification"): re-derive every finalized batch
//! from LEZ and accept it only if the range, the changes and the root all match.

use prost::Message;

use crate::{
    batch::Batch,
    lez::MockLez,
    membership_set::{MembershipSet, fr_to_bytes_le},
};

pub struct ZoneIndexer {
    pub membership_set: MembershipSet,
    pub verified: u64,
    halted: Option<String>,
}

impl ZoneIndexer {
    pub fn new() -> Self {
        Self {
            membership_set: MembershipSet::new(),
            verified: 0,
            halted: None,
        }
    }

    pub fn halted(&self) -> Option<&str> {
        self.halted.as_deref()
    }

    /// Verify one finalized batch. A rejected batch halts the Zone indexer: the RFC requires
    /// rejecting it and every later batch.
    pub fn on_batch(&mut self, payload: &[u8], lez: &MockLez) -> Result<Batch, String> {
        if let Some(reason) = &self.halted {
            return Err(format!("halted earlier: {reason}"));
        }
        let result = self.verify(payload, lez);
        if let Err(reason) = &result {
            self.halted = Some(reason.clone());
        }
        result
    }

    fn verify(&mut self, payload: &[u8], lez: &MockLez) -> Result<Batch, String> {
        let batch = Batch::decode(payload).map_err(|e| format!("undecodable batch: {e}"))?;

        // Invariant 3, range: contiguous with the previous batch, no gap, no overlap.
        let expected_from = self.membership_set.lez_to + 1;
        if batch.lez_from != expected_from || batch.lez_to < batch.lez_from {
            return Err(format!(
                "range [{}..{}] is not contiguous, expected to start at {expected_from}",
                batch.lez_from, batch.lez_to
            ));
        }
        // Invariant 4: only finalized LEZ blocks (every mock block is finalized once produced).
        if batch.lez_to > lez.last_finalized_block_id() {
            return Err(format!(
                "covers LEZ block {} beyond the last finalized block {}",
                batch.lez_to,
                lez.last_finalized_block_id()
            ));
        }

        // Invariants 2 and 3, changes: re-derive from LEZ and compare.
        let mut next = self.membership_set.clone();
        let mut expected = vec![];
        for block in lez.blocks(batch.lez_from, batch.lez_to) {
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

        // Invariant 5: the root after the changes.
        if batch.root_after != fr_to_bytes_le(&next.root()) {
            return Err("root_after does not match the replayed root".into());
        }

        self.membership_set = next;
        self.verified += 1;
        Ok(batch)
    }
}
