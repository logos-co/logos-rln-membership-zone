//! The RLN Membership Zone node (`docs/rfc-rln-membership-zone.md`), on the Logos testnet:
//! one binary, two modes.
//!
//! Both derive the tree from the registration program's LEZ blocks (`lez.rs`, `reader.rs`) and
//! follow the Zone channel on Bedrock through the Zone SDK, as a final tier plus a view
//! (`chain.rs`, after LEZ's `chain_state`). The sequencer follows the blocks LEZ has closed and
//! publishes one batch (`batch.rs`) per change or heartbeat; the indexer follows only the LEZ
//! blocks final on Bedrock, publishes nothing and judges every batch of the channel against
//! its own tree. Both serve the registry over HTTP (`http.rs`).
//!
//! Everything a node must keep lives in `--data-dir`: `key.hex` (the sequencer's accredited
//! key; the channel id is `sha256(CHANNEL_NAME)`) and `tree/`, a sled store holding the tree,
//! the reader's state, the Zone channel state and the Zone SDK checkpoint.

mod batch;
mod chain;
mod http;
mod lez;
mod reader;
mod registry;

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
    ops::channel::{ChannelId, inscribe::Inscription},
};
use lb_key_management_system_service::keys::{ED25519_SECRET_KEY_SIZE, Ed25519Key, ZkPublicKey};
use lb_zone_sdk::{
    CommonHttpClient, Slot,
    adapter::{Node as _, NodeHttpClient},
    sequencer::{
        ChannelUpdate, Error as SeqError, Event, FinalizedOp, FundingConfig, InscriptionInfo,
        SequencerCheckpoint, SequencerConfig, ZoneSequencer, channel_inscriptions,
    },
};
use prost::Message as _;
use rand::{RngCore, thread_rng};
use reqwest::Url;
use sha2::{Digest as _, Sha256};

use crate::{
    chain::{AcceptOutcome, ChannelEntry, PublishRecord},
    http::{AppState, Shared},
    lez::{MAX_BLOCK_RANGE_LEN, SequencerLez},
    reader::{BlockIngestError, HASH_HISTORY},
    registry::Registry,
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

/// What this binary is: the one sequencer of the Zone, or an indexer that trusts nobody.
#[derive(Clone, Copy, Debug, PartialEq, ValueEnum)]
enum Mode {
    /// Follow the closed LEZ blocks, publish batches on the Zone channel, serve the API.
    Sequencer,
    /// Follow the LEZ blocks final on Bedrock, judge every batch of the channel against the
    /// tree, serve the API. No accredited key, no funds, no publishing.
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
        default_value = "rln-membership-zone:testnet:v0.3.0",
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
    /// LEZ Sequencer JSON-RPC the blocks come from: the public testnet one, or
    /// `http://127.0.0.1:3040` for a LEZ sequencer run next to it (`lez-sequencer/`).
    #[arg(
        long,
        default_value = "https://testnet.lez.logos.co",
        env = "LEZ_SEQUENCER_URL"
    )]
    lez_sequencer_url: String,
    /// First LEZ block the registry covers: the block the program was deployed in.
    #[arg(long, default_value_t = DEFAULT_FIRST_LEZ_BLOCK_ID, env = "FIRST_LEZ_BLOCK_ID")]
    first_lez_block_id: u64,
    /// Drop the tree and read it again from LEZ, keeping the Zone channel state and the Zone SDK
    /// checkpoint: after a decoder change that skipped transactions.
    #[arg(long, env = "REBUILD_TREE")]
    rebuild_tree: bool,
    /// Seconds between LEZ polls.
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

/// The node's key: created on first start, read back after. The sequencer's is the channel's
/// accredited key; the indexer's only names it to the SDK.
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

/// What the node holds for a channel (`GET /channel/:id`), the fields this check reads.
#[derive(serde::Deserialize)]
struct ChannelOnChain {
    accredited_keys: Vec<String>,
    tip_message: String,
}

/// Refuse to start a sequencer on a channel this data dir cannot continue. The Zone SDK learns
/// a channel's tip from finalized history only, so without a checkpoint it chains the next
/// batch on the root; on a channel that already has messages that batch is invalid
/// (`InvalidParent`) and vanishes without a trace. Likewise another key cannot write at all.
async fn check_channel_on_chain(
    node_url: &Url,
    channel_id: ChannelId,
    our_key: &[u8; 32],
    registry: &Shared,
    resumed: bool,
) -> Result<()> {
    let url = node_url.join(&format!("channel/{}", hex::encode(channel_id.as_ref())))?;
    let response = reqwest::get(url.clone())
        .await
        .with_context(|| format!("GET {url}"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        println!("  chain     channel not on chain yet; the first batch creates it");
        return Ok(());
    }
    let channel: ChannelOnChain = response
        .error_for_status()
        .with_context(|| format!("GET {url}"))?
        .json()
        .await
        .with_context(|| format!("GET {url}: unexpected body"))?;
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
            "channel tip on chain is {}.., our newest batch is {}..: the Zone channel state does not \
             match the channel (a batch of ours may still be in flight, or data from another run \
             was lost); wait, or pick another CHANNEL_NAME",
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

/// Apply the LEZ blocks `from..=to` to the tree in spans of at most `MAX_BLOCK_RANGE_LEN`,
/// each checked to continue the chain first; a rewritten history is rewound to the last block
/// both agree on and read again. Returns how many blocks were applied.
async fn apply_range(
    source: &SequencerLez,
    registry: &Shared,
    from: u64,
    to: u64,
) -> Result<usize> {
    let mut applied = 0;
    let mut start = from;
    while start <= to {
        let end = to.min(start + MAX_BLOCK_RANGE_LEN - 1);
        let blocks = source.blocks(start, end).await?;
        // Fewer than asked: the rest is not final on Bedrock yet, read again next time.
        let short_answer = (blocks.len() as u64) < end - start + 1;
        // The chain check comes first: nothing of a span that does not continue the
        // reader's chain is applied or logged.
        let broken = {
            let registry = registry.read().expect("registry lock");
            match registry.head.check_chain(&blocks) {
                Ok(()) => false,
                Err(e @ BlockIngestError::BrokenChainLink { .. }) => {
                    println!("LEZ FORK: {e}; looking for the last block LEZ still agrees on");
                    true
                }
                Err(e) => return Err(e.into()),
            }
        };
        if broken {
            let fork = find_fork(source, registry).await?;
            let mut registry = registry.write().expect("registry lock");
            let was_at = registry.head.lez_block_id;
            let pending_before = registry.pending_changes.len();
            registry.rewind_to(fork)?;
            registry.save()?;
            println!(
                "LEZ FORK: at block {fork}; blocks {}..{was_at} ({} block(s), no tree change) \
                 forgotten, {} pending change(s) dropped; reading again from {}",
                fork + 1,
                was_at - fork,
                pending_before - registry.pending_changes.len(),
                fork + 1
            );
            start = fork + 1;
            continue;
        }
        let mut registry = registry.write().expect("registry lock");
        for block in &blocks {
            if !block.events.is_empty() {
                let names: Vec<_> = block.events.iter().map(lez::selector_name).collect();
                println!(
                    "LEZ block {}: events {}",
                    block.block_id(),
                    names.join(", ")
                );
            }
            if block.private_transaction_count > 0 {
                println!(
                    "WARNING: LEZ block {}: {} privacy-preserving transaction(s) under the \
                     registration program skipped; the registry changed in a way this tree does \
                     not have",
                    block.block_id(),
                    block.private_transaction_count
                );
            }
        }
        registry.apply_blocks(&blocks)?;
        registry.save()?;
        applied += blocks.len();
        if short_answer {
            break;
        }
        start = end + 1;
    }
    Ok(applied)
}

/// The newest LEZ block the reader applied whose hash LEZ still serves: where a rewritten
/// history forks off. Walks back from the reader's head through the hashes it kept.
async fn find_fork(source: &SequencerLez, registry: &Shared) -> Result<u64> {
    let (mut high, oldest) = {
        let registry = registry.read().expect("registry lock");
        let oldest = registry
            .head
            .oldest_hashed_block_id()
            .context("no block hash kept, cannot find where LEZ forked")?;
        (registry.head.lez_block_id, oldest)
    };
    loop {
        let low = high.saturating_sub(63).max(oldest);
        println!("LEZ FORK: comparing blocks {low}..{high} with the hashes kept");
        let blocks = source.blocks(low, high).await?;
        {
            let registry = registry.read().expect("registry lock");
            for block in blocks.iter().rev() {
                if registry.head.block_hash(block.block_id()) == Some(block.hash()) {
                    return Ok(block.block_id());
                }
            }
        }
        if low == oldest {
            bail!(
                "LEZ FORK older than the last {} blocks kept: the tree must be read again \
                 (REBUILD_TREE=true)",
                HASH_HISTORY
            );
        }
        high = low - 1;
    }
}

/// Say what the final tier made of a finalized entry.
fn report_finalized(entry: &ChannelEntry, l1_slot: u64, outcome: Option<&AcceptOutcome>) {
    let msg: [u8; 32] = entry.msg.into();
    let range = entry.batch.as_ref().map_or("no batch".to_string(), |b| {
        format!("[{}..{}]", b.from_lez_block_id, b.to_lez_block_id)
    });
    match outcome {
        Some(AcceptOutcome::Applied(_)) => println!(
            "batch {range} msg {} FINALIZED at L1 slot {l1_slot}: applied, root {}..",
            short(&msg),
            entry
                .batch
                .as_ref()
                .map(|b| hex::encode(&b.root_after[..4]))
                .unwrap_or_default()
        ),
        Some(AcceptOutcome::Parked(e)) => println!(
            "batch {range} msg {} FINALIZED at L1 slot {l1_slot}: PARKED, {e}; the final tier \
             is frozen here",
            short(&msg)
        ),
        Some(AcceptOutcome::RetryableFailure(e)) => println!(
            "batch {range} msg {} FINALIZED at L1 slot {l1_slot}: {e}",
            short(&msg)
        ),
        Some(AcceptOutcome::AlreadyApplied) => {}
        None => println!(
            "entry msg {} FINALIZED at L1 slot {l1_slot}: {range}",
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
                    let outcome = registry.apply_finalized(entry.clone(), l1_slot);
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

    // LEZ source: closed blocks for the sequencer, Bedrock-finalized ones for the indexer.
    let program_account_id = parse_32(&args.program_account_id, "PROGRAM_ACCOUNT_ID")?;
    let tree_id = parse_32(&args.tree_id, "TREE_ID")?;
    let source = SequencerLez::new(
        args.lez_sequencer_url.clone(),
        program_account_id,
        tree_id,
        indexer,
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
        lez_source: source.describe(),
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
    // pays; the indexer with a key of its own that is not accredited, so it only reads (it
    // never funds a transaction, the funding key is a name to the SDK).
    let signing_key = load_or_create_key(&args.data_dir.join("key.hex"))?;
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
    if !indexer {
        check_channel_on_chain(
            &node_url,
            channel_id,
            &signing_key.public_key().to_bytes(),
            &registry,
            resumed,
        )
        .await?;
    }
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
        println!("  lez       {}", source.describe());
        println!("  http      http://{}", args.http_addr);
        println!("  data      {}", args.data_dir.display());
        println!(
            "  channel   {} final (final msg {}), {} in view, covered to LEZ block {}, first \
             block {}{}",
            registry.chain.final_entries().len(),
            short(&registry.chain.final_msg().into()),
            registry.chain.view().len(),
            registry.covered_to(),
            registry.first_lez_block_id,
            if resumed { " (checkpoint resumed)" } else { "" }
        );
        if let Some(stall) = registry.chain.stall() {
            println!(
                "  WARNING   the final tier is frozen at batch msg {} (L1 slot {}): {}; {} batch(es) \
                 parked after it",
                short(&stall.msg),
                stall.l1_slot,
                stall.error,
                stall.orphans_since
            );
        }
    }
    source.set_registered_count(
        registry
            .read()
            .expect("registry lock")
            .head
            .stats
            .registered,
    );
    if let Some(msg) = registry.read().expect("registry lock").in_flight() {
        println!(
            "batch msg {} still in flight from the previous run; no new batch until it lands",
            short(&msg)
        );
    }
    println!("waiting for the Zone SDK to be ready...");

    let mut ready = false;
    // Batches LEZ has contradicted, as last reported (warned once per change).
    let mut contradicted_count = 0;
    let channel_view = sdk.subscribe_channel_view();
    let mut last_publish_unix = now_unix();
    let mut tick = tokio::time::interval(Duration::from_secs(args.poll_sec.max(1)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut finality_tick = tokio::time::interval(Duration::from_secs(FINALITY_SCAN_SEC));
    finality_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            event = sdk.next_event() => match event {
                Event::Ready => {
                    if !ready { println!("ready\n"); }
                    ready = true;
                }
                Event::BlocksProcessed { checkpoint, channel_update, finalized, .. } => {
                    save_checkpoint(&registry, &checkpoint)?;
                    // The channel tip is one of our batches: it and every pending publish
                    // before it are in a Bedrock block (an inscription we published is not
                    // echoed in `adopted` on a plain chain extension).
                    let tip: [u8; 32] = channel_view.borrow().tip_message.into();
                    for msg in registry.write().expect("registry lock").mark_adopted_up_to(tip) {
                        println!("batch msg {} adopted in a Bedrock block (channel tip)", short(&msg));
                    }
                    match &channel_update {
                        ChannelUpdate::Extension { adopted } => {
                            let entries: Vec<ChannelEntry> = adopted.iter().filter_map(|tx| tx.inscription()).map(entry_of).collect();
                            for entry in &entries {
                                println!("channel entry msg {} adopted in a Bedrock block", short(&entry.msg.into()));
                            }
                            registry.write().expect("registry lock").apply_extension(entries);
                        }
                        ChannelUpdate::Conflict { .. } => {
                            let canonical: Vec<ChannelEntry> = channel_update
                                .canonical_chain()
                                .into_iter()
                                .flatten()
                                .filter_map(|tx| tx.inscription())
                                .map(entry_of)
                                .collect();
                            let dropped = registry.write().expect("registry lock").apply_conflict(canonical);
                            // A reorg dropped our batches: publish the same payloads again, in
                            // order (new messages, since their parent changed).
                            for entry in dropped {
                                let Some(batch) = entry.batch else { continue };
                                let inscription = Inscription::try_from(batch.encode_to_vec()).map_err(|e| anyhow!("payload too large: {e:?}"))?;
                                match sdk.handle().publish(inscription).await {
                                    Ok((result, checkpoint)) => {
                                        let info = result.tx.inscription();
                                        let record = registry.write().expect("registry lock").record_publish(info.this_msg, info.parent_msg, &batch);
                                        save_checkpoint(&registry, &checkpoint)?;
                                        println!(
                                            "batch [{}..{}] msg {} orphaned by a reorg, published again as msg {} ({record:?})",
                                            batch.from_lez_block_id, batch.to_lez_block_id,
                                            short(&entry.msg.into()), short(&info.this_msg.into())
                                        );
                                    }
                                    Err(e) => println!("republish after reorg failed: {e}"),
                                }
                            }
                        }
                    }
                    if let Some(gap) = registry.read().expect("registry lock").chain.check_for_gaps_in_lineage() {
                        println!(
                            "WARNING: channel entry msg {} chains on {}, which the view does not hold: the lineage has a gap",
                            short(&gap.msg.into()), short(&gap.parent.into())
                        );
                    }
                    // Bedrock finality reached entries of the channel: into the final tier.
                    for tx in &finalized {
                        let l1_slot = tx.l1_slot.into_inner();
                        for op in &tx.ops {
                            let FinalizedOp::Inscription(info) = op else { continue };
                            let entry = entry_of(info);
                            let outcome = registry.write().expect("registry lock").apply_finalized(entry.clone(), l1_slot);
                            report_finalized(&entry, l1_slot, outcome.as_ref());
                        }
                    }
                    registry.write().expect("registry lock").save()?;
                }
                _ => {}
            },
            _ = tick.tick(), if ready => {
                // 1. LEZ: apply every block since the last poll (the finalized ones, for the
                //    indexer).
                let head = match source.last_block_id().await {
                    Ok(head) => head,
                    Err(e) => { println!("LEZ unreachable: {e}"); continue; }
                };
                let applied_to = registry.read().expect("registry lock").head.lez_block_id;
                if head > applied_to
                    && let Err(e) = apply_range(&source, &registry, applied_to + 1, head).await
                {
                    println!("LEZ read failed: {e}");
                    continue;
                }
                if indexer {
                    let decided = registry.write().expect("registry lock").retry_folds();
                    if decided > 0 {
                        println!("{decided} batch(es) judged now that LEZ is read far enough");
                        registry.write().expect("registry lock").save()?;
                    }
                    continue;
                }
                // 2. The sequencer checks its own statements against the tree LEZ gives now.
                {
                    let registry = registry.read().expect("registry lock");
                    let contradicted = registry.contradicted();
                    if contradicted.len() != contradicted_count {
                        contradicted_count = contradicted.len();
                        for e in contradicted {
                            println!(
                                "WARNING: batch msg {} claims root {}.. at LEZ block {}, LEZ now gives {}..: an indexer parks on it and every later batch",
                                &e.msg_id[..8], &e.root.clone().unwrap_or_default()[..8], e.to_lez_block_id.unwrap_or_default(),
                                e.to_lez_block_id.and_then(|to| registry.head.root_at(to)).map(|r| hex::encode(&r[..4])).unwrap_or_default()
                            );
                        }
                    }
                }
                // 3. One batch covering the LEZ blocks not covered yet, when there are changes
                //    or the heartbeat is due, with at most one batch in flight.
                let batch = {
                    let registry = registry.read().expect("registry lock");
                    if registry.in_flight().is_some() { continue; }
                    let due = registry.entries().is_empty()
                        || !registry.pending_changes.is_empty()
                        || now_unix().saturating_sub(last_publish_unix) >= args.heartbeat_sec;
                    if due { registry.next_batch() } else { None }
                };
                let Some(batch) = batch else { continue };
                let inscription = match Inscription::try_from(batch.encode_to_vec()) {
                    Ok(inscription) => inscription,
                    Err(e) => { println!("batch [{}..{}] does not fit one inscription: {e:?}", batch.from_lez_block_id, batch.to_lez_block_id); continue; }
                };
                match sdk.handle().publish(inscription).await {
                    Ok((result, checkpoint)) => {
                        let info = result.tx.inscription();
                        let record = {
                            let mut registry = registry.write().expect("registry lock");
                            let record = registry.record_publish(info.this_msg, info.parent_msg, &batch);
                            registry.save()?;
                            record
                        };
                        save_checkpoint(&registry, &checkpoint)?;
                        last_publish_unix = now_unix();
                        let described: Vec<_> = batch.changes.iter().map(|c| c.describe()).collect();
                        println!(
                            "batch [{}..{}] published as msg {} on {}: root {}.., {}{}",
                            batch.from_lez_block_id,
                            batch.to_lez_block_id,
                            short(&info.this_msg.into()),
                            short(&info.parent_msg.into()),
                            hex::encode(&batch.root_after[..4]),
                            if described.is_empty() { "no changes".into() } else { described.join("; ") },
                            if record == PublishRecord::Extended { "".into() } else { format!(" ({record:?})") }
                        );
                    }
                    Err(SeqError::Unavailable { reason }) => println!("not ready to publish ({reason}), retry next poll"),
                    Err(e) => println!("publish failed: {e}"),
                }
            },
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
