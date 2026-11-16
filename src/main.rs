//! The RLN Membership Zone node (`docs/rfc-rln-membership-zone.md`), on the Logos testnet:
//! one binary, two modes.
//!
//! Both follow two channels on Bedrock through the Zone SDK, each kept as a final tier plus a
//! view after LEZ's `chain_state`: the LEZ channel (`lez_chain.rs`), whose inscriptions are the
//! LEZ blocks the tree is derived from (`lez.rs`, `reader.rs`), and the Zone channel
//! (`zone_chain.rs`), whose inscriptions are the batches (`batch.rs`). The sequencer publishes
//! one batch per change or heartbeat over the LEZ blocks adopted on Bedrock; the indexer
//! publishes nothing and judges every batch against the tree of the LEZ blocks finalized on
//! Bedrock. Both serve the registry over HTTP (`http.rs`).
//!
//! Everything a node must keep lives in `--data-dir`: `key.hex` (the sequencer's accredited
//! key; the channel id is `sha256(CHANNEL_NAME)`; an indexer has none, it signs nothing) and
//! `tree/`, a sled store holding the tree of the finalized LEZ blocks, the
//! reader's state, the Zone channel state and both Zone SDK checkpoints.

mod batch;
mod http;
mod lez;
mod lez_chain;
mod reader;
mod registry;
mod zone_chain;

use std::{
    fs,
    path::PathBuf,
    sync::{Arc, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, anyhow, bail};
use clap::{Parser, ValueEnum};
use lb_core::mantle::{
    gas::GasCost,
    ops::channel::{ChannelId, MsgId, inscribe::Inscription},
};
use lb_key_management_system_service::keys::{ED25519_SECRET_KEY_SIZE, Ed25519Key, ZkPublicKey};
use lb_zone_sdk::{
    CommonHttpClient, Slot,
    adapter::{Node as _, NodeHttpClient},
    sequencer::{
        ChannelUpdate, Error as SeqError, Event, FinalizedOp, FinalizedTx, FundingConfig,
        InscriptionInfo, SequencerCheckpoint, SequencerConfig, ZoneSequencer, channel_inscriptions,
    },
};
use prost::Message as _;
use rand::{RngCore, thread_rng};
use reqwest::Url;
use sha2::{Digest as _, Sha256};

use crate::{
    batch::Batch,
    http::{AppState, Shared},
    lez::{Block, LezSequencer},
    registry::Registry,
    zone_chain::{AcceptOutcome, ChannelEntry, PublishRecord},
};

/// Seconds between two finality scans (`reconcile_finality`).
const FINALITY_SCAN_SEC: u64 = 300;
/// L1 slots per `immutable_blocks` page of the finality scan.
const FINALITY_SCAN_PAGE: u64 = 100;

/// The registration program on the testnet LEZ: its account id
/// (`HAxY9SiqUjJQHdtvazFDWFCxn1Jw1dtbUGio8DMC4bBw` in base58).
const PROGRAM_ACCOUNT_ID_HEX: &str =
    "f04803944507677bb79b27b61cd0ad6eea81928f281570347e2df6249495106a";
/// The registry (`tree_id`) the program serves, the first field of every event.
const TREE_ID_HEX: &str = "47b2de4f08080e74e2d4beffe8e5e87e709dd598619c9b322b64f2756979b4cf";
/// LEZ block the program was deployed in: no event comes before it.
const DEFAULT_FIRST_LEZ_BLOCK_ID: u64 = 49_431;
/// The LEZ channel on Bedrock (one inscription per LEZ block) on the testnet.
const LEZ_CHANNEL_ID_HEX: &str = "0101010101010101010101010101010101010101010101010101010101010101";
/// Pages of `FINALITY_SCAN_PAGE` finalized L1 slots read backwards from the LIB to find the
/// newest LEZ inscription (LEZ inscribes a block every minute or two).
const LEZ_INSCRIPTION_SCAN_PAGES: u64 = 60;

/// What this binary is: the one sequencer of the Zone, or an indexer that trusts nobody.
#[derive(Clone, Copy, Debug, PartialEq, ValueEnum)]
enum Mode {
    /// Publish batches over the LEZ blocks adopted on Bedrock, serve the API.
    Sequencer,
    /// Judge every batch of the channel against the tree of the LEZ blocks finalized on
    /// Bedrock, serve the API. No accredited key, no funds, no publishing.
    Indexer,
}

#[derive(Parser, Debug)]
#[command(about = "RLN Membership Zone node: sequencer or indexer, with an HTTP registry API")]
struct Args {
    /// `sequencer` publishes the batches, `indexer` reads and judges them.
    #[arg(long, value_enum, default_value_t = Mode::Sequencer, env = "MODE")]
    mode: Mode,
    /// Logos Blockchain node HTTP API (no auth, local).
    #[arg(long, default_value = "http://127.0.0.1:8080", env = "NODE_URL")]
    node_url: String,
    /// Node wallet key paying the Mantle Transaction fees (64 hex); the sequencer needs it.
    #[arg(long, env = "FUNDING_PK")]
    funding_pk: Option<String>,
    /// Hard cap per publish, in atomic units.
    #[arg(long, default_value_t = 1_000_000, env = "MAX_TX_FEE")]
    max_tx_fee: u64,
    /// Reserve over the mandatory fee, in percent; the excess becomes the tip that ranks the
    /// transaction in a congested mempool. The Zone SDK default is 12.
    #[arg(long, default_value_t = 12, env = "PRIORITY_FEE_PERCENT")]
    priority_fee_percent: u64,
    /// Key and tree store live here.
    #[arg(long, default_value = "data", env = "DATA_DIR")]
    data_dir: PathBuf,
    /// HTTP API bind address.
    #[arg(long, default_value = "127.0.0.1:3000", env = "HTTP_ADDR")]
    http_addr: String,
    /// Name of the Zone channel: its id is `sha256(name)`.
    #[arg(
        long,
        default_value = "rln-membership-zone:testnet:v0.3.2-1",
        env = "CHANNEL_NAME"
    )]
    channel_name: String,
    /// Zone channel id (64 hex), overriding `--channel-name`.
    #[arg(long, env = "CHANNEL_ID")]
    channel_id: Option<String>,
    /// Registry namespace in the program (`tree_id`, 64 hex).
    #[arg(long, default_value = TREE_ID_HEX, env = "TREE_ID")]
    tree_id: String,
    /// Registration program account id (64 hex) whose events build the tree.
    #[arg(long, default_value = PROGRAM_ACCOUNT_ID_HEX, env = "PROGRAM_ACCOUNT_ID")]
    program_account_id: String,
    /// The LEZ channel on Bedrock (64 hex): the LEZ blocks come from its inscriptions, through
    /// the Zone SDK.
    #[arg(long, default_value = LEZ_CHANNEL_ID_HEX, env = "LEZ_CHANNEL_ID")]
    lez_channel_id: String,
    /// LEZ Sequencer JSON-RPC that fills the finalized LEZ history fast (pinned by hash to the
    /// blocks Bedrock finalized): the public testnet one, or `http://127.0.0.1:3040` for a LEZ
    /// sequencer run next to it (`lez-sequencer/`).
    #[arg(
        long,
        default_value = "https://testnet.lez.logos.co",
        env = "LEZ_SEQUENCER_URL"
    )]
    lez_sequencer_url: String,
    /// First LEZ block the registry covers: the block the program was deployed in.
    #[arg(long, default_value_t = DEFAULT_FIRST_LEZ_BLOCK_ID, env = "FIRST_LEZ_BLOCK_ID")]
    first_lez_block_id: u64,
    /// Drop the tree and read it again from LEZ, keeping the Zone channel state and its Zone
    /// SDK checkpoint: after a decoder change that skipped transactions.
    #[arg(long, env = "REBUILD_TREE")]
    rebuild_tree: bool,
    /// Seconds between publish checks (the sequencer) or judgments (the indexer).
    #[arg(long, default_value_t = 10, env = "POLL_SEC")]
    poll_sec: u64,
    /// Publish an empty batch after this long without one.
    #[arg(long, default_value_t = 3_600, env = "HEARTBEAT_SEC")]
    heartbeat_sec: u64,
    /// LIB slots a published transaction may stay unmined before the Zone SDK re-funds it
    /// under a new transaction hash (same message id); 0 disables. Off by default: the
    /// mempool is FIFO with a 9 h TTL, and a re-funded transaction goes to the back of the
    /// queue, while a resubmit of the same transaction keeps its place.
    #[arg(long, default_value_t = 0, env = "STALE_REFUND_SLOTS")]
    stale_refund_slots: u64,
}

fn parse_funding_pk(hex_str: &str) -> Result<ZkPublicKey> {
    let trimmed = hex_str.trim().trim_start_matches("0x");
    for candidate in [trimmed.to_owned(), format!("0x{trimmed}")] {
        if let Ok(pk) = serde_json::from_value::<ZkPublicKey>(serde_json::Value::String(candidate))
        {
            return Ok(pk);
        }
    }
    Err(anyhow!(
        "FUNDING_PK is not a valid field-element hex string"
    ))
}

fn parse_32(hex_str: &str, what: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(hex_str.trim().trim_start_matches("0x"))
        .with_context(|| format!("{what} is not hex"))?;
    <[u8; 32]>::try_from(bytes).map_err(|_| anyhow!("{what} must be 32 bytes"))
}

/// A key that signs nothing: the Zone SDK wants one per instance, a reader of a channel it is
/// not accredited on never uses it. Fresh on every start, never written.
fn reader_key() -> Ed25519Key {
    let mut key_bytes = [0u8; ED25519_SECRET_KEY_SIZE];
    thread_rng().fill_bytes(&mut key_bytes);
    Ed25519Key::from_bytes(&key_bytes)
}

/// The sequencer's accredited key: created on first start, read back after.
fn load_or_create_key(path: &PathBuf) -> Result<Ed25519Key> {
    let mut key_bytes = [0u8; ED25519_SECRET_KEY_SIZE];
    if path.exists() {
        let text = fs::read_to_string(path)?;
        key_bytes = parse_32(&text, "key.hex")?;
    } else {
        thread_rng().fill_bytes(&mut key_bytes);
        fs::write(path, hex::encode(key_bytes))?;
        println!("new key written to {}", path.display());
    }
    Ok(Ed25519Key::from_bytes(&key_bytes))
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn short(msg: &[u8; 32]) -> String {
    hex::encode(&msg[..4])
}

/// A channel entry from an inscription the SDK reports.
fn entry_of(info: &InscriptionInfo) -> ChannelEntry {
    ChannelEntry::new(info.this_msg, info.parent_msg, &info.payload)
}

/// Keep the Zone SDK checkpoint with the tree store.
fn save_checkpoint(registry: &Shared, checkpoint: &SequencerCheckpoint) -> Result<()> {
    let mut registry = registry.write().expect("registry lock");
    registry.set_checkpoint(serde_json::to_value(checkpoint)?);
    registry.save()
}

/// The Zone channel and the node to ask about it.
struct ZoneChannel {
    node_url: Url,
    id: ChannelId,
}

/// What the node holds for a channel (`GET /channel/:id`), the fields this check reads.
#[derive(serde::Deserialize)]
struct ChannelOnChain {
    accredited_keys: Vec<String>,
    tip_message: String,
}

/// The channel as the node knows it (`GET /channel/:id`): `None` while no message of it is on
/// chain (the first inscription creates it).
async fn channel_on_chain(node_url: &Url, channel_id: ChannelId) -> Result<Option<ChannelOnChain>> {
    let url = node_url.join(&format!("channel/{}", hex::encode(channel_id.as_ref())))?;
    let response = reqwest::get(url.clone())
        .await
        .with_context(|| format!("GET {url}"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    response
        .error_for_status()
        .with_context(|| format!("GET {url}"))?
        .json()
        .await
        .map(Some)
        .with_context(|| format!("GET {url}: unexpected body"))
}

/// The Zone SDK checkpoint for a channel with no message on chain yet: nothing to backfill, so
/// the SDK starts at the node's LIB instead of walking finalized history from genesis (30 to
/// 60 minutes on the testnet, hours when the node is slow). Every field is what a cold start
/// computes for an empty channel: root tip, no pending transaction, no channel note, root
/// config lineage.
async fn checkpoint_at_lib(node: &NodeHttpClient) -> Result<SequencerCheckpoint> {
    let info = node
        .consensus_info()
        .await
        .map_err(|e| anyhow!("consensus info: {e}"))?
        .cryptarchia_info;
    Ok(SequencerCheckpoint {
        last_msg_id: MsgId::root(),
        pending_txs: vec![],
        lib: info.lib,
        lib_slot: info.lib_slot,
        channel_notes: vec![],
        finalized_config: MsgId::root(),
        funding: vec![],
    })
}

/// Refuse to start a sequencer on a channel this data dir cannot continue. The Zone SDK learns
/// a channel's tip from finalized history only, so without a checkpoint it chains the next
/// batch on the root; on a channel that already has messages that batch is invalid
/// (`InvalidParent`) and vanishes without a trace. Likewise another key cannot write at all.
fn check_channel_on_chain(
    channel: &ChannelOnChain,
    our_key: &[u8; 32],
    registry: &Shared,
    resumed: bool,
) -> Result<()> {
    let ours = hex::encode(our_key);
    if !channel
        .accredited_keys
        .iter()
        .any(|k| k.eq_ignore_ascii_case(&ours))
    {
        bail!(
            "channel is on chain with accredited keys {:?}, not ours ({ours}): this key cannot \
             write to it; restore the original key.hex or pick another CHANNEL_NAME",
            channel.accredited_keys
        );
    }
    let newest_msg = registry
        .read()
        .expect("registry lock")
        .entries()
        .last()
        .map(|e| e.msg_id.clone());
    let tip = channel.tip_message.to_ascii_lowercase();
    match newest_msg {
        Some(msg) if msg == tip && resumed => {
            println!(
                "  chain     channel tip {}.. is our newest batch",
                &tip[..8]
            );
            Ok(())
        }
        Some(msg) if msg == tip => bail!(
            "channel tip {}.. is our newest batch but the data dir holds no Zone SDK checkpoint: \
             the SDK would chain on the root and every batch would be invalid; restore the data \
             dir or pick another CHANNEL_NAME",
            &tip[..8]
        ),
        Some(msg) => bail!(
            "channel tip on chain is {}.., our newest batch is {}..: the Zone channel state does \
             not match the channel (a batch of ours may still be in flight, or data from another \
             run was lost); wait, or pick another CHANNEL_NAME",
            &tip[..8],
            &msg[..8]
        ),
        None => bail!(
            "channel is on chain (tip {}..) but this data dir holds no batch: data from another \
             run was lost; restore it or pick another CHANNEL_NAME",
            &tip[..8]
        ),
    }
}

/// Keep the Zone SDK checkpoint on the LEZ channel with the tree store.
fn save_lez_checkpoint(registry: &Shared, checkpoint: &SequencerCheckpoint) -> Result<()> {
    let mut registry = registry.write().expect("registry lock");
    registry.set_lez_checkpoint(serde_json::to_value(checkpoint)?);
    registry.save()
}

/// The checkpoint the LEZ channel's reader starts from on a fresh store: at the node's LIB,
/// on the newest LEZ inscription at or below it (found by reading the finalized L1 blocks
/// backwards from the LIB, a page at a time), so the SDK walks nothing before. Returns that
/// inscription's LEZ block too: the finalized tree is built up to it.
async fn lez_checkpoint_at_lib(
    node: &NodeHttpClient,
    lez_channel_id: ChannelId,
    source: &LezSequencer,
) -> Result<(SequencerCheckpoint, Block)> {
    let info = node
        .consensus_info()
        .await
        .map_err(|e| anyhow!("consensus info: {e}"))?
        .cryptarchia_info;
    let lib_slot = u64::from(info.lib_slot);
    let mut page_to = lib_slot;
    for _ in 0..LEZ_INSCRIPTION_SCAN_PAGES {
        let page_from = page_to.saturating_sub(FINALITY_SCAN_PAGE - 1);
        let blocks = node
            .immutable_blocks(Slot::from(page_from), Slot::from(page_to))
            .await
            .map_err(|e| anyhow!("immutable blocks {page_from}..{page_to}: {e}"))?;
        let newest = blocks
            .iter()
            .rev()
            .flat_map(|block| block.transactions.iter().rev())
            .flat_map(|tx| channel_inscriptions(tx, lez_channel_id).into_iter().rev())
            .next();
        if let Some(inscription) = newest {
            let block = source
                .decode_block(&inscription.payload)
                .context("the newest finalized LEZ inscription is not a LEZ block")?;
            return Ok((
                SequencerCheckpoint {
                    last_msg_id: inscription.this_msg,
                    pending_txs: vec![],
                    lib: info.lib,
                    lib_slot: info.lib_slot,
                    channel_notes: vec![],
                    finalized_config: MsgId::root(),
                    funding: vec![],
                },
                block,
            ));
        }
        if page_from == 0 {
            break;
        }
        page_to = page_from - 1;
    }
    bail!(
        "no LEZ inscription in the {} finalized L1 slots below the LIB ({lib_slot}): is \
         LEZ_CHANNEL_ID the LEZ channel?",
        LEZ_INSCRIPTION_SCAN_PAGES * FINALITY_SCAN_PAGE
    )
}

/// Finalized LEZ blocks `from..=to` read from the LEZ Sequencer, applied to the finalized
/// tree. They are trusted only through the hash chain: the caller applies the Bedrock-finalized
/// block after them next, which chains on the last one or is refused.
async fn fill_final_from_rpc(
    source: &LezSequencer,
    registry: &Shared,
    from: u64,
    to: u64,
) -> Result<()> {
    if from > to {
        return Ok(());
    }
    println!(
        "LEZ blocks {from}..{to} read from the LEZ Sequencer under the block Bedrock finalized"
    );
    let blocks = source.blocks(from, to).await?;
    let mut registry = registry.write().expect("registry lock");
    for block in &blocks {
        if let Some(expected) = registry.apply_lez_finalized(block)? {
            bail!(
                "the LEZ Sequencer skipped blocks: expected {expected}, got {}",
                block.block_id()
            );
        }
    }
    registry.save()
}

/// A LEZ block Bedrock finalized: onto the finalized tree, the blocks it skipped filled from
/// the LEZ Sequencer first (pinned by this block's `prev_block_hash`).
async fn apply_lez_finalized(
    source: &LezSequencer,
    registry: &Shared,
    block: &Block,
) -> Result<()> {
    let gap = registry
        .write()
        .expect("registry lock")
        .apply_lez_finalized(block)?;
    if let Some(expected) = gap {
        fill_final_from_rpc(source, registry, expected, block.block_id() - 1).await?;
        if let Some(expected) = registry
            .write()
            .expect("registry lock")
            .apply_lez_finalized(block)?
        {
            bail!(
                "LEZ block {} still does not follow the finalized tree (at {})",
                block.block_id(),
                expected - 1
            );
        }
    }
    Ok(())
}

/// A LEZ block adopted on Bedrock, onto the head. Blocks the SDK skipped before it (a stream
/// it dropped and reopened, entries it does not echo) are read from the LEZ Sequencer first,
/// trusted only through the hash chain: they must chain on the head and the adopted block must
/// chain on the last of them, or nothing is applied.
async fn adopt_lez_block(source: &LezSequencer, registry: &Shared, block: &Block) -> Result<()> {
    let gap = registry
        .write()
        .expect("registry lock")
        .apply_lez_adopted(block)?;
    let Some(expected) = gap else {
        return Ok(());
    };
    let to = block.block_id() - 1;
    println!(
        "LEZ blocks {expected}..{to} read from the LEZ Sequencer under adopted block {}",
        block.block_id()
    );
    let filler = source.blocks(expected, to).await?;
    let mut prev = registry
        .read()
        .expect("registry lock")
        .head()
        .last_block_hash();
    for (id, candidate) in (expected..=to).zip(&filler) {
        if candidate.block_id() != id || prev.is_some_and(|p| candidate.prev_block_hash() != p) {
            bail!(
                "LEZ block {} from the LEZ Sequencer does not chain under adopted block {}",
                candidate.block_id(),
                block.block_id()
            );
        }
        prev = Some(candidate.hash());
    }
    if prev != Some(block.prev_block_hash()) {
        bail!(
            "the LEZ Sequencer's blocks {expected}..{to} do not lead to adopted block {}",
            block.block_id()
        );
    }
    let mut registry = registry.write().expect("registry lock");
    for candidate in &filler {
        if registry.apply_lez_adopted(candidate)?.is_some() {
            bail!("LEZ block {} still missing before", candidate.block_id());
        }
    }
    if registry.apply_lez_adopted(block)?.is_some() {
        bail!(
            "adopted LEZ block {} still does not follow the head",
            block.block_id()
        );
    }
    Ok(())
}

/// One `BlocksProcessed` event of the Zone SDK on the LEZ channel: adopted LEZ blocks onto the
/// head, a Bedrock reorg refolds the head over the canonical lineage, finalized LEZ blocks onto
/// the finalized tree. The events of a block are logged when the head first sees it.
async fn on_lez_blocks_processed(
    source: &LezSequencer,
    registry: &Shared,
    checkpoint: &SequencerCheckpoint,
    channel_update: &ChannelUpdate,
    finalized: &[FinalizedTx],
) -> Result<()> {
    save_lez_checkpoint(registry, checkpoint)?;
    let decode = |info: &InscriptionInfo| match source.decode_block(&info.payload) {
        Ok(block) => Some(block),
        Err(e) => {
            println!(
                "LEZ channel entry msg {} is not a LEZ block: {e}",
                short(&info.this_msg.into())
            );
            None
        }
    };
    match channel_update {
        ChannelUpdate::Extension { adopted } => {
            for block in adopted
                .iter()
                .filter_map(|tx| tx.inscription())
                .filter_map(decode)
            {
                if !block.events.is_empty() {
                    let names: Vec<_> = block.events.iter().map(lez::selector_name).collect();
                    println!(
                        "LEZ block {} adopted: events {}",
                        block.block_id(),
                        names.join(", ")
                    );
                }
                if let Err(e) = adopt_lez_block(source, registry, &block).await {
                    println!("{e}");
                }
            }
        }
        ChannelUpdate::Conflict { .. } => {
            let canonical: Vec<Block> = channel_update
                .canonical_chain()
                .into_iter()
                .flatten()
                .filter_map(|tx| tx.inscription())
                .filter_map(decode)
                .collect();
            registry
                .write()
                .expect("registry lock")
                .apply_lez_conflict()?;
            for block in &canonical {
                if let Err(e) = adopt_lez_block(source, registry, block).await {
                    println!("{e}");
                    break;
                }
            }
            println!(
                "LEZ channel reorg on Bedrock: the head is the finalized tree folded over the \
                 canonical lineage again, at LEZ block {}",
                registry.read().expect("registry lock").head().lez_block_id
            );
        }
    }
    for tx in finalized {
        for op in &tx.ops {
            let FinalizedOp::Inscription(info) = op else {
                continue;
            };
            let Some(block) = decode(info) else { continue };
            apply_lez_finalized(source, registry, &block).await?;
        }
    }
    registry.write().expect("registry lock").save()
}

/// Say what the final tier made of a finalized entry.
fn report_finalized(entry: &ChannelEntry, l1_slot: u64, outcome: Option<&AcceptOutcome>) {
    let msg: [u8; 32] = entry.msg.into();
    let range = entry.batch.as_ref().map_or("no batch".to_string(), |b| {
        format!("[{}..{}]", b.from_lez_block_id, b.to_lez_block_id)
    });
    match outcome {
        Some(AcceptOutcome::Applied(_)) => println!(
            "batch {range} msg {} finalized at L1 slot {l1_slot}: applied, root {}..",
            short(&msg),
            entry
                .batch
                .as_ref()
                .map(|b| hex::encode(&b.root_after[..4]))
                .unwrap_or_default()
        ),
        Some(AcceptOutcome::Parked(e)) => println!(
            "batch {range} msg {} finalized at L1 slot {l1_slot}: parked, {e}; the final tier \
             is frozen here",
            short(&msg)
        ),
        Some(AcceptOutcome::RetryableFailure(e)) => println!(
            "batch {range} msg {} finalized at L1 slot {l1_slot}: {e}",
            short(&msg)
        ),
        Some(AcceptOutcome::AlreadyApplied) => {}
        None => println!(
            "entry msg {} finalized at L1 slot {l1_slot}: {range}",
            short(&msg)
        ),
    }
}

/// The finality the SDK stream may have missed (a restart between the event and the save):
/// read the finalized L1 blocks from the anchor up to the LIB and apply every inscription of
/// the channel found there. Idempotent with the stream path. Returns how many were new.
async fn reconcile_finality(
    node: &NodeHttpClient,
    channel_id: ChannelId,
    registry: &Shared,
) -> Result<usize> {
    let Some(anchor) = registry.read().expect("registry lock").anchor() else {
        return Ok(0);
    };
    let lib = u64::from(
        node.consensus_info()
            .await
            .map_err(|e| anyhow!("consensus info: {e}"))?
            .cryptarchia_info
            .lib_slot,
    );
    let mut applied = 0;
    let mut page_from = anchor.slot + 1;
    while page_from <= lib {
        let page_to = lib.min(page_from + FINALITY_SCAN_PAGE - 1);
        let blocks = node
            .immutable_blocks(Slot::from(page_from), Slot::from(page_to))
            .await
            .map_err(|e| anyhow!("immutable blocks {page_from}..{page_to}: {e}"))?;
        let mut registry = registry.write().expect("registry lock");
        for block in &blocks {
            let l1_slot = u64::from(block.header.slot);
            for tx in &block.transactions {
                for info in channel_inscriptions(tx, channel_id) {
                    let entry = entry_of(&info);
                    let outcome = registry.apply_zone_finalized(entry.clone(), l1_slot);
                    if !matches!(outcome, Some(AcceptOutcome::AlreadyApplied)) {
                        report_finalized(&entry, l1_slot, outcome.as_ref());
                        applied += 1;
                    }
                }
            }
        }
        registry.save()?;
        page_from = page_to + 1;
    }
    Ok(applied)
}

/// One `BlocksProcessed` event of the Zone SDK: the channel tip holds our batches up to `tip`,
/// the view moved (`channel_update`: an extension, or a conflict whose dropped batches of ours
/// are published again) and Bedrock finality reached the `finalized` entries, which go into
/// the final tier. A conflict the node does not confirm (its channel tip is still our newest
/// entry) is ignored: the SDK also reports a pending entry it could not see mined as orphaned,
/// and publishing it again would fork our own channel.
async fn on_blocks_processed(
    sdk: &mut ZoneSequencer<NodeHttpClient>,
    registry: &Shared,
    channel: &ZoneChannel,
    tip: [u8; 32],
    checkpoint: &SequencerCheckpoint,
    channel_update: &ChannelUpdate,
    finalized: &[FinalizedTx],
) -> Result<()> {
    save_checkpoint(registry, checkpoint)?;
    // The channel tip is one of our batches: it and every pending publish before it are in a
    // Bedrock block (an inscription we published is not echoed in `adopted` on a plain chain
    // extension).
    for msg in registry
        .write()
        .expect("registry lock")
        .mark_adopted_up_to(tip)
    {
        println!(
            "batch msg {} adopted in a Bedrock block (channel tip)",
            short(&msg)
        );
    }
    match channel_update {
        ChannelUpdate::Extension { adopted } => {
            let entries: Vec<ChannelEntry> = adopted
                .iter()
                .filter_map(|tx| tx.inscription())
                .map(entry_of)
                .collect();
            for entry in &entries {
                println!(
                    "channel entry msg {} adopted in a Bedrock block",
                    short(&entry.msg.into())
                );
            }
            registry
                .write()
                .expect("registry lock")
                .apply_zone_extension(entries);
        }
        ChannelUpdate::Conflict { .. } => {
            let newest_held: Option<String> = registry
                .read()
                .expect("registry lock")
                .zone
                .view()
                .last()
                .map(|entry| hex::encode(<[u8; 32]>::from(entry.msg)));
            if let Some(on_chain) = channel_on_chain(&channel.node_url, channel.id).await?
                && newest_held.is_some_and(|m| m.eq_ignore_ascii_case(&on_chain.tip_message))
            {
                println!(
                    "the SDK reports a conflict on the channel, but the node's channel tip is \
                     still our newest entry {}..: nothing was dropped, ignored",
                    &on_chain.tip_message[..8]
                );
                return registry.write().expect("registry lock").save();
            }
            let canonical: Vec<ChannelEntry> = channel_update
                .canonical_chain()
                .into_iter()
                .flatten()
                .filter_map(|tx| tx.inscription())
                .map(entry_of)
                .collect();
            let dropped = registry
                .write()
                .expect("registry lock")
                .apply_zone_conflict(canonical);
            // A reorg dropped our batches: publish the same payloads again, in order (new
            // messages, since their parent changed).
            for entry in dropped {
                let Some(batch) = entry.batch else { continue };
                republish(sdk, registry, entry.msg.into(), &batch).await?;
            }
        }
    }
    if let Some(gap) = registry
        .read()
        .expect("registry lock")
        .zone
        .check_for_gaps_in_lineage()
    {
        println!(
            "WARNING: channel entry msg {} chains on {}, which the view does not hold: the \
             lineage has a gap",
            short(&gap.msg.into()),
            short(&gap.parent.into())
        );
    }
    // Bedrock finality reached entries of the channel: into the final tier.
    for tx in finalized {
        let l1_slot = tx.l1_slot.into_inner();
        for op in &tx.ops {
            let FinalizedOp::Inscription(info) = op else {
                continue;
            };
            let entry = entry_of(info);
            let outcome = registry
                .write()
                .expect("registry lock")
                .apply_zone_finalized(entry.clone(), l1_slot);
            report_finalized(&entry, l1_slot, outcome.as_ref());
        }
    }
    registry.write().expect("registry lock").save()
}

/// Publish `batch` again after a reorg orphaned its message `orphaned`: a new message, since
/// its parent changed. A refusal by the SDK is logged and left to the next conflict or poll.
async fn republish(
    sdk: &mut ZoneSequencer<NodeHttpClient>,
    registry: &Shared,
    orphaned: [u8; 32],
    batch: &Batch,
) -> Result<()> {
    let inscription = Inscription::try_from(batch.encode_to_vec())
        .map_err(|e| anyhow!("payload too large: {e:?}"))?;
    match sdk.handle().publish(inscription).await {
        Ok((result, checkpoint)) => {
            let info = result.tx.inscription();
            let record = registry.write().expect("registry lock").record_publish(
                info.this_msg,
                info.parent_msg,
                batch,
            );
            save_checkpoint(registry, &checkpoint)?;
            println!(
                "batch [{}..{}] msg {} orphaned by a reorg, published again as msg {} \
                 ({record:?})",
                batch.from_lez_block_id,
                batch.to_lez_block_id,
                short(&orphaned),
                short(&info.this_msg.into())
            );
        }
        Err(e) => println!("republish after reorg failed: {e}"),
    }
    Ok(())
}

/// One tick. The indexer judges what it could not judge before (the finalized tree moved).
/// The sequencer checks its own statements against the tree, then publishes one batch
/// covering the adopted LEZ blocks not covered yet when changes wait or the heartbeat is due,
/// with at most one batch in flight. `Err` only for the tree store.
async fn on_tick(
    sdk: &mut ZoneSequencer<NodeHttpClient>,
    registry: &Shared,
    args: &Args,
    contradicted_count: &mut usize,
    last_publish_unix: &mut u64,
) -> Result<()> {
    if args.mode == Mode::Indexer {
        let decided = registry.write().expect("registry lock").retry_folds();
        if decided > 0 {
            println!("{decided} batch(es) judged now that LEZ finality reached them");
            registry.write().expect("registry lock").save()?;
        }
        return Ok(());
    }
    warn_contradicted(&registry.read().expect("registry lock"), contradicted_count);
    let batch = {
        let registry = registry.read().expect("registry lock");
        if registry.in_flight().is_some() {
            return Ok(());
        }
        let due = registry.entries().is_empty()
            || !registry.pending_changes.is_empty()
            || now_unix().saturating_sub(*last_publish_unix) >= args.heartbeat_sec;
        if due { registry.next_batch() } else { None }
    };
    let Some(batch) = batch else { return Ok(()) };
    publish_batch(sdk, registry, &batch, last_publish_unix).await
}

/// The sequencer's own batches that LEZ now contradicts (the tree at `to_lez_block_id` has
/// another root than the batch claims), warned once per change of the set.
fn warn_contradicted(registry: &Registry, contradicted_count: &mut usize) {
    let contradicted = registry.contradicted();
    if contradicted.len() == *contradicted_count {
        return;
    }
    *contradicted_count = contradicted.len();
    for e in contradicted {
        let now = e
            .to_lez_block_id
            .and_then(|to| registry.head().root_at(to))
            .map(|r| hex::encode(&r[..4]))
            .unwrap_or_default();
        println!(
            "WARNING: batch msg {} claims root {}.. at LEZ block {}, LEZ now gives {}..: an \
             indexer parks on it and every later batch",
            &e.msg_id[..8],
            &e.root.clone().unwrap_or_default()[..8],
            e.to_lez_block_id.unwrap_or_default(),
            now
        );
    }
}

/// Publish the next batch: recorded as in flight and saved with the checkpoint on success, so
/// a restart knows it; `last_publish_unix` restarts the heartbeat. A batch too large for one
/// inscription or a refusal by the SDK is logged and tried again next poll. A batch that
/// would not apply on our own head is a bug: fatal before anything reaches Bedrock.
async fn publish_batch(
    sdk: &mut ZoneSequencer<NodeHttpClient>,
    registry: &Shared,
    batch: &Batch,
    last_publish_unix: &mut u64,
) -> Result<()> {
    if let Err(e) = registry.read().expect("registry lock").would_apply(batch) {
        bail!(
            "batch [{}..{}] would not apply on our own head ({e}); stopping before publishing \
             it",
            batch.from_lez_block_id,
            batch.to_lez_block_id
        );
    }
    let inscription = match Inscription::try_from(batch.encode_to_vec()) {
        Ok(inscription) => inscription,
        Err(e) => {
            println!(
                "batch [{}..{}] does not fit one inscription: {e:?}",
                batch.from_lez_block_id, batch.to_lez_block_id
            );
            return Ok(());
        }
    };
    match sdk.handle().publish(inscription).await {
        Ok((result, checkpoint)) => {
            let info = result.tx.inscription();
            let record = {
                let mut registry = registry.write().expect("registry lock");
                let record = registry.record_publish(info.this_msg, info.parent_msg, batch);
                registry.save()?;
                record
            };
            save_checkpoint(registry, &checkpoint)?;
            *last_publish_unix = now_unix();
            let described: Vec<_> = batch.changes.iter().map(|c| c.describe()).collect();
            println!(
                "batch [{}..{}] published as msg {} on {}: root {}.., {}{}",
                batch.from_lez_block_id,
                batch.to_lez_block_id,
                short(&info.this_msg.into()),
                short(&info.parent_msg.into()),
                hex::encode(&batch.root_after[..4]),
                if described.is_empty() {
                    "no changes".into()
                } else {
                    described.join("; ")
                },
                if record == PublishRecord::Extended {
                    "".into()
                } else {
                    format!(" ({record:?})")
                }
            );
            // Checked above; a disagreement here means the head moved under the publish.
            if record == PublishRecord::Skipped {
                let error = registry
                    .read()
                    .expect("registry lock")
                    .entries()
                    .last()
                    .and_then(|e| e.error.as_ref().map(ToString::to_string))
                    .unwrap_or_default();
                bail!(
                    "batch msg {} does not apply on our own head ({error}); stopping instead of \
                     publishing it again",
                    short(&info.this_msg.into())
                );
            }
        }
        Err(SeqError::Unavailable { reason }) => {
            println!("not ready to publish ({reason}), retry next poll")
        }
        Err(e) => println!("publish failed: {e}"),
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let args = Args::parse();
    fs::create_dir_all(&args.data_dir)?;
    let indexer = args.mode == Mode::Indexer;

    // The LEZ Sequencer RPC: decodes the LEZ blocks and fills finalized history.
    let program_account_id = parse_32(&args.program_account_id, "PROGRAM_ACCOUNT_ID")?;
    let tree_id = parse_32(&args.tree_id, "TREE_ID")?;
    let source = LezSequencer::new(args.lez_sequencer_url.clone(), program_account_id, tree_id);
    let lez_channel_id = ChannelId::from(parse_32(&args.lez_channel_id, "LEZ_CHANNEL_ID")?);
    let lez_source = format!(
        "LEZ channel {} on Bedrock through the Zone SDK (adopted blocks at the head, finalized \
         ones judged); finalized history from {}",
        hex::encode(lez_channel_id.as_ref()),
        source.describe()
    );

    // The tree store under the data dir, with the Zone channel state and the checkpoint.
    let registry: Shared = Arc::new(RwLock::new(Registry::open(
        &args.data_dir,
        tree_id,
        args.first_lez_block_id,
        args.rebuild_tree,
        indexer,
    )?));

    // The channel id is any 32 bytes nobody has written to yet: the key signing the first
    // inscription becomes its accredited key. A name hashes to a stable, readable choice.
    let channel_id = ChannelId::from(match &args.channel_id {
        Some(id) => parse_32(id, "CHANNEL_ID")?,
        None => Sha256::digest(args.channel_name.as_bytes()).into(),
    });
    let node_url: Url = args.node_url.parse().context("invalid NODE_URL")?;
    let node = NodeHttpClient::new(CommonHttpClient::new(None), node_url.clone());

    // HTTP API, the same in both modes.
    let state = AppState {
        registry: registry.clone(),
        channel_id: hex::encode(channel_id.as_ref()),
        lez_source: lez_source.clone(),
        mode: format!("{:?}", args.mode).to_lowercase(),
    };
    let listener = tokio::net::TcpListener::bind(&args.http_addr)
        .await
        .with_context(|| format!("bind {}", args.http_addr))?;
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, http::router(state)).await {
            eprintln!("http server stopped: {e}");
        }
    });

    // The Zone SDK on the node: the sequencer with its accredited key and the wallet key that
    // pays; the indexer with a throwaway key, since it only reads (it never funds a
    // transaction either, the funding key is a name to the SDK).
    let signing_key = if indexer {
        reader_key()
    } else {
        load_or_create_key(&args.data_dir.join("key.hex"))?
    };
    let funding_pk = match (&args.funding_pk, indexer) {
        (Some(pk), _) => parse_funding_pk(pk)?,
        (None, true) => parse_funding_pk(&"00".repeat(32))?,
        (None, false) => bail!("FUNDING_PK is required to run the sequencer"),
    };
    let funding = FundingConfig {
        funding_pk,
        change_pk: None,
        max_tx_fee: GasCost::new(args.max_tx_fee),
        priority_fee_percent: args.priority_fee_percent,
    };
    let checkpoint: Option<SequencerCheckpoint> = registry
        .read()
        .expect("registry lock")
        .checkpoint()
        .map(|c| serde_json::from_value(c.clone()))
        .transpose()
        .context("Zone SDK checkpoint in the tree store")?;
    let resumed = checkpoint.is_some();
    let channel = channel_on_chain(&node_url, channel_id).await?;
    let checkpoint = match &channel {
        Some(channel) => {
            if !indexer {
                check_channel_on_chain(
                    channel,
                    &signing_key.public_key().to_bytes(),
                    &registry,
                    resumed,
                )?;
            }
            checkpoint
        }
        // No message of the channel on chain: nothing to walk, start at the LIB. A batch of
        // ours still in flight is the one thing a stored checkpoint knows better.
        None if registry
            .read()
            .expect("registry lock")
            .in_flight()
            .is_none() =>
        {
            println!("  chain     channel not on chain yet; the first batch creates it");
            Some(checkpoint_at_lib(&node).await?)
        }
        None => checkpoint,
    };
    let sdk_config = SequencerConfig {
        stale_refund_slots: args.stale_refund_slots,
        ..SequencerConfig::new(funding)
    };
    let mut sdk = ZoneSequencer::init_with_config(
        channel_id,
        signing_key,
        node.clone(),
        sdk_config,
        checkpoint,
    );

    // The Zone SDK on the LEZ channel, read-only: a throwaway key, nothing to fund. A fresh
    // store starts at the LIB, on the newest finalized LEZ inscription, with the finalized tree
    // built up to that block from the LEZ Sequencer (pinned by the block's hash).
    let lez_checkpoint: Option<SequencerCheckpoint> = registry
        .read()
        .expect("registry lock")
        .lez_checkpoint()
        .map(|c| serde_json::from_value(c.clone()))
        .transpose()
        .context("LEZ channel checkpoint in the tree store")?;
    let lez_checkpoint = match lez_checkpoint {
        Some(checkpoint) => checkpoint,
        None => {
            let (checkpoint, anchor) =
                lez_checkpoint_at_lib(&node, lez_channel_id, &source).await?;
            let final_block_id = registry
                .read()
                .expect("registry lock")
                .lez
                .final_reader()
                .lez_block_id;
            if final_block_id >= args.first_lez_block_id {
                bail!(
                    "the finalized tree is at LEZ block {final_block_id} but the LEZ channel has \
                     no checkpoint: start once with REBUILD_TREE=true"
                );
            }
            println!(
                "  lez       finalized LEZ block {} at the LIB (slot {}); reading the finalized \
                 history up to it from the LEZ Sequencer...",
                anchor.block_id(),
                u64::from(checkpoint.lib_slot)
            );
            if anchor.block_id() >= args.first_lez_block_id {
                fill_final_from_rpc(
                    &source,
                    &registry,
                    args.first_lez_block_id,
                    anchor.block_id() - 1,
                )
                .await?;
                if registry
                    .write()
                    .expect("registry lock")
                    .apply_lez_finalized(&anchor)?
                    .is_some()
                {
                    bail!(
                        "the LEZ Sequencer's history does not reach LEZ block {}",
                        anchor.block_id()
                    );
                }
            }
            save_lez_checkpoint(&registry, &checkpoint)?;
            checkpoint
        }
    };
    let mut lez_sdk = ZoneSequencer::init_with_config(
        lez_channel_id,
        reader_key(),
        node.clone(),
        SequencerConfig::new(FundingConfig {
            funding_pk: parse_funding_pk(&"00".repeat(32))?,
            change_pk: None,
            max_tx_fee: GasCost::new(0),
            priority_fee_percent: 0,
        }),
        Some(lez_checkpoint),
    );

    {
        let registry = registry.read().expect("registry lock");
        println!(
            "RLN Membership Zone {}",
            if indexer { "indexer" } else { "sequencer" }
        );
        println!("  node      {node_url}");
        println!(
            "  channel   {} ({})",
            hex::encode(channel_id.as_ref()),
            args.channel_id.as_deref().map_or_else(
                || format!("sha256 of {:?}", args.channel_name),
                |_| "given".into()
            )
        );
        println!("  tree_id   {}", hex::encode(tree_id));
        println!("  lez       {lez_source}");
        println!(
            "  lez       finalized tree at LEZ block {}, head at {}",
            registry.lez.final_reader().lez_block_id,
            registry.head().lez_block_id
        );
        println!("  http      http://{}", args.http_addr);
        println!("  data      {}", args.data_dir.display());
        println!(
            "  channel   {} final (final msg {}), {} in view, covered to LEZ block {}, first \
             block {}{}",
            registry.zone.final_entries().len(),
            short(&registry.zone.final_msg().into()),
            registry.zone.view().len(),
            registry.covered_to(),
            registry.first_lez_block_id,
            if resumed { " (checkpoint resumed)" } else { "" }
        );
        if let Some(stall) = registry.zone.stall() {
            println!(
                "  WARNING   the final tier is frozen at batch msg {} (L1 slot {}): {}; {} \
                 batch(es) parked after it",
                short(&stall.msg),
                stall.l1_slot,
                stall.error,
                stall.orphans_since
            );
        }
    }
    if let Some(msg) = registry.read().expect("registry lock").in_flight() {
        println!(
            "batch msg {} still in flight from the previous run; no new batch until it lands",
            short(&msg)
        );
    }
    println!("waiting for the Zone SDK to be ready...");

    let zone_channel = ZoneChannel {
        node_url: node_url.clone(),
        id: channel_id,
    };
    let mut ready = false;
    let mut lez_ready = false;
    // Batches LEZ has contradicted, as last reported (warned once per change).
    let mut contradicted_count = 0;
    let channel_view = sdk.subscribe_channel_view();
    let mut last_publish_unix = now_unix();
    let mut tick = tokio::time::interval(Duration::from_secs(args.poll_sec.max(1)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut finality_tick = tokio::time::interval(Duration::from_secs(FINALITY_SCAN_SEC));
    finality_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        // rustfmt does not reach into `select!`: every arm is one call, the work is in the
        // `on_*` functions above.
        tokio::select! {
            event = sdk.next_event() => match event {
                Event::Ready => {
                    if !ready {
                        println!("ready\n");
                    }
                    ready = true;
                }
                Event::BlocksProcessed { checkpoint, channel_update, finalized, .. } => {
                    let tip: [u8; 32] = channel_view.borrow().tip_message.into();
                    on_blocks_processed(
                        &mut sdk,
                        &registry,
                        &zone_channel,
                        tip,
                        &checkpoint,
                        &channel_update,
                        &finalized,
                    )
                    .await?;
                }
                _ => {}
            },
            lez_event = lez_sdk.next_event() => match lez_event {
                Event::Ready => {
                    if !lez_ready {
                        println!("LEZ channel ready");
                    }
                    lez_ready = true;
                }
                Event::BlocksProcessed { checkpoint, channel_update, finalized, .. } => {
                    on_lez_blocks_processed(
                        &source,
                        &registry,
                        &checkpoint,
                        &channel_update,
                        &finalized,
                    )
                    .await?;
                }
                _ => {}
            },
            _ = tick.tick(), if ready && lez_ready => {
                on_tick(
                    &mut sdk,
                    &registry,
                    &args,
                    &mut contradicted_count,
                    &mut last_publish_unix,
                )
                .await?;
            }
            _ = finality_tick.tick(), if ready => {
                match reconcile_finality(&node, channel_id, &registry).await {
                    Ok(0) => {}
                    Ok(n) => println!("finality scan: {n} entry(ies) the stream had missed"),
                    Err(e) => println!("finality scan failed: {e}"),
                }
            }
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    registry.write().expect("registry lock").close()?;
    println!("stopped; state in {}", args.data_dir.display());
    Ok(())
}
