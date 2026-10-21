//! The Zone sequencer (RFC "Zone Sequencer"): a reader that turns the LEZ blocks it has not
//! covered yet into one batch and publishes it on Bedrock. Publishing itself goes through the
//! Zone SDK in `main.rs`; this is the batch building.

use anyhow::{Result, anyhow};
use lb_core::mantle::ops::channel::inscribe::Inscription;
use prost::Message;

use crate::{
    batch::Batch,
    lez::{Block, Event, MockLez},
    reader::Reader,
};

pub struct ZoneSequencer {
    reader: Reader,
    /// Omit every `Slashed` event from the tree and the batches: provable censorship.
    dishonest: bool,
}

impl ZoneSequencer {
    pub fn new(tree_id: [u8; 32], dishonest: bool) -> Self {
        Self {
            reader: Reader::new(tree_id),
            dishonest,
        }
    }

    /// Last LEZ block covered by a batch this sequencer published.
    pub fn covered_block_id(&self) -> u64 {
        self.reader.applied_block_id
    }

    /// Build the next batch from every LEZ block after the last covered one, ending early at a
    /// block boundary when the encoded batch would not fit in one inscription. Returns the
    /// batch, its inscription and the reader state to commit once the batch is published.
    pub fn build_batch(&self, lez: &MockLez) -> Result<(Batch, Inscription, Reader)> {
        let from = self.reader.applied_block_id + 1;
        let mut next = self.reader.clone();
        let mut changes = vec![];
        let mut fitted = None;
        for block in lez.blocks(from, lez.last_block_id()) {
            // The dishonest Zone sequencer censors every `Slashed`: it applies the block
            // without them, so both its changes and its root leave the slashed leaf in the tree.
            let censored;
            let block = if self.dishonest {
                censored = Block {
                    events: block
                        .events
                        .iter()
                        .filter(|event| !matches!(event, Event::Slashed { .. }))
                        .cloned()
                        .collect(),
                    ..block.clone()
                };
                &censored
            } else {
                block
            };
            changes.extend(next.apply_block(block));
            let batch = Batch {
                from_lez_block_id: from,
                to_lez_block_id: block.block_id,
                changes: changes.clone(),
                root_after: next.root().to_vec(),
            };
            match Inscription::try_from(batch.encode_to_vec()) {
                Ok(inscription) => fitted = Some((batch, inscription, next.clone())),
                Err(_) => break,
            }
        }
        fitted.ok_or_else(|| anyhow!("LEZ block {from} alone exceeds one inscription"))
    }

    /// The batch is published: adopt the reader state it was built from.
    pub fn commit(&mut self, next: Reader) {
        self.reader = next;
    }
}
