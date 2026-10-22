//! What the PoC reads from LEZ (RFC "Derivation Rules"): the closed blocks of the registration
//! program's chain, each with its timestamp and the program's events. Two sources: a LEZ
//! Indexer's JSON-RPC (`getEvents`, finalized blocks only), and a stand-in clock that closes
//! empty blocks for runs without LEZ.

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use rln::prelude::{Hasher, PoseidonHash};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::reader::{fr_from_bytes_le, fr_to_bytes_le};

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
    Sequencer(SequencerLez),
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
            Source::Sequencer(q) => format!(
                "LEZ Sequencer {} (closed blocks, events derived from the program's \
                 transactions), program {}",
                q.url,
                hex::encode(q.program_account_id)
            ),
        }
    }

    /// `block_id` of the latest closed block this source serves.
    pub async fn last_block_id(&self) -> Result<u64> {
        match self {
            Source::Ticking(t) => Ok(t.current_block_id()),
            Source::Indexer(i) => i.last_finalized_block_id().await,
            Source::Sequencer(q) => q.last_block_id().await,
        }
    }

    /// The blocks `from..=to`, in order; `to` is at most `last_block_id()`.
    pub async fn blocks(&self, from: u64, to: u64) -> Result<Vec<Block>> {
        match self {
            Source::Ticking(t) => Ok(t.blocks(from, to)),
            Source::Indexer(i) => i.blocks(from, to).await,
            Source::Sequencer(q) => q.blocks(from, to).await,
        }
    }

    /// Tell the source how many `Registered` the reader has applied, so a source that
    /// derives `leaf_index` itself continues the count after a restart from a snapshot.
    pub fn set_registered_count(&self, count: u64) {
        if let Source::Sequencer(q) = self {
            q.registered.store(count, Ordering::Relaxed);
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

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(
            <[u8; 4]>::try_from(self.take(4)?).expect("4 bytes"),
        ))
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

/// The LEZ Sequencer's JSON-RPC (`getLastBlockId`, `getBlockRange`, at most 1024 blocks per
/// call, closed blocks included before finality). The RPC serves no events, so the events
/// are derived from the program's transactions instead: a block holds successful
/// transactions only, so every `Register` to the program is a `Registered` whose
/// `leaf_index` is the count of earlier ones, every `Slash` a `Slashed` of
/// `poseidon(identity_secret)`, and so on. Same tree as from the events; the input differs
/// from the RFC's.
pub struct SequencerLez {
    client: reqwest::Client,
    url: String,
    program_account_id: [u8; 32],
    /// `Registered` seen so far, the next `leaf_index`.
    registered: AtomicU64,
    /// How long a membership lives after the timestamp its `Register` carries; the program
    /// computes the expiry itself and the transaction does not carry it. `None` = unknown:
    /// leaves never expire in this PoC until the program's rule is confirmed.
    active_duration_ms: Option<u64>,
}

/// `MAX_BLOCK_RANGE_LEN` of the LEZ Sequencer.
pub const SEQUENCER_RANGE: u64 = 1024;

impl SequencerLez {
    pub fn new(url: String, program_account_id: [u8; 32], active_duration_ms: Option<u64>) -> Self {
        Self {
            client: reqwest::Client::new(),
            url,
            program_account_id,
            registered: AtomicU64::new(0),
            active_duration_ms,
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

    pub async fn last_block_id(&self) -> Result<u64> {
        self.rpc("getLastBlockId", json!([])).await
    }

    async fn blocks(&self, from: u64, to: u64) -> Result<Vec<Block>> {
        let mut blocks = vec![];
        let mut start = from;
        while start <= to {
            let end = to.min(start + SEQUENCER_RANGE - 1);
            let encoded: Vec<String> = self.rpc("getBlockRange", json!([start, end])).await?;
            if encoded.len() as u64 != end - start + 1 {
                bail!(
                    "getBlockRange {start}..{end} returned {} blocks",
                    encoded.len()
                );
            }
            for text in &encoded {
                let bytes = BASE64.decode(text).context("block is not base64")?;
                blocks.push(self.decode_block(&bytes)?);
            }
            start = end + 1;
        }
        Ok(blocks)
    }

    /// Borsh of `Block{header{block_id u64, prev_block_hash [32], hash [32], timestamp u64},
    /// body{transactions: Vec<LeeTransaction>}, bedrock_status u8}`.
    fn decode_block(&self, bytes: &[u8]) -> Result<Block> {
        let mut c = Cursor(bytes);
        let block_id = c.u64()?;
        c.take(64)?;
        let timestamp_ms = c.u64()?;
        let tx_count = c.u32()?;
        let mut events = vec![];
        for tx_index in 0..tx_count {
            match self.decode_transaction(&mut c) {
                Ok(Some(event)) => events.push(event),
                Ok(None) => {}
                // The rest of the body cannot be located after a bad transaction.
                Err(e) => bail!("LEZ block {block_id} tx {tx_index}: {e}"),
            }
        }
        Ok(Block {
            block_id,
            timestamp_ms,
            events,
        })
    }

    /// One `LeeTransaction`: `0` = Public(`PublicTransaction{message{program_account_id [32],
    /// shard_selectors: Vec<[64]>, nonces: Vec<u128>, instruction_data: Vec<u8>, fee:
    /// Option<FeeDeclaration>}, witness_set}`), `1` = PrivacyPreserving, whose layout is not
    /// decoded here: a block holding one cannot be read past it (LEZ v0.3.1 layouts).
    fn decode_transaction(&self, c: &mut Cursor<'_>) -> Result<Option<Event>> {
        let tag = c.u8()?;
        if tag != 0 {
            bail!("privacy-preserving transactions are not decoded here");
        }
        let program = c.bytes32()?;
        let selectors = c.u32()?;
        c.take(selectors as usize * 64)?;
        let nonces = c.u32()?;
        c.take(nonces as usize * 16)?;
        let data_len = c.u32()? as usize;
        let data = c.take(data_len)?.to_vec();
        self.skip_fee_and_witnesses(c)?;
        if program != self.program_account_id {
            return Ok(None);
        }
        match self.decode_instruction(&data) {
            Ok(event) => Ok(event),
            Err(e) => {
                println!(
                    "transaction to the program skipped ({} bytes): {e}",
                    data.len()
                );
                Ok(None)
            }
        }
    }

    /// `fee: Option<FeeDeclaration{payer [32], gas_limit u64, tip u64, max_fee u128}>` then
    /// `WitnessSet{signatures_and_public_keys: Vec<([64], [32])>}`.
    fn skip_fee_and_witnesses(&self, c: &mut Cursor<'_>) -> Result<()> {
        if c.u8()? == 1 {
            c.take(32 + 8 + 8 + 16)?;
        }
        let witnesses = c.u32()?;
        c.take(witnesses as usize * 96)?;
        Ok(())
    }

    /// The program's `Instruction` as deployed on the testnet (read off real transactions,
    /// 2026-10-22; lez-rln style, durations in seconds): borsh enum
    /// `0 InitConfig{tree_id, price_per_unit u128, account [32], registration_fee u64,
    /// grace_period_sec u32, withdrawal_delay_sec u32}` (no event),
    /// `1 Register{tree_id, id_commitment, rate_limit u64, registered_at_ms u64 (the client's
    /// clock, 80-340 s before the block on the five registrations seen), deposit u128,
    /// grace_period_sec u32, withdrawal_delay_sec u32, leaf_index u64}`; the expiry is not in
    /// the transaction, see `active_duration_ms`.
    /// `Extend`, `Erase` and `Slash` are assumed to follow as tags 2, 3, 4 with the RFC
    /// fields; none has been seen on chain yet, so they are decoded best effort and a
    /// transaction that does not fit is skipped with a log line, not an error.
    fn decode_instruction(&self, data: &[u8]) -> Result<Option<Event>> {
        let mut c = Cursor(data);
        let tag = c.u8()?;
        let tree_id = c.bytes32()?;
        let event = match tag {
            0 => return Ok(None),
            1 => {
                let id_commitment = c.bytes32()?;
                let rate_limit = c.u64()?;
                let registered_at_ms = c.u64()?;
                c.take(16 + 4 + 4)?;
                let leaf_index = c.u64()?;
                let expiry_timestamp_ms = self
                    .active_duration_ms
                    .map_or(u64::MAX, |d| registered_at_ms.saturating_add(d));
                let expected = self.registered.fetch_add(1, Ordering::Relaxed);
                if leaf_index != expected {
                    println!(
                        "Register carries leaf_index {leaf_index}, {expected} Registered seen \
                         before it: following the instruction"
                    );
                    self.registered.store(leaf_index + 1, Ordering::Relaxed);
                }
                Event::Registered {
                    tree_id,
                    id_commitment,
                    rate_limit,
                    expiry_timestamp_ms,
                    leaf_index,
                }
            }
            2 => Event::Extended {
                tree_id,
                id_commitment: c.bytes32()?,
                expiry_timestamp_ms: c.u64()?,
            },
            3 => Event::Erased {
                tree_id,
                id_commitment: c.bytes32()?,
            },
            4 => {
                let secret =
                    fr_from_bytes_le(&c.bytes32()?).map_err(|e| anyhow!("identity_secret: {e}"))?;
                Event::Slashed {
                    tree_id,
                    id_commitment: fr_to_bytes_le(&Hasher::<PoseidonHash>::hash_single(secret)),
                }
            }
            n => bail!("unknown instruction tag {n}"),
        };
        if !c.0.is_empty() {
            bail!(
                "{} bytes left after decoding {} as tag {tag}",
                c.0.len(),
                event.name()
            );
        }
        Ok(Some(event))
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
    fn test_decode_register_and_slash_instructions() {
        let lez = SequencerLez::new("http://unused".into(), [1; 32], Some(1_000));
        let keys = rln::prelude::IdentityKeys::generate::<PoseidonHash, rand::rngs::ThreadRng>(
            &mut rand::thread_rng(),
        );
        // The deployed Register layout, with the leaf index last.
        let register = |leaf_index: u64| {
            let mut b = vec![1u8];
            b.extend([7u8; 32]);
            b.extend(fr_to_bytes_le(&keys.id_commitment()));
            b.extend(100u64.to_le_bytes());
            b.extend(5_000u64.to_le_bytes());
            b.extend(10_000u128.to_le_bytes());
            b.extend(2_592_000u32.to_le_bytes());
            b.extend(604_800u32.to_le_bytes());
            b.extend(leaf_index.to_le_bytes());
            b
        };
        let first = lez.decode_instruction(&register(0)).unwrap().unwrap();
        let second = lez.decode_instruction(&register(1)).unwrap().unwrap();
        assert!(matches!(
            first,
            Event::Registered {
                leaf_index: 0,
                rate_limit: 100,
                expiry_timestamp_ms: 6_000,
                ..
            }
        ));
        assert!(matches!(second, Event::Registered { leaf_index: 1, .. }));
        let mut init = vec![0u8];
        init.extend([7u8; 32]);
        init.extend([0u8; 64]);
        assert_eq!(lez.decode_instruction(&init).unwrap(), None);
        let mut slash = vec![4u8];
        slash.extend([7u8; 32]);
        slash.extend(fr_to_bytes_le(&keys.identity_secret()));
        assert_eq!(
            lez.decode_instruction(&slash).unwrap(),
            Some(Event::Slashed {
                tree_id: [7; 32],
                id_commitment: fr_to_bytes_le(&keys.id_commitment()),
            })
        );
    }

    #[test]
    fn test_decode_block_with_two_public_transactions() {
        let program = [1u8; 32];
        let lez = SequencerLez::new("http://unused".into(), program, None);
        let tx = |to: [u8; 32], data: &[u8]| {
            let mut b = vec![0u8];
            b.extend(to);
            b.extend(1u32.to_le_bytes());
            b.extend([5u8; 64]);
            b.extend(1u32.to_le_bytes());
            b.extend(9u128.to_le_bytes());
            b.extend((data.len() as u32).to_le_bytes());
            b.extend(data);
            b.push(1);
            b.extend([2u8; 64]);
            b.extend(1u32.to_le_bytes());
            b.extend([3u8; 96]);
            b
        };
        let mut erase = vec![3u8];
        erase.extend([7u8; 32]);
        erase.extend([8u8; 32]);
        let mut block = vec![];
        block.extend(42u64.to_le_bytes());
        block.extend([0u8; 64]);
        block.extend(1_234u64.to_le_bytes());
        block.extend(2u32.to_le_bytes());
        block.extend(tx([4u8; 32], b"other program"));
        block.extend(tx(program, &erase));
        block.push(0);
        let decoded = lez.decode_block(&block).unwrap();
        assert_eq!(decoded.block_id, 42);
        assert_eq!(decoded.timestamp_ms, 1_234);
        assert_eq!(
            decoded.events,
            vec![Event::Erased {
                tree_id: [7; 32],
                id_commitment: [8; 32],
            }]
        );
    }

    /// LEZ testnet block 50056 as `getBlockRange` returned it on 2026-10-22: three public
    /// transactions, the second a `Register` to the program (leaf 0).
    #[test]
    fn test_decode_real_testnet_block_50056() {
        let program = <[u8; 32]>::try_from(
            hex::decode("f04803944507677bb79b27b61cd0ad6eea81928f281570347e2df6249495106a")
                .unwrap(),
        )
        .unwrap();
        let lez = SequencerLez::new("http://unused".into(), program, None);
        let bytes = BASE64.decode(BLOCK_50056).unwrap();
        let block = lez.decode_block(&bytes).unwrap();
        assert_eq!(block.block_id, 50056);
        assert_eq!(block.timestamp_ms, 1_792_597_956_640);
        assert_eq!(block.events.len(), 1);
        let Event::Registered {
            tree_id,
            id_commitment,
            rate_limit,
            expiry_timestamp_ms,
            leaf_index,
        } = &block.events[0]
        else {
            panic!("expected Registered");
        };
        assert_eq!(
            hex::encode(tree_id),
            "47b2de4f08080e74e2d4beffe8e5e87e709dd598619c9b322b64f2756979b4cf"
        );
        assert!(hex::encode(id_commitment).starts_with("a878f8b554fabd"));
        assert_eq!(*rate_limit, 100);
        // No active duration given: the leaf does not expire in the PoC.
        assert_eq!(*expiry_timestamp_ms, u64::MAX);
        assert_eq!(*leaf_index, 0);
    }

    const BLOCK_50056: &str = "iMMAAAAAAABZhMkDiBFQefGZ7yzYj0cPBeEsaLOs++4lMW1TJiFl+qByf/Q+2TfS0jWzHtIGqjiBU+rjIipAwqGe5a/WQW8iIBgqX6EBAAADAAAAAPBIA5RFB2d7t5snthzQrW7qgZKPKBVwNH4t9iSUlRBqBQAAAKDd99hEF9cqCOvklZ2NQgjRxFPjzUQx7fRvD4BhkbCu8EgDlEUHZ3u3mye2HNCtbuqBko8oFXA0fi32JJSVEGrqYNmkMWEGw7bOAPerVg97ZjuPL1ygw6TubO4Lpg25nwAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAalJnme5xzjOU5XpgOaQuUf1FXvZU2aumquwEo+HVks0AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAC9MRVovQ2xvY2tQcm9ncmFtQWNjb3VudC8wMDAwMDUwGVig/GIyp8fm3q4azY1sQqMsvgATFdUcGC/uReCwMpxgqS3Ugdrc0Z4maEa/C/exlYedBgaBAe6KBfUhcJKmOPBIA5RFB2d7t5snthzQrW7qgZKPKBVwNH4t9iSUlRBqAQAAAAAAAAAAAAAAAAAAAAAAAABxAAAAAUey3k8ICA504tS+/+jl6H5wndWYYZybMitk8nVpebTPqHj4tVT6ve3OcgoYXEkrENG4MAIo04TaTIAuLSAn+yZkAAAAAAAAAJ/fKF+hAQAAECcAAAAAAAAAAAAAAAAAAACNJwCAOgkAAAAAAAAAAAAB6mDZpDFhBsO2zgD3q1YPe2Y7jy9coMOk7mzuC6YNuZ+AhB4AAAAAAAAAAAAAAAAAAMgCCAAAAAAAAAAAAAAAAAEAAAB/KBMTcIgm6gU+LFxOhbl3QeN2Sv6l+TcLoyuNqgSjN0hncnoHxrioqFe7umL+bG3LZE4tOBv7C6yavnZatJN5/DsPyPnWwuZyNyGyUKqM5Y9Qi45aDJUmXNmXM4+0r5EAFb2gtaXWMpX0kzR2aj0aes3op7YaztKtfjNMVXNEPfIEAAAAWGJBEKsBRrEh46GSS5a1QFI1VuJL1cfAflKr+l/rr4sVvaC1pdYylfSTNHZqPRp6zeinthrO0q1+M0xVc0Q98n8aXp3JgWudVUWv3G1IexPUosaP4ro7HRU2ALOVpbjEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAByJOq24C+LN9hBsWO+uLAUcKQuc4dezM/Nchgr4bE7XAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAasoMxKiRfFSYI+7p9qPiW4yVFVRIFBRvlZdpP2a6uY4AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABBAAAAAAPNAAAAAAAAkwIAAAAAAACwfAYAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAHIhAAAAAAAAAAAAAAAAAAAAAAAAAAAZWKD8YjKnx+berhrNjWxCoyy+ABMV1RwYL+5F4LAynAMAAAAvTEVaL0Nsb2NrUHJvZ3JhbUFjY291bnQvMDAwMDAwMRlYoPxiMqfH5t6uGs2NbEKjLL4AExXVHBgv7kXgsDKcL0xFWi9DbG9ja1Byb2dyYW1BY2NvdW50LzAwMDAwMTAZWKD8YjKnx+berhrNjWxCoyy+ABMV1RwYL+5F4LAynC9MRVovQ2xvY2tQcm9ncmFtQWNjb3VudC8wMDAwMDUwGVig/GIyp8fm3q4azY1sQqMsvgATFdUcGC/uReCwMpwAAAAAEAAAACAYKl+hAQAAiMMAAAAAAAAAAAAAAAI=";

    #[test]
    fn test_selectors_follow_the_program_event_convention() {
        let s = Selectors::from_program_name("rln");
        let expected = &Sha256::digest(b"rln::Registered")[..8];
        assert_eq!(&s.registered[..], expected);
        assert_ne!(s.registered, s.slashed);
    }
}
