//! What the Zone reads from LEZ (RFC "Derivation Rules"): the blocks of the registration
//! program's chain, each with its timestamp, where it stands on Bedrock and the program's
//! events. The source is a LEZ Sequencer's JSON-RPC, the public testnet one or one run next to
//! it; it serves no events, so they are derived from the program's transactions in each block,
//! decoded with LEZ's own `common` crate, into the program's own `rln_layouts::events` types.

use anyhow::{Context as _, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
pub use common::block::{BedrockStatus, BlockHeader};
use common::{block::Block as LezBlock, transaction::LeeTransaction};
use lee::AccountId;
use rln::prelude::{Hasher, PoseidonHash};
pub use rln_layouts::events::{Erased, Extended, Registered, RlnEvent, Slashed};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::reader::{fr_from_bytes_le, fr_to_bytes_le};

/// `MAX_BLOCK_RANGE_LEN` of the LEZ Sequencer: the most blocks one `getBlockRange` returns.
pub const MAX_BLOCK_RANGE_LEN: u64 = 1024;

/// The program's own name of an event (`SELECTOR_NAME`, what its selector hashes).
pub fn selector_name(event: &RlnEvent) -> &'static str {
    match event {
        RlnEvent::Registered(_) => Registered::SELECTOR_NAME,
        RlnEvent::Extended(_) => Extended::SELECTOR_NAME,
        RlnEvent::Slashed(_) => Slashed::SELECTOR_NAME,
        RlnEvent::Erased(_) => Erased::SELECTOR_NAME,
    }
}

/// A LEZ block as a reader sees it: LEZ's own header (id, hashes, the LEZ clock in ms) and
/// Bedrock status, with the registration program's events (its `rln_layouts::events` types,
/// RFC "LEZ Requirements / Events") in transaction order; the events of any other `tree_id`
/// the program serves are dropped.
#[derive(Debug, Clone)]
pub struct Block {
    pub header: BlockHeader,
    pub bedrock_status: BedrockStatus,
    pub events: Vec<RlnEvent>,
    /// Privacy-preserving transactions with a public effect under the registration program.
    /// They carry no instruction and emit no event, so each changed the registry without this
    /// reader seeing what; the ones under other programs are not counted.
    pub private_transaction_count: u32,
}

impl Block {
    pub fn block_id(&self) -> u64 {
        self.header.block_id
    }

    pub fn prev_block_hash(&self) -> [u8; 32] {
        self.header.prev_block_hash.0
    }

    pub fn hash(&self) -> [u8; 32] {
        self.header.hash.0
    }

    pub fn timestamp_ms(&self) -> u64 {
        self.header.timestamp
    }

    pub fn is_finalized(&self) -> bool {
        matches!(self.bedrock_status, BedrockStatus::Finalized)
    }
}

/// The leading blocks that are finalized on Bedrock: what a reader that trusts no closed block
/// applies. Stops at the first other one, so the chain stays contiguous.
pub fn finalized_prefix(mut blocks: Vec<Block>) -> Vec<Block> {
    let end = blocks
        .iter()
        .position(|b| !b.is_finalized())
        .unwrap_or(blocks.len());
    blocks.truncate(end);
    blocks
}

/// The LEZ Sequencer's JSON-RPC (`getLastBlockId`, `getBlockRange`, at most 1024 blocks per
/// call, closed blocks included before finality). The RPC serves no events, so the events
/// are derived from the program's transactions instead: a block holds the transactions it
/// executed, so every `Register` to the program is a `Registered` whose `leaf_index` is the
/// count of earlier ones, every `Slash` a `Slashed` of `poseidon(identity_secret)`, and so on.
pub struct SequencerLez {
    client: reqwest::Client,
    url: String,
    program: AccountId,
    /// The registry followed; the program's transactions for another `tree_id` are dropped.
    tree_id: [u8; 32],
    /// Serve only the blocks finalized on Bedrock: a reader that waits for LEZ finality instead
    /// of trusting closed blocks (the indexer).
    finalized_only: bool,
}

impl SequencerLez {
    pub fn new(
        url: String,
        program_account_id: [u8; 32],
        tree_id: [u8; 32],
        finalized_only: bool,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            url,
            program: AccountId::new(program_account_id),
            tree_id,
            finalized_only,
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
            "LEZ Sequencer {} ({} blocks, events derived from the program's transactions), \
             program {}",
            self.url,
            if self.finalized_only {
                "Bedrock-finalized"
            } else {
                "closed"
            },
            hex::encode(borsh::to_vec(&self.program).expect("an account id serializes"))
        )
    }

    /// `block_id` of the latest closed block.
    pub async fn last_block_id(&self) -> Result<u64> {
        self.rpc("getLastBlockId", json!([])).await
    }

    /// The blocks `from..=to`, in order, with the program's events of each; with
    /// `finalized_only`, the leading ones finalized on Bedrock (fewer than asked while LEZ
    /// finality lags).
    ///
    /// A call asks for up to `MAX_BLOCK_RANGE_LEN` blocks; when the sequencer refuses the answer
    /// as too big (its 10 MiB response cap, code `-32008`, blocks with many transactions)
    /// the range is halved and asked again, down to one block.
    pub async fn blocks(&self, from: u64, to: u64) -> Result<Vec<Block>> {
        let mut blocks = vec![];
        let mut start = from;
        let mut range = MAX_BLOCK_RANGE_LEN;
        while start <= to {
            let end = to.min(start + range - 1);
            let encoded: Vec<String> = match self.rpc("getBlockRange", json!([start, end])).await {
                Ok(encoded) => encoded,
                Err(e) if range > 1 && e.to_string().contains("\"code\":-32008") => {
                    range /= 2;
                    continue;
                }
                Err(e) => return Err(e),
            };
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
        Ok(if self.finalized_only {
            finalized_prefix(blocks)
        } else {
            blocks
        })
    }

    /// LEZ's borsh `Block`, its hash checked, with the program's events of its transactions.
    pub fn decode_block(&self, bytes: &[u8]) -> Result<Block> {
        let block: LezBlock = borsh::from_slice(bytes).context("not a LEZ block")?;
        let block_id = block.header.block_id;
        if block.recompute_hash() != block.header.hash {
            bail!("LEZ block {block_id}: the header hash is not the hash of its contents");
        }
        let mut events = vec![];
        let mut private_transaction_count = 0;
        for (tx_index, tx) in block.body.transactions.iter().enumerate() {
            match tx {
                LeeTransaction::Public(tx) => {
                    if tx.message.program_account_id != self.program {
                        continue;
                    }
                    match self.decode_instruction(&tx.message.instruction_data) {
                        Ok(Some(event)) => events.push(event),
                        Ok(None) => {}
                        // The tree is then wrong until a restart with `--rebuild-tree`.
                        Err(e) => println!(
                            "LEZ block {block_id} tx {tx_index}: transaction to the program \
                             skipped ({} bytes): {e}; instruction {}",
                            tx.message.instruction_data.len(),
                            hex::encode(&tx.message.instruction_data)
                        ),
                    }
                }
                LeeTransaction::PrivacyPreserving(tx) => {
                    let touches_program = tx
                        .message
                        .public_actions
                        .iter()
                        .flat_map(|action| &action.effects)
                        .any(|effect| effect.program_account_id == self.program);
                    private_transaction_count += u32::from(touches_program);
                }
            }
        }
        Ok(Block {
            header: block.header,
            bedrock_status: block.bedrock_status,
            events,
            private_transaction_count,
        })
    }

    /// Decode the program's `Instruction` with the program's own layouts crate and build
    /// the event the program emits for it, in its `events` module's types (`RlnEvent`):
    /// `Registered` with the expiry at the end of the grace period (`now + active + grace`),
    /// `Extended` with the expiry the instruction claims, `Slashed` (`poseidon(identity_secret)`
    /// must be the `id_commitment`), `Erased`; `Initialize` emits none. Every value a handler needs
    /// travels in the instruction as a claim the chain asserts. An instruction for another
    /// registry (`tree_id`) is dropped.
    fn decode_instruction(&self, data: &[u8]) -> Result<Option<RlnEvent>> {
        use rln_layouts::{Instruction, expiry_ms, secs_to_millis};
        let instruction: Instruction =
            borsh::from_slice(data).context("not an instruction of the program")?;
        let tree_id = match &instruction {
            Instruction::Initialize { tree_id, .. }
            | Instruction::Register { tree_id, .. }
            | Instruction::Slash { tree_id, .. }
            | Instruction::Extend { tree_id, .. }
            | Instruction::Erase { tree_id, .. } => *tree_id,
        };
        if tree_id != self.tree_id {
            return Ok(None);
        }
        Ok(Some(match instruction {
            Instruction::Initialize { .. } => return Ok(None),
            Instruction::Register {
                id_commitment,
                rate_limit,
                now_ms,
                active_duration_sec,
                grace_period_duration_sec,
                leaf_index,
                ..
            } => {
                let grace_start_ms = now_ms.saturating_add(secs_to_millis(active_duration_sec));
                RlnEvent::Registered(Registered {
                    tree_id,
                    id_commitment,
                    rate_limit,
                    expiry_timestamp_ms: expiry_ms(
                        grace_start_ms,
                        secs_to_millis(grace_period_duration_sec),
                    ),
                    leaf_index,
                })
            }
            Instruction::Slash {
                id_commitment,
                identity_secret,
                ..
            } => {
                let secret = fr_from_bytes_le(&identity_secret)
                    .map_err(|e| anyhow!("identity_secret: {e}"))?;
                let derived = fr_to_bytes_le(&Hasher::<PoseidonHash>::hash_single(secret));
                if derived != id_commitment {
                    bail!(
                        "Slash: poseidon(identity_secret) {} is not the id_commitment {}",
                        hex::encode(derived),
                        hex::encode(id_commitment)
                    );
                }
                RlnEvent::Slashed(Slashed {
                    tree_id,
                    id_commitment,
                })
            }
            Instruction::Extend {
                id_commitment,
                expiry_timestamp_ms,
                ..
            } => RlnEvent::Extended(Extended {
                tree_id,
                id_commitment,
                expiry_timestamp_ms,
            }),
            Instruction::Erase { id_commitment, .. } => RlnEvent::Erased(Erased {
                tree_id,
                id_commitment,
            }),
        }))
    }
}

#[cfg(test)]
mod test {
    use common::{
        HashType,
        block::{BlockBody, HashableBlockData},
    };

    use super::*;

    const PROGRAM: [u8; 32] = [1; 32];
    const TREE: [u8; 32] = [7; 32];

    fn lez() -> SequencerLez {
        SequencerLez::new("http://unused".into(), PROGRAM, TREE, false)
    }

    /// A LEZ block in its own borsh, the hash computed as LEZ does; the transactions are
    /// hand-assembled borsh of `LeeTransaction` (a public one is `0 | program [32] |
    /// shard_selectors | nonces | instruction_data | fee | witness_set`).
    fn lez_block(block_id: u64, timestamp: u64, txs: Vec<Vec<u8>>, status: u8) -> Vec<u8> {
        let transactions: Vec<LeeTransaction> = txs
            .iter()
            .map(|bytes| borsh::from_slice(bytes).expect("a LeeTransaction"))
            .collect();
        let data = HashableBlockData {
            block_id,
            prev_block_hash: HashType([0; 32]),
            timestamp,
            transactions: transactions.clone(),
        };
        let hash = data.compute_hash();
        let block = LezBlock {
            header: BlockHeader {
                block_id,
                prev_block_hash: HashType([0; 32]),
                hash,
                timestamp,
            },
            body: BlockBody { transactions },
            bedrock_status: match status {
                0 => BedrockStatus::Pending,
                1 => BedrockStatus::Safe,
                _ => BedrockStatus::Finalized,
            },
        };
        borsh::to_vec(&block).unwrap()
    }

    fn public_tx(to: [u8; 32], data: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8];
        b.extend(to);
        b.extend(0u32.to_le_bytes());
        b.extend(1u32.to_le_bytes());
        b.extend(1u128.to_le_bytes());
        b.extend((data.len() as u32).to_le_bytes());
        b.extend(data);
        b.push(0);
        b.extend(1u32.to_le_bytes());
        b.extend([3u8; 64]);
        b.extend(witness_key());
        b
    }

    /// A real ed25519 public key: LEE refuses a witness key that is not a valid point.
    fn witness_key() -> [u8; 32] {
        lb_key_management_system_service::keys::Ed25519Key::from_bytes(&[7u8; 32])
            .public_key()
            .to_bytes()
    }

    fn bytes(n: u32, fill: u8) -> Vec<u8> {
        let mut b = n.to_le_bytes().to_vec();
        b.extend(vec![fill; n as usize]);
        b
    }

    /// A privacy-preserving transaction whose one public effect is under `effect_program`.
    fn private_tx(effect_program: [u8; 32]) -> Vec<u8> {
        let mut private = vec![1u8];
        // Message.public_actions: one action with one effect
        private.extend(1u32.to_le_bytes());
        private.extend([10u8; 32]);
        private.extend(1u32.to_le_bytes());
        private.extend(effect_program);
        private.extend([12u8; 32]);
        private.extend(bytes(3, 13));
        // Message.nonces: one
        private.extend(1u32.to_le_bytes());
        private.extend(9u128.to_le_bytes());
        // Message.private_actions: one
        private.extend(1u32.to_le_bytes());
        private.extend([14u8; 96]);
        private.extend(bytes(5, 15));
        private.extend(bytes(33, 16));
        private.push(17);
        // Message.block_validity_window {Some(7), None}, timestamp_validity_window {None, Some(8)}
        private.push(1);
        private.extend(7u64.to_le_bytes());
        private.push(0);
        private.push(0);
        private.push(1);
        private.extend(8u64.to_le_bytes());
        // Message.program_image_claims: Disclosed then Undisclosed
        private.extend(2u32.to_le_bytes());
        private.push(0);
        private.extend([18u8; 64]);
        private.push(1);
        private.extend([19u8; 32]);
        // WitnessSet: one signature + key, then the proof
        private.extend(1u32.to_le_bytes());
        private.extend([20u8; 64]);
        private.extend(witness_key());
        private.extend(bytes(4, 21));
        private
    }

    fn register(leaf_index: u64, id: [u8; 32]) -> Vec<u8> {
        borsh::to_vec(&rln_layouts::Instruction::Register {
            tree_id: TREE,
            id_commitment: id,
            rate_limit: 100,
            now_ms: 5_000,
            price_per_unit: 10_000,
            active_duration_sec: 2,
            grace_period_duration_sec: 1,
            leaf_index,
        })
        .unwrap()
    }

    #[test]
    fn test_decode_every_instruction() {
        use rln_layouts::Instruction;
        let lez = lez();
        let keys = rln::prelude::IdentityKeys::generate::<PoseidonHash, rand::rngs::ThreadRng>(
            &mut rand::thread_rng(),
        );
        let id = fr_to_bytes_le(&keys.id_commitment());
        let first = lez.decode_instruction(&register(0, id)).unwrap().unwrap();
        let second = lez.decode_instruction(&register(1, id)).unwrap().unwrap();
        assert!(matches!(
            first,
            RlnEvent::Registered(Registered {
                tree_id: TREE,
                leaf_index: 0,
                rate_limit: 100,
                // now 5000 + active 2 s + grace 1 s
                expiry_timestamp_ms: 8_000,
                ..
            })
        ));
        assert!(matches!(
            second,
            RlnEvent::Registered(Registered { leaf_index: 1, .. })
        ));
        let init = borsh::to_vec(&Instruction::Initialize {
            tree_id: TREE,
            price_per_unit: 0,
            treasury_account_id: [0; 32],
            max_total_rate_limit: 0,
            active_duration_for_new_memberships_sec: 0,
            grace_period_duration_for_new_memberships_sec: 0,
        })
        .unwrap();
        assert_eq!(lez.decode_instruction(&init).unwrap(), None);
        let slash = borsh::to_vec(&Instruction::Slash {
            tree_id: TREE,
            id_commitment: id,
            identity_secret: fr_to_bytes_le(&keys.identity_secret()),
            rate_limit: 100,
        })
        .unwrap();
        assert_eq!(
            lez.decode_instruction(&slash).unwrap(),
            Some(RlnEvent::Slashed(Slashed {
                tree_id: TREE,
                id_commitment: id
            }))
        );
        let wrong_secret = borsh::to_vec(&Instruction::Slash {
            tree_id: TREE,
            id_commitment: id,
            identity_secret: fr_to_bytes_le(&rln::prelude::Fr::from(3u64)),
            rate_limit: 100,
        })
        .unwrap();
        assert!(lez.decode_instruction(&wrong_secret).is_err());
        let extend = borsh::to_vec(&Instruction::Extend {
            tree_id: TREE,
            id_commitment: [8; 32],
            now_ms: 5_000,
            price_per_unit: 10_000,
            rate_limit: 100,
            expiry_timestamp_ms: 9_000,
        })
        .unwrap();
        assert_eq!(
            lez.decode_instruction(&extend).unwrap(),
            Some(RlnEvent::Extended(Extended {
                tree_id: TREE,
                id_commitment: [8; 32],
                expiry_timestamp_ms: 9_000,
            }))
        );
        let erase = borsh::to_vec(&Instruction::Erase {
            tree_id: TREE,
            id_commitment: [8; 32],
            rate_limit: 100,
            now_ms: 5_000,
        })
        .unwrap();
        assert_eq!(
            lez.decode_instruction(&erase).unwrap(),
            Some(RlnEvent::Erased(Erased {
                tree_id: TREE,
                id_commitment: [8; 32],
            }))
        );
        // Another registry of the same program: no event.
        let other = borsh::to_vec(&Instruction::Erase {
            tree_id: [9; 32],
            id_commitment: [8; 32],
            rate_limit: 100,
            now_ms: 5_000,
        })
        .unwrap();
        assert_eq!(lez.decode_instruction(&other).unwrap(), None);
    }

    /// A block with a transaction to another program, a privacy-preserving one under another
    /// program, one under ours (counted) and a `Register` to ours (an event); an undecodable
    /// instruction to ours is skipped and the block still reads.
    #[test]
    fn test_decode_block_derives_events_counts_private_transactions_and_checks_the_hash() {
        let lez = lez();
        let mut erase_with_a_tag_not_known = vec![9u8];
        erase_with_a_tag_not_known.extend([7u8; 32]);
        erase_with_a_tag_not_known.extend([8u8; 32]);
        let bytes = lez_block(
            43,
            2_345,
            vec![
                public_tx([4u8; 32], b"other program"),
                private_tx([11u8; 32]),
                private_tx(PROGRAM),
                public_tx(PROGRAM, &erase_with_a_tag_not_known),
                public_tx(
                    PROGRAM,
                    &register(0, fr_to_bytes_le(&rln::prelude::Fr::from(5u64))),
                ),
            ],
            0,
        );
        let block = lez.decode_block(&bytes).unwrap();
        assert_eq!(block.block_id(), 43);
        assert_eq!(block.timestamp_ms(), 2_345);
        assert!(matches!(block.bedrock_status, BedrockStatus::Pending));
        assert_eq!(block.private_transaction_count, 1);
        assert_eq!(block.events.len(), 1);
        assert!(matches!(
            block.events[0],
            RlnEvent::Registered(Registered {
                leaf_index: 0,
                rate_limit: 100,
                ..
            })
        ));
        // A block whose header hash is not its contents' is refused.
        let mut forged = bytes.clone();
        forged[8 + 32] ^= 1;
        assert!(lez.decode_block(&forged).is_err());
        // A reader waiting for LEZ finality takes the finalized blocks in front, no more.
        let final_block = |id: u64| lez.decode_block(&lez_block(id, 1, vec![], 2)).unwrap();
        let prefix = finalized_prefix(vec![final_block(1), final_block(2), block.clone()]);
        assert_eq!(
            prefix.iter().map(Block::block_id).collect::<Vec<_>>(),
            [1, 2]
        );
        assert!(finalized_prefix(vec![block, final_block(1)]).is_empty());
    }

    /// LEZ testnet block 50056 as `getBlockRange` returns it: three public transactions, the
    /// second a `Register` to the program (leaf 0); finalized on Bedrock.
    #[test]
    fn test_decode_real_testnet_block_50056() {
        let program = <[u8; 32]>::try_from(
            hex::decode("f04803944507677bb79b27b61cd0ad6eea81928f281570347e2df6249495106a")
                .unwrap(),
        )
        .unwrap();
        let tree_id = <[u8; 32]>::try_from(
            hex::decode("47b2de4f08080e74e2d4beffe8e5e87e709dd598619c9b322b64f2756979b4cf")
                .unwrap(),
        )
        .unwrap();
        let lez = SequencerLez::new("http://unused".into(), program, tree_id, false);
        let bytes = BASE64.decode(BLOCK_50056).unwrap();
        let block = lez.decode_block(&bytes).unwrap();
        assert_eq!(block.block_id(), 50056);
        assert_eq!(block.timestamp_ms(), 1_792_597_956_640);
        assert!(block.is_finalized());
        assert_eq!(block.events.len(), 1);
        let RlnEvent::Registered(Registered {
            tree_id: event_tree_id,
            id_commitment,
            rate_limit,
            expiry_timestamp_ms,
            leaf_index,
        }) = &block.events[0]
        else {
            panic!("expected Registered");
        };
        assert_eq!(*event_tree_id, tree_id);
        assert!(hex::encode(id_commitment).starts_with("a878f8b554fabd"));
        assert_eq!(*rate_limit, 100);
        // now_ms 1792597876639 + active 30 days + grace 7 days.
        assert_eq!(
            *expiry_timestamp_ms,
            1_792_597_876_639 + 2_592_000_000 + 604_800_000
        );
        assert_eq!(*leaf_index, 0);
    }

    const BLOCK_50056: &str = "iMMAAAAAAABZhMkDiBFQefGZ7yzYj0cPBeEsaLOs++4lMW1TJiFl+qByf/Q+2TfS0jWzHtIGqjiBU+rjIipAwqGe5a/WQW8iIBgqX6EBAAADAAAAAPBIA5RFB2d7t5snthzQrW7qgZKPKBVwNH4t9iSUlRBqBQAAAKDd99hEF9cqCOvklZ2NQgjRxFPjzUQx7fRvD4BhkbCu8EgDlEUHZ3u3mye2HNCtbuqBko8oFXA0fi32JJSVEGrqYNmkMWEGw7bOAPerVg97ZjuPL1ygw6TubO4Lpg25nwAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAalJnme5xzjOU5XpgOaQuUf1FXvZU2aumquwEo+HVks0AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAC9MRVovQ2xvY2tQcm9ncmFtQWNjb3VudC8wMDAwMDUwGVig/GIyp8fm3q4azY1sQqMsvgATFdUcGC/uReCwMpxgqS3Ugdrc0Z4maEa/C/exlYedBgaBAe6KBfUhcJKmOPBIA5RFB2d7t5snthzQrW7qgZKPKBVwNH4t9iSUlRBqAQAAAAAAAAAAAAAAAAAAAAAAAABxAAAAAUey3k8ICA504tS+/+jl6H5wndWYYZybMitk8nVpebTPqHj4tVT6ve3OcgoYXEkrENG4MAIo04TaTIAuLSAn+yZkAAAAAAAAAJ/fKF+hAQAAECcAAAAAAAAAAAAAAAAAAACNJwCAOgkAAAAAAAAAAAAB6mDZpDFhBsO2zgD3q1YPe2Y7jy9coMOk7mzuC6YNuZ+AhB4AAAAAAAAAAAAAAAAAAMgCCAAAAAAAAAAAAAAAAAEAAAB/KBMTcIgm6gU+LFxOhbl3QeN2Sv6l+TcLoyuNqgSjN0hncnoHxrioqFe7umL+bG3LZE4tOBv7C6yavnZatJN5/DsPyPnWwuZyNyGyUKqM5Y9Qi45aDJUmXNmXM4+0r5EAFb2gtaXWMpX0kzR2aj0aes3op7YaztKtfjNMVXNEPfIEAAAAWGJBEKsBRrEh46GSS5a1QFI1VuJL1cfAflKr+l/rr4sVvaC1pdYylfSTNHZqPRp6zeinthrO0q1+M0xVc0Q98n8aXp3JgWudVUWv3G1IexPUosaP4ro7HRU2ALOVpbjEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAByJOq24C+LN9hBsWO+uLAUcKQuc4dezM/Nchgr4bE7XAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAasoMxKiRfFSYI+7p9qPiW4yVFVRIFBRvlZdpP2a6uY4AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABBAAAAAAPNAAAAAAAAkwIAAAAAAACwfAYAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAHIhAAAAAAAAAAAAAAAAAAAAAAAAAAAZWKD8YjKnx+berhrNjWxCoyy+ABMV1RwYL+5F4LAynAMAAAAvTEVaL0Nsb2NrUHJvZ3JhbUFjY291bnQvMDAwMDAwMRlYoPxiMqfH5t6uGs2NbEKjLL4AExXVHBgv7kXgsDKcL0xFWi9DbG9ja1Byb2dyYW1BY2NvdW50LzAwMDAwMTAZWKD8YjKnx+berhrNjWxCoyy+ABMV1RwYL+5F4LAynC9MRVovQ2xvY2tQcm9ncmFtQWNjb3VudC8wMDAwMDUwGVig/GIyp8fm3q4azY1sQqMsvgATFdUcGC/uReCwMpwAAAAAEAAAACAYKl+hAQAAiMMAAAAAAAAAAAAAAAI=";
}
