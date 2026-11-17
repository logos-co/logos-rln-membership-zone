//! The LEZ chain as the zone keeps it, after LEZ's `chain_state` applied to LEZ's own channel
//! on Bedrock: a final tier (the LEZ blocks whose inscription Bedrock finalized, in the
//! persisted `Reader`) and a view (the adopted ones, chained on the final block), the head
//! being the final reader folded over the view. The blocks come from the LEZ channel through
//! the Zone SDK (`adopted`, `finalized`, `Conflict`), so the zone never commits on Bedrock to
//! a LEZ block that is not on Bedrock; the LEZ Sequencer's RPC only fills finalized history,
//! pinned by the hash chain to a block Bedrock finalized.

use anyhow::{Result, bail};
use tracing::warn;

use crate::{
    batch::Change,
    lez::Block,
    reader::{BlockIngestError, Reader},
};

/// What an adopted block did to the head.
#[derive(Debug)]
pub enum Adopted {
    /// Chained on the head and applied; its changes, in derivation-rule order.
    Applied(Vec<Change>),
    /// The head already holds it (a re-delivery).
    AlreadyApplied,
    /// Did not chain on the head: a block the SDK reports out of order, or a lineage this head
    /// does not follow (a `Conflict` is due).
    DoesNotChain(BlockIngestError),
}

/// What a finalized block did to the final tier.
#[derive(Debug)]
pub enum Finalized {
    /// Chained on the final block and applied; `new_to_head` when the head had not seen it as
    /// an adopted block (the stream ran ahead of the view, or a restart), with its changes;
    /// `view_dropped` when the view held another block at its id: the adopted blocks are gone
    /// from the head, with everything derived from them.
    Applied {
        changes: Vec<Change>,
        new_to_head: bool,
        view_dropped: bool,
    },
    /// The final tier already holds it.
    AlreadyApplied,
    /// Blocks are missing before it: the caller fills `expected..block_id` from the LEZ
    /// Sequencer, pinned by the hash chain, then applies it again.
    Gap { expected: u64 },
}

pub struct LezChain {
    /// Finalized LEZ blocks only; the store on disk.
    final_reader: Reader,
    /// Adopted blocks not finalized yet, chained on the final block, lineage order.
    view: Vec<Block>,
    /// The final reader folded over the view: a temporary store, rebuilt from the final
    /// reader on start and on every `Conflict`.
    head: Reader,
}

impl LezChain {
    pub fn new(final_reader: Reader) -> Result<Self> {
        let head = final_reader.clone_temporary()?;
        Ok(Self {
            final_reader,
            view: vec![],
            head,
        })
    }

    pub fn final_reader(&self) -> &Reader {
        &self.final_reader
    }

    pub fn head(&self) -> &Reader {
        &self.head
    }

    pub fn view(&self) -> &[Block] {
        &self.view
    }

    pub fn persist(&mut self) -> Result<()> {
        self.final_reader.persist()
    }

    pub fn close(&mut self) -> Result<()> {
        self.final_reader.close()?;
        self.head.close()
    }

    /// An adopted LEZ block (the SDK's `Extension`): onto the head and into the view.
    pub fn apply_adopted(&mut self, block: &Block) -> Result<Adopted> {
        if block.block_id() <= self.head.lez_block_id {
            let known = self
                .view
                .iter()
                .any(|held| held.block_id() == block.block_id() && held.hash() == block.hash())
                || (block.block_id() <= self.final_reader.lez_block_id
                    && self.final_reader.last_block_hash().is_some_and(|h| {
                        block.block_id() != self.final_reader.lez_block_id || h == block.hash()
                    }));
            return Ok(if known {
                Adopted::AlreadyApplied
            } else {
                Adopted::DoesNotChain(BlockIngestError::UnexpectedBlockId {
                    expected: self.head.lez_block_id + 1,
                    got: block.block_id(),
                })
            });
        }
        if let Err(e) = self.head.check_chain(std::slice::from_ref(block)) {
            return Ok(Adopted::DoesNotChain(e));
        }
        let changes = self.head.apply_block(block)?;
        self.view.push(block.clone());
        Ok(Adopted::Applied(changes))
    }

    /// Back to the final tier: the head is the final reader again and the view is empty (a
    /// Bedrock reorg, the SDK's `Conflict`; the caller adopts the canonical lineage again).
    pub fn reset_to_final(&mut self) -> Result<()> {
        self.head = self.final_reader.clone_temporary()?;
        self.view.clear();
        Ok(())
    }

    /// The view persisted by the last run, adopted again onto a head at the final tier; the
    /// changes of every block applied, in order. Stops at the first block that does not chain
    /// (the SDK refills what is missing).
    pub fn restore_view(&mut self, blocks: &[Block]) -> Result<Vec<(u64, Vec<Change>)>> {
        let mut restored = vec![];
        for block in blocks {
            match self.apply_adopted(block)? {
                Adopted::Applied(changes) => restored.push((block.block_id(), changes)),
                Adopted::AlreadyApplied => {}
                Adopted::DoesNotChain(e) => {
                    warn!(
                        "persisted LEZ block {} does not chain on the finalized tree: {e}; the \
                         view restarts there",
                        block.block_id()
                    );
                    break;
                }
            }
        }
        Ok(restored)
    }

    /// A LEZ block Bedrock finalized (the SDK's `finalized`, or the RPC filling a gap): onto
    /// the final reader, out of the view.
    pub fn apply_finalized(&mut self, block: &Block) -> Result<Finalized> {
        if block.block_id() <= self.final_reader.lez_block_id {
            return Ok(Finalized::AlreadyApplied);
        }
        match self.final_reader.check_chain(std::slice::from_ref(block)) {
            Ok(()) => {}
            Err(BlockIngestError::UnexpectedBlockId { expected, .. }) => {
                return Ok(Finalized::Gap { expected });
            }
            Err(e @ BlockIngestError::BrokenChainLink { .. }) => bail!(
                "finalized LEZ block {} does not chain on the final tier ({e}): the final tier \
                 holds a history Bedrock did not finalize; start once with REBUILD_TREE=true",
                block.block_id()
            ),
        }
        let changes = self.final_reader.apply_block(block)?;
        let (new_to_head, view_dropped) = match self.view.first() {
            Some(held) if held.hash() == block.hash() => {
                self.view.remove(0);
                (false, false)
            }
            Some(held) => {
                // The view sits on a branch Bedrock did not finalize and no `Conflict` said
                // so: the head follows the final tier again, the SDK refills the view.
                warn!(
                    "LEZ block {} finalized with hash {}.., the view held {}..: {} adopted \
                     block(s) dropped, the head is the final tier again",
                    block.block_id(),
                    hex::encode(&block.hash()[..4]),
                    hex::encode(&held.hash()[..4]),
                    self.view.len()
                );
                self.view.clear();
                self.head = self.final_reader.clone_temporary()?;
                (true, true)
            }
            None => {
                // Nothing adopted ahead of the final tier: the head is at the final block and
                // takes this one too.
                self.head.apply_block(block)?;
                (true, false)
            }
        };
        Ok(Finalized::Applied {
            changes,
            new_to_head,
            view_dropped,
        })
    }
}

/// Blocks read from a LEZ Sequencer to fill a gap are trusted only through the hash chain:
/// `filler` must be the blocks `expected..` in order, chained on `prev` (the hash of the block
/// before them; `None` when there is none) and leading to `block`, a block Bedrock adopted or
/// finalized. Checked before anything is applied.
pub fn check_filler(
    prev: Option<[u8; 32]>,
    expected: u64,
    filler: &[Block],
    block: &Block,
) -> Result<(), BlockIngestError> {
    let mut prev = prev;
    for (id, candidate) in (expected..).zip(filler.iter().chain(std::iter::once(block))) {
        if candidate.block_id() != id {
            return Err(BlockIngestError::UnexpectedBlockId {
                expected: id,
                got: candidate.block_id(),
            });
        }
        if let Some(prev) = prev
            && candidate.prev_block_hash() != prev
        {
            return Err(BlockIngestError::BrokenChainLink {
                block_id: candidate.block_id(),
                expected_prev: prev,
                got_prev: candidate.prev_block_hash(),
            });
        }
        prev = Some(candidate.hash());
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::lez::{ProgramEvent, Registered, RlnEvent};

    fn id(n: u8) -> [u8; 32] {
        let mut id = [0u8; 32];
        id[0] = n;
        id
    }

    fn registered(n: u8, leaf_index: u64) -> ProgramEvent {
        ProgramEvent::from(RlnEvent::Registered(Registered {
            tree_id: [0; 32],
            id_commitment: id(n),
            rate_limit: 100,
            expiry_timestamp_ms: u64::MAX,
            leaf_index,
        }))
    }

    /// Block `block_id` with hash `hash` chaining on hash `prev`.
    fn block(block_id: u64, prev: u8, hash: u8, events: Vec<ProgramEvent>) -> Block {
        Block {
            header: crate::lez::BlockHeader {
                block_id,
                prev_block_hash: common::HashType([prev; 32]),
                hash: common::HashType([hash; 32]),
                timestamp: 1_000 * block_id,
            },
            events,
            private_transaction_count: 0,
        }
    }

    fn chain() -> LezChain {
        let reader = Reader::open_temporary([0; 32], 0).unwrap();
        LezChain::new(reader).unwrap()
    }

    #[test]
    fn test_adopted_blocks_fold_on_the_head_and_finality_moves_them_to_the_final_tier() {
        let mut chain = chain();
        let b1 = block(1, 0, 1, vec![registered(1, 0)]);
        let b2 = block(2, 1, 2, vec![]);
        assert!(matches!(chain.apply_adopted(&b1).unwrap(), Adopted::Applied(c) if c.len() == 1));
        assert!(matches!(chain.apply_adopted(&b2).unwrap(), Adopted::Applied(c) if c.is_empty()));
        assert!(matches!(
            chain.apply_adopted(&b1).unwrap(),
            Adopted::AlreadyApplied
        ));
        assert_eq!(chain.head().lez_block_id, 2);
        assert_eq!(chain.final_reader().lez_block_id, 0);
        assert_eq!(chain.view().len(), 2);
        assert_eq!(chain.head().active_count(), 1);
        // A block that does not chain is refused, the head untouched.
        assert!(matches!(
            chain.apply_adopted(&block(3, 9, 3, vec![])).unwrap(),
            Adopted::DoesNotChain(BlockIngestError::BrokenChainLink { block_id: 3, .. })
        ));
        // Finality reaches block 1: into the final tier, out of the view, nothing new to the
        // head.
        assert!(matches!(
            chain.apply_finalized(&b1).unwrap(),
            Finalized::Applied {
                new_to_head: false,
                ..
            }
        ));
        assert_eq!(chain.final_reader().lez_block_id, 1);
        assert_eq!(chain.view().len(), 1);
        assert_eq!(chain.final_reader().root(), chain.head().root());
        assert!(matches!(
            chain.apply_finalized(&b1).unwrap(),
            Finalized::AlreadyApplied
        ));
        // A gap: block 4 before block 2 and 3 are final.
        assert!(matches!(
            chain.apply_finalized(&block(4, 3, 4, vec![])).unwrap(),
            Finalized::Gap { expected: 2 }
        ));
    }

    #[test]
    fn test_a_conflict_rebuilds_the_head_from_the_final_tier() {
        let mut chain = chain();
        let b1 = block(1, 0, 1, vec![registered(1, 0)]);
        chain.apply_adopted(&b1).unwrap();
        chain.apply_finalized(&b1).unwrap();
        let root_1 = chain.head().root();
        // Adopted: block 2 registers leaf 1. A reorg replaces it by a block 2 without events.
        chain
            .apply_adopted(&block(2, 1, 2, vec![registered(2, 1)]))
            .unwrap();
        assert_eq!(chain.head().active_count(), 2);
        let other_2 = block(2, 1, 7, vec![]);
        let other_3 = block(3, 7, 8, vec![registered(3, 1)]);
        chain.reset_to_final().unwrap();
        assert_eq!(chain.head().lez_block_id, 1);
        assert_eq!(
            chain
                .restore_view(&[b1.clone(), other_2, other_3])
                .unwrap()
                .len(),
            2
        );
        assert_eq!(chain.view().len(), 2);
        assert_eq!(chain.head().lez_block_id, 3);
        assert_eq!(chain.head().active_count(), 2);
        assert_ne!(chain.head().root(), root_1);
        assert_eq!(chain.head().root_at(1), Some(root_1));
        // Finality on another hash than the view's first: the view is dropped, the head is
        // the final tier again, and the block counts as new to the head.
        let finalized_2 = block(2, 1, 9, vec![]);
        assert!(matches!(
            chain.apply_finalized(&finalized_2).unwrap(),
            Finalized::Applied {
                new_to_head: true,
                ..
            }
        ));
        assert!(chain.view().is_empty());
        assert_eq!(chain.head().lez_block_id, 2);
        assert_eq!(chain.head().root(), root_1);
        // A finalized block that does not chain on the final tier is fatal.
        assert!(chain.apply_finalized(&block(3, 5, 6, vec![])).is_err());
    }

    /// The outcome says when the view was dropped by finality, so the owner can drop what it
    /// derived from the adopted blocks.
    #[test]
    fn test_finality_on_another_hash_reports_the_view_dropped() {
        let mut chain = chain();
        let b1 = block(1, 0, 1, vec![]);
        chain.apply_adopted(&b1).unwrap();
        assert!(matches!(
            chain.apply_finalized(&b1).unwrap(),
            Finalized::Applied {
                new_to_head: false,
                view_dropped: false,
                ..
            }
        ));
        chain
            .apply_adopted(&block(2, 1, 2, vec![registered(2, 0)]))
            .unwrap();
        assert!(matches!(
            chain.apply_finalized(&block(2, 1, 9, vec![])).unwrap(),
            Finalized::Applied {
                new_to_head: true,
                view_dropped: true,
                ..
            }
        ));
        assert!(matches!(
            chain.apply_finalized(&block(3, 9, 3, vec![])).unwrap(),
            Finalized::Applied {
                new_to_head: true,
                view_dropped: false,
                ..
            }
        ));
    }

    #[test]
    fn test_check_filler_wants_the_chain_from_prev_to_the_block() {
        let b3 = block(3, 2, 3, vec![]);
        let b4 = block(4, 3, 4, vec![]);
        let b5 = block(5, 4, 5, vec![]);
        // Before the first block nothing is known: the ids and the links decide.
        check_filler(None, 3, &[b3.clone(), b4.clone()], &b5).unwrap();
        check_filler(Some([2; 32]), 3, &[b3.clone(), b4.clone()], &b5).unwrap();
        // Nothing missing: the block itself must chain on prev.
        check_filler(Some([4; 32]), 5, &[], &b5).unwrap();
        assert!(matches!(
            check_filler(Some([7; 32]), 5, &[], &b5),
            Err(BlockIngestError::BrokenChainLink { block_id: 5, .. })
        ));
        // A gap, a block out of order, a link to another history.
        assert!(matches!(
            check_filler(None, 3, std::slice::from_ref(&b3), &b5),
            Err(BlockIngestError::UnexpectedBlockId {
                expected: 4,
                got: 5
            })
        ));
        assert!(matches!(
            check_filler(None, 3, &[b4.clone(), b3.clone()], &b5),
            Err(BlockIngestError::UnexpectedBlockId {
                expected: 3,
                got: 4
            })
        ));
        assert!(matches!(
            check_filler(Some([9; 32]), 3, &[b3, b4], &b5),
            Err(BlockIngestError::BrokenChainLink { block_id: 3, .. })
        ));
    }
}
