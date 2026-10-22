//! What the PoC reads from LEZ (RFC "Derivation Rules"): the closed blocks of the registration
//! program's chain, each with its timestamp and the program's events. Two sources: a LEZ
//! Indexer's JSON-RPC (`getEvents`, finalized blocks only), and a stand-in clock that closes
//! empty blocks for runs without LEZ.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

/// One event of the registration program (RFC "LEZ Requirements / Events").
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
pub enum Source {
    Ticking(TickingLez),
    Indexer(IndexerLez),
}

impl Source {
    /// Short name for logs.
    pub fn describe(&self) -> String {
        match self {
            Source::Ticking(t) => {
                format!("ticking stand-in, one empty block per {} s", t.block_sec)
            }
            Source::Indexer(i) => format!(
                "LEZ Indexer {} (finalized blocks), program {}",
                i.url, i.program_account_id
            ),
        }
    }

    /// `block_id` of the latest closed block this source serves.
    pub async fn last_block_id(&self) -> Result<u64> {
        match self {
            Source::Ticking(t) => Ok(t.current_block_id()),
            Source::Indexer(i) => i.last_finalized_block_id().await,
        }
    }

    /// The blocks `from..=to`, in order; `to` is at most `last_block_id()`.
    pub async fn blocks(&self, from: u64, to: u64) -> Result<Vec<Block>> {
        match self {
            Source::Ticking(t) => Ok(t.blocks(from, to)),
            Source::Indexer(i) => i.blocks(from, to).await,
        }
    }
}

/// A stand-in while no LEZ source is wired: one empty block per `block_sec` of wall-clock
/// time, `block_id = unix seconds / block_sec`, so a restart continues the same numbering.
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

    fn blocks(&self, from: u64, to: u64) -> Vec<Block> {
        (from..=to)
            .map(|block_id| Block {
                block_id,
                timestamp_ms: block_id * self.block_sec * 1000,
                events: vec![],
            })
            .collect()
    }
}

/// The 8-byte selectors of the four events: the first 8 bytes of
/// `sha256("<program>::<Event>")` (LEZ `ProgramEvent` convention).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Selectors {
    pub registered: [u8; 8],
    pub extended: [u8; 8],
    pub erased: [u8; 8],
    pub slashed: [u8; 8],
}

impl Selectors {
    pub fn from_program_name(name: &str) -> Self {
        let selector = |event: &str| {
            let digest = Sha256::digest(format!("{name}::{event}").as_bytes());
            let mut out = [0u8; 8];
            out.copy_from_slice(&digest[..8]);
            out
        };
        Self {
            registered: selector("Registered"),
            extended: selector("Extended"),
            erased: selector("Erased"),
            slashed: selector("Slashed"),
        }
    }
}

/// A LEZ Indexer's JSON-RPC: `getEvents` for the program's events (finalized blocks only,
/// at most 1000 blocks per call), `getBlockById` for each block's timestamp.
pub struct IndexerLez {
    client: reqwest::Client,
    url: String,
    /// Base58 account id, as the indexer's JSON expects.
    program_account_id: String,
    /// Known selectors; without them events are told apart by their borsh length, which
    /// cannot separate `Erased` from `Slashed` (both remove the leaf).
    selectors: Option<Selectors>,
}

#[derive(Deserialize)]
struct EventRecord {
    block_id: u64,
    tx_index: u32,
    selector: String,
    data: String,
}

#[derive(Deserialize)]
struct BlockHeader {
    block_id: u64,
    timestamp: u64,
}

#[derive(Deserialize)]
struct IndexerBlock {
    header: BlockHeader,
}

impl IndexerLez {
    pub fn new(url: String, program_account_id: String, selectors: Option<Selectors>) -> Self {
        Self {
            client: reqwest::Client::new(),
            url,
            program_account_id,
            selectors,
        }
    }

    async fn rpc<T: for<'de> Deserialize<'de>>(&self, method: &str, params: Value) -> Result<T> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let response: Value = self
            .client
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("{method}: request to {}", self.url))?
            .json()
            .await
            .with_context(|| format!("{method}: response is not JSON"))?;
        if let Some(error) = response.get("error") {
            bail!("{method}: {error}");
        }
        let result = response
            .get("result")
            .cloned()
            .ok_or_else(|| anyhow!("{method}: no result"))?;
        serde_json::from_value(result).with_context(|| format!("{method}: unexpected result"))
    }

    pub async fn last_finalized_block_id(&self) -> Result<u64> {
        let id: Option<u64> = self.rpc("getLastFinalizedBlockId", json!([])).await?;
        id.ok_or_else(|| anyhow!("the LEZ Indexer has no finalized block yet"))
    }

    async fn blocks(&self, from: u64, to: u64) -> Result<Vec<Block>> {
        let filter = json!([{
            "from_block": from,
            "to_block": to,
            "program_account_id": self.program_account_id,
        }]);
        let mut records: Vec<EventRecord> = self.rpc("getEvents", filter).await?;
        records.sort_by_key(|r| (r.block_id, r.tx_index));
        let mut blocks = Vec::with_capacity((to - from + 1) as usize);
        let mut next = records.into_iter().peekable();
        for block_id in from..=to {
            let block: Option<IndexerBlock> = self.rpc("getBlockById", json!([block_id])).await?;
            let header = block
                .ok_or_else(|| anyhow!("the LEZ Indexer does not hold block {block_id}"))?
                .header;
            let mut events = vec![];
            while let Some(record) = next.next_if(|r| r.block_id == block_id) {
                match self.decode(&record) {
                    Ok(event) => events.push(event),
                    Err(e) => println!(
                        "LEZ block {block_id} tx {}: event skipped: {e}",
                        record.tx_index
                    ),
                }
            }
            blocks.push(Block {
                block_id: header.block_id,
                timestamp_ms: header.timestamp,
                events,
            });
        }
        Ok(blocks)
    }

    /// Decode one event record: borsh of the RFC structs, all fields fixed size, so a plain
    /// field-by-field read is the whole decoder.
    fn decode(&self, record: &EventRecord) -> Result<Event> {
        let data = BASE64
            .decode(&record.data)
            .context("event data is not base64")?;
        let selector = {
            let bytes = hex::decode(&record.selector).context("selector is not hex")?;
            <[u8; 8]>::try_from(bytes).map_err(|_| anyhow!("selector is not 8 bytes"))?
        };
        let kind = match self.selectors {
            Some(s) if selector == s.registered => "Registered",
            Some(s) if selector == s.extended => "Extended",
            Some(s) if selector == s.erased => "Erased",
            Some(s) if selector == s.slashed => "Slashed",
            Some(_) => bail!("unknown selector {}", record.selector),
            // No selectors given: tell the events apart by their length.
            None => match data.len() {
                88 => "Registered",
                72 => "Extended",
                64 => "Erased",
                n => bail!("unknown selector {} with {n} bytes", record.selector),
            },
        };
        let mut cursor = Cursor(&data);
        let tree_id = cursor.bytes32()?;
        let id_commitment = cursor.bytes32()?;
        let event = match kind {
            "Registered" => Event::Registered {
                tree_id,
                id_commitment,
                rate_limit: cursor.u64()?,
                expiry_timestamp_ms: cursor.u64()?,
                leaf_index: cursor.u64()?,
            },
            "Extended" => Event::Extended {
                tree_id,
                id_commitment,
                expiry_timestamp_ms: cursor.u64()?,
            },
            "Erased" => Event::Erased {
                tree_id,
                id_commitment,
            },
            _ => Event::Slashed {
                tree_id,
                id_commitment,
            },
        };
        if !cursor.0.is_empty() {
            bail!(
                "{} bytes left after decoding {}",
                cursor.0.len(),
                event.name()
            );
        }
        Ok(event)
    }
}

struct Cursor<'a>(&'a [u8]);

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        if self.0.len() < n {
            bail!("event data too short");
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }

    fn bytes32(&mut self) -> Result<[u8; 32]> {
        Ok(<[u8; 32]>::try_from(self.take(32)?).expect("32 bytes"))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(
            <[u8; 8]>::try_from(self.take(8)?).expect("8 bytes"),
        ))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_decode_registered_by_length() {
        let lez = IndexerLez::new("http://unused".into(), "x".into(), None);
        let mut data = vec![7u8; 32];
        data.extend([9u8; 32]);
        data.extend(100u64.to_le_bytes());
        data.extend(5_000u64.to_le_bytes());
        data.extend(3u64.to_le_bytes());
        let record = EventRecord {
            block_id: 1,
            tx_index: 0,
            selector: "0000000000000000".into(),
            data: BASE64.encode(&data),
        };
        assert_eq!(
            lez.decode(&record).unwrap(),
            Event::Registered {
                tree_id: [7; 32],
                id_commitment: [9; 32],
                rate_limit: 100,
                expiry_timestamp_ms: 5_000,
                leaf_index: 3,
            }
        );
    }

    #[test]
    fn test_selectors_follow_the_program_event_convention() {
        let s = Selectors::from_program_name("rln");
        let expected = &Sha256::digest(b"rln::Registered")[..8];
        assert_eq!(&s.registered[..], expected);
        assert_ne!(s.registered, s.slashed);
    }
}
