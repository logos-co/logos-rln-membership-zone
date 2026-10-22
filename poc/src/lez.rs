//! What the PoC reads from LEZ (RFC "Derivation Rules"): the closed blocks of the registration
//! program's chain, each with its timestamp and the program's events. The source is behind a
//! trait so the sequencer runs the same way against the LEZ Indexer, the LEZ Sequencer once it
//! serves events, or a stand-in clock while the program is not deployed.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;

/// One event of the registration program (RFC "LEZ Requirements / Events").
// Constructed by the LEZ Indexer source once the program is deployed; the ticking stand-in
// closes empty blocks only.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Registered {
        tree_id: [u8; 32],
        id_commitment: [u8; 32],
        rate_limit: u64,
        expiry_timestamp_ms: u64,
        leaf_index: u64,
    },
    Extended {
        tree_id: [u8; 32],
        id_commitment: [u8; 32],
        expiry_timestamp_ms: u64,
    },
    Erased {
        tree_id: [u8; 32],
        id_commitment: [u8; 32],
    },
    Slashed {
        tree_id: [u8; 32],
        id_commitment: [u8; 32],
    },
}

impl Event {
    pub fn name(&self) -> &'static str {
        match self {
            Event::Registered { .. } => "Registered",
            Event::Extended { .. } => "Extended",
            Event::Erased { .. } => "Erased",
            Event::Slashed { .. } => "Slashed",
        }
    }
}

/// A closed LEZ block as a reader sees it: its id, its timestamp (the LEZ clock, ms) and the
/// registration program's events in transaction order.
#[derive(Debug, Clone)]
pub struct Block {
    pub block_id: u64,
    pub timestamp_ms: u64,
    pub events: Vec<Event>,
}

/// A source of closed LEZ blocks.
pub trait LezSource: Send + Sync {
    /// Short name for logs.
    fn describe(&self) -> String;
    /// `block_id` of the latest closed block this source serves.
    fn last_block_id(&mut self) -> Result<u64>;
    /// The blocks `from..=to`, in order; `to` is at most `last_block_id()`.
    fn blocks(&mut self, from: u64, to: u64) -> Result<Vec<Block>>;
}

/// A stand-in while the registration program is not deployed: one empty block per
/// `block_sec` of wall-clock time, `block_id = unix seconds / block_sec`, so a restart
/// continues the same numbering. Exercises every other part: batches, publishing, finality,
/// the HTTP API.
pub struct TickingLez {
    block_sec: u64,
}

impl TickingLez {
    pub fn new(block_sec: u64) -> Self {
        Self {
            block_sec: block_sec.max(1),
        }
    }

    pub fn current_block_id(&self) -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        now / self.block_sec
    }
}

impl LezSource for TickingLez {
    fn describe(&self) -> String {
        format!("ticking stand-in, one empty block per {} s", self.block_sec)
    }

    fn last_block_id(&mut self) -> Result<u64> {
        Ok(self.current_block_id())
    }

    fn blocks(&mut self, from: u64, to: u64) -> Result<Vec<Block>> {
        Ok((from..=to)
            .map(|block_id| Block {
                block_id,
                timestamp_ms: block_id * self.block_sec * 1000,
                events: vec![],
            })
            .collect())
    }
}
