//! What the Zone sequencer reads from LEZ (RFC "Derivation Rules"): the closed blocks of the
//! registration program's chain, each with its timestamp and the program's events. The source
//! is a LEZ Sequencer's JSON-RPC, the public testnet one or one run next to it; it serves no
//! events, so they are derived from the program's transactions in each block.

use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context as _, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use rln::prelude::{Hasher, PoseidonHash};
use serde::Deserialize;
use serde_json::{Value, json};

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
    /// leaves never expire until the program's rule is confirmed.
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

    /// Short name for logs.
    pub fn describe(&self) -> String {
        format!(
            "LEZ Sequencer {} (closed blocks, events derived from the program's transactions), \
             program {}, membership lifetime {}",
            self.url,
            hex::encode(self.program_account_id),
            self.active_duration_ms
                .map_or("unknown (leaves never expire)".to_owned(), |d| format!(
                    "{d} ms"
                ))
        )
    }

    /// Tell the source how many `Registered` the reader has applied, so the `leaf_index`
    /// check continues the count after a restart from a snapshot.
    pub fn set_registered_count(&self, count: u64) {
        self.registered.store(count, Ordering::Relaxed);
    }

    /// `block_id` of the latest closed block.
    pub async fn last_block_id(&self) -> Result<u64> {
        self.rpc("getLastBlockId", json!([])).await
    }

    /// The blocks `from..=to`, in order, with the program's events of each.
    pub async fn blocks(&self, from: u64, to: u64) -> Result<Vec<Block>> {
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

/// A borsh reader over fixed layouts: everything decoded here is fixed size or
/// length-prefixed, so a plain field-by-field read is the whole decoder.
struct Cursor<'a>(&'a [u8]);

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        if self.0.len() < n {
            bail!("data too short");
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

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(
            <[u8; 8]>::try_from(self.take(8)?).expect("8 bytes"),
        ))
    }

    fn bytes32(&mut self) -> Result<[u8; 32]> {
        Ok(<[u8; 32]>::try_from(self.take(32)?).expect("32 bytes"))
    }
}

#[cfg(test)]
mod test {
    use super::*;

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
        // No active duration given: the leaf does not expire.
        assert_eq!(*expiry_timestamp_ms, u64::MAX);
        assert_eq!(*leaf_index, 0);
    }

    const BLOCK_50056: &str = "iMMAAAAAAABZhMkDiBFQefGZ7yzYj0cPBeEsaLOs++4lMW1TJiFl+qByf/Q+2TfS0jWzHtIGqjiBU+rjIipAwqGe5a/WQW8iIBgqX6EBAAADAAAAAPBIA5RFB2d7t5snthzQrW7qgZKPKBVwNH4t9iSUlRBqBQAAAKDd99hEF9cqCOvklZ2NQgjRxFPjzUQx7fRvD4BhkbCu8EgDlEUHZ3u3mye2HNCtbuqBko8oFXA0fi32JJSVEGrqYNmkMWEGw7bOAPerVg97ZjuPL1ygw6TubO4Lpg25nwAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAalJnme5xzjOU5XpgOaQuUf1FXvZU2aumquwEo+HVks0AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAC9MRVovQ2xvY2tQcm9ncmFtQWNjb3VudC8wMDAwMDUwGVig/GIyp8fm3q4azY1sQqMsvgATFdUcGC/uReCwMpxgqS3Ugdrc0Z4maEa/C/exlYedBgaBAe6KBfUhcJKmOPBIA5RFB2d7t5snthzQrW7qgZKPKBVwNH4t9iSUlRBqAQAAAAAAAAAAAAAAAAAAAAAAAABxAAAAAUey3k8ICA504tS+/+jl6H5wndWYYZybMitk8nVpebTPqHj4tVT6ve3OcgoYXEkrENG4MAIo04TaTIAuLSAn+yZkAAAAAAAAAJ/fKF+hAQAAECcAAAAAAAAAAAAAAAAAAACNJwCAOgkAAAAAAAAAAAAB6mDZpDFhBsO2zgD3q1YPe2Y7jy9coMOk7mzuC6YNuZ+AhB4AAAAAAAAAAAAAAAAAAMgCCAAAAAAAAAAAAAAAAAEAAAB/KBMTcIgm6gU+LFxOhbl3QeN2Sv6l+TcLoyuNqgSjN0hncnoHxrioqFe7umL+bG3LZE4tOBv7C6yavnZatJN5/DsPyPnWwuZyNyGyUKqM5Y9Qi45aDJUmXNmXM4+0r5EAFb2gtaXWMpX0kzR2aj0aes3op7YaztKtfjNMVXNEPfIEAAAAWGJBEKsBRrEh46GSS5a1QFI1VuJL1cfAflKr+l/rr4sVvaC1pdYylfSTNHZqPRp6zeinthrO0q1+M0xVc0Q98n8aXp3JgWudVUWv3G1IexPUosaP4ro7HRU2ALOVpbjEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAByJOq24C+LN9hBsWO+uLAUcKQuc4dezM/Nchgr4bE7XAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAasoMxKiRfFSYI+7p9qPiW4yVFVRIFBRvlZdpP2a6uY4AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABBAAAAAAPNAAAAAAAAkwIAAAAAAACwfAYAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAHIhAAAAAAAAAAAAAAAAAAAAAAAAAAAZWKD8YjKnx+berhrNjWxCoyy+ABMV1RwYL+5F4LAynAMAAAAvTEVaL0Nsb2NrUHJvZ3JhbUFjY291bnQvMDAwMDAwMRlYoPxiMqfH5t6uGs2NbEKjLL4AExXVHBgv7kXgsDKcL0xFWi9DbG9ja1Byb2dyYW1BY2NvdW50LzAwMDAwMTAZWKD8YjKnx+berhrNjWxCoyy+ABMV1RwYL+5F4LAynC9MRVovQ2xvY2tQcm9ncmFtQWNjb3VudC8wMDAwMDUwGVig/GIyp8fm3q4azY1sQqMsvgATFdUcGC/uReCwMpwAAAAAEAAAACAYKl+hAQAAiMMAAAAAAAAAAAAAAAI=";
}
