//! PoC of the RLN Membership Zone (`docs/rfc-rln-membership-zone.md`): one Zone sequencer on
//! the Logos testnet.
//!
//! It reads closed LEZ blocks with the registration program's events (`lez.rs`), derives the
//! tree (`reader.rs`), publishes one batch (`batch.rs`) per change or heartbeat on its channel
//! through the Zone SDK, follows each batch to Bedrock finality, and serves the registry over
//! HTTP (`http.rs`): roots with their status, Merkle proof paths at the newest root in a
//! Bedrock block.
//! Until the program is deployed the LEZ source is a stand-in clock that closes empty blocks,
//! so the channel, the batches, finality and the API run end to end with an empty tree.
//!
//! Everything it must keep lives in `--data-dir`: `key.hex` (the accredited key; the channel id
//! is its public key), `checkpoint.json` (Zone SDK resume point), `history.json` (the published
//! batches). The trees are rebuilt from LEZ at start.

mod batch;
mod http;
mod lez;
mod reader;
mod registry;

use std::{
    fs,
    path::PathBuf,
    sync::{Arc, RwLock},
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow, bail};
use clap::Parser;
use lb_core::mantle::{
    gas::GasCost,
    ops::channel::{ChannelId, MsgId, inscribe::Inscription},
    transactions::TxHash,
};
use lb_key_management_system_service::keys::{ED25519_SECRET_KEY_SIZE, Ed25519Key, ZkPublicKey};
use lb_zone_sdk::{
    CommonHttpClient,
    adapter::NodeHttpClient,
    sequencer::{
        Error as SeqError, Event, FinalizedOp, FinalizedTx, FundingConfig, SequencerCheckpoint,
        SequencerConfig, ZoneSequencer,
    },
};
use prost::Message as _;
use rand::{RngCore, thread_rng};
use reqwest::Url;
use sha2::{Digest as _, Sha256};

use crate::{
    http::{AppState, Shared},
    lez::{LezSource, TickingLez},
    registry::{History, Registry, Status, now_unix},
};

/// Largest LEZ block range fetched at once (the LEZ Indexer caps `getEvents` at 1000).
const FETCH_SPAN: u64 = 1000;
/// The registration program on the testnet LEZ: its account id, base58 and hex.
const PROGRAM_ACCOUNT_ID_BASE58: &str = "HAxY9SiqUjJQHdtvazFDWFCxn1Jw1dtbUGio8DMC4bBw";
const PROGRAM_ACCOUNT_ID_HEX: &str =
    "f04803944507677bb79b27b61cd0ad6eea81928f281570347e2df6249495106a";
/// The registry (`tree_id`) the program serves, the first field of every event.
const TREE_ID_HEX: &str = "47b2de4f08080e74e2d4beffe8e5e87e709dd598619c9b322b64f2756979b4cf";
/// LEZ block the program was deployed in: no event comes before it.
const DEFAULT_FIRST_LEZ_BLOCK_ID: u64 = 49_431;

#[derive(Parser, Debug)]
#[command(about = "RLN Membership Zone PoC: Zone sequencer with an HTTP registry API")]
struct Args {
    /// Logos Blockchain node HTTP API (no auth, local).
    #[arg(long, default_value = "http://127.0.0.1:8080", env = "NODE_URL")]
    node_url: String,
    /// Node wallet key paying the Mantle Transaction fees (64 hex).
    #[arg(long, env = "FUNDING_PK")]
    funding_pk: String,
    /// Hard cap per publish, in atomic units.
    #[arg(long, default_value_t = 1_000_000, env = "MAX_TX_FEE")]
    max_tx_fee: u64,
    /// Key, checkpoint and history live here.
    #[arg(long, default_value = "poc/data", env = "DATA_DIR")]
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
    /// LEZ source: `indexer` = a LEZ Indexer's JSON-RPC (`getEvents`, finalized blocks);
    /// `ticking` = stand-in clock with empty blocks.
    #[arg(long, default_value = "ticking", env = "LEZ_SOURCE")]
    lez_source: String,
    /// LEZ Indexer JSON-RPC, the event source (`poc/lez-indexer/indexer_config.json`).
    #[arg(long, default_value = "http://127.0.0.1:8779", env = "LEZ_INDEXER_URL")]
    lez_indexer_url: String,
    /// LEZ Sequencer JSON-RPC, for the latest closed block (`getLastBlockId`).
    #[arg(
        long,
        default_value = "https://testnet.lez.logos.co",
        env = "LEZ_SEQUENCER_URL"
    )]
    lez_sequencer_url: String,
    /// First LEZ block the registry covers: the block the program was deployed in (default
    /// 49431 on the testnet). Ignored by the ticking source, which starts at first start.
    #[arg(long, env = "FIRST_LEZ_BLOCK_ID")]
    first_lez_block_id: Option<u64>,
    /// Seconds between LEZ polls (and the block period of the ticking source).
    #[arg(long, default_value_t = 10, env = "POLL_SEC")]
    poll_sec: u64,
    /// Publish an empty batch after this long without one.
    #[arg(long, default_value_t = 600, env = "HEARTBEAT_SEC")]
    heartbeat_sec: u64,
    /// LIB slots a published transaction may stay unmined before the Zone SDK re-funds it
    /// under a new transaction hash (same message id); 0 disables. The SDK default of 30 is
    /// one block's worth of LIB progress on the testnet, which re-funds every unmined batch at
    /// every finalized block.
    #[arg(long, default_value_t = 3_000, env = "STALE_REFUND_SLOTS")]
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

/// The accredited key: created on first start, read back after.
fn load_or_create_key(path: &PathBuf) -> Result<Ed25519Key> {
    let mut key_bytes = [0u8; ED25519_SECRET_KEY_SIZE];
    if path.exists() {
        let text = fs::read_to_string(path)?;
        key_bytes = parse_32(&text, "key.hex")?;
    } else {
        thread_rng().fill_bytes(&mut key_bytes);
        fs::write(path, hex::encode(key_bytes))?;
        println!("new accredited key written to {}", path.display());
    }
    Ok(Ed25519Key::from_bytes(&key_bytes))
}

fn msg_bytes(msg: &MsgId) -> [u8; 32] {
    *msg.as_ref()
}

fn tx_bytes(tx_hash: &TxHash) -> [u8; 32] {
    tx_hash.0
}

fn save_checkpoint(path: &PathBuf, checkpoint: &SequencerCheckpoint) -> Result<()> {
    fs::write(path, serde_json::to_string(checkpoint)?)
        .with_context(|| format!("write {}", path.display()))
}

/// Fetch and apply LEZ blocks `from..=to` in spans the source accepts.
fn apply_range(source: &mut dyn LezSource, registry: &Shared, from: u64, to: u64) -> Result<usize> {
    let mut applied = 0;
    let mut start = from;
    while start <= to {
        let end = to.min(start + FETCH_SPAN - 1);
        let blocks = source.blocks(start, end)?;
        let mut registry = registry.write().expect("registry lock");
        for block in &blocks {
            if !block.events.is_empty() {
                let names: Vec<_> = block.events.iter().map(|e| e.name()).collect();
                println!("LEZ block {}: events {}", block.block_id, names.join(", "));
            }
        }
        registry.apply_blocks(&blocks);
        applied += blocks.len();
        start = end + 1;
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
    let key_path = args.data_dir.join("key.hex");
    let checkpoint_path = args.data_dir.join("checkpoint.json");
    let history_path = args.data_dir.join("history.json");

    // LEZ source.
    let program_account_id = parse_32(&args.program_account_id, "PROGRAM_ACCOUNT_ID")?;
    let mut source: Box<dyn LezSource> = match args.lez_source.as_str() {
        "ticking" => Box::new(TickingLez::new(args.poll_sec)),
        "indexer" => bail!(
            "LEZ source `indexer` is not wired yet: it will read program {} ({}) from {} and the \
             head from {}",
            PROGRAM_ACCOUNT_ID_BASE58,
            hex::encode(program_account_id),
            args.lez_indexer_url,
            args.lez_sequencer_url
        ),
        other => bail!("unknown LEZ source {other:?}: `indexer` or `ticking`"),
    };
    // The ticking stand-in numbers its blocks by wall clock, so the program's deploy block
    // means nothing to it: it always starts at the block of the first start.
    let ticking = args.lez_source == "ticking";
    let first_block = if ticking {
        None
    } else {
        Some(
            args.first_lez_block_id
                .unwrap_or(DEFAULT_FIRST_LEZ_BLOCK_ID),
        )
    };

    // Registry: resume the published batches, rebuild the trees from LEZ below.
    let tree_id = parse_32(&args.tree_id, "TREE_ID")?;
    let history = match History::load(&history_path)? {
        Some(history) => history,
        None => {
            let first_lez_block_id = match first_block {
                Some(id) => id,
                None => source.last_block_id()?,
            };
            History {
                first_lez_block_id,
                covered_to: first_lez_block_id.saturating_sub(1),
                published: vec![],
            }
        }
    };
    let registry: Shared = Arc::new(RwLock::new(Registry::new(tree_id, history)));

    // Zone SDK sequencer on the node.
    let signing_key = load_or_create_key(&key_path)?;
    // The channel id is any 32 bytes nobody has written to yet: the key signing the first
    // inscription becomes its accredited key. A name hashes to a stable, readable choice.
    let channel_id = ChannelId::from(match &args.channel_id {
        Some(id) => parse_32(id, "CHANNEL_ID")?,
        None => Sha256::digest(args.channel_name.as_bytes()).into(),
    });
    let funding = FundingConfig {
        funding_pk: parse_funding_pk(&args.funding_pk)?,
        change_pk: None,
        max_tx_fee: GasCost::new(args.max_tx_fee),
        priority_fee_percent: FundingConfig::DEFAULT_PRIORITY_FEE_PERCENT,
    };
    let node_url: Url = args.node_url.parse().context("invalid NODE_URL")?;
    let node = NodeHttpClient::new(CommonHttpClient::new(None), node_url.clone());
    let checkpoint: Option<SequencerCheckpoint> = if checkpoint_path.exists() {
        Some(serde_json::from_str(&fs::read_to_string(
            &checkpoint_path,
        )?)?)
    } else {
        None
    };
    let resumed = checkpoint.is_some();
    let sdk_config = SequencerConfig {
        stale_refund_slots: args.stale_refund_slots,
        ..SequencerConfig::new(funding)
    };
    let mut sdk =
        ZoneSequencer::init_with_config(channel_id, signing_key, node, sdk_config, checkpoint);

    // HTTP API.
    let state = AppState {
        registry: registry.clone(),
        channel_id: hex::encode(channel_id.as_ref()),
        lez_source: source.describe(),
    };
    let listener = tokio::net::TcpListener::bind(&args.http_addr)
        .await
        .with_context(|| format!("bind {}", args.http_addr))?;
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, http::router(state)).await {
            eprintln!("http server stopped: {e}");
        }
    });

    {
        let registry = registry.read().expect("registry lock");
        println!("RLN Membership Zone PoC");
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
            "  history   {} batches published, covered to LEZ block {}, first block {}{}",
            registry.history.published.len(),
            registry.history.covered_to,
            registry.history.first_lez_block_id,
            if resumed { " (checkpoint resumed)" } else { "" }
        );
    }

    // Rebuild the trees: apply every LEZ block up to the last covered one again.
    let (first, covered_to) = {
        let registry = registry.read().expect("registry lock");
        (
            registry.history.first_lez_block_id,
            registry.history.covered_to,
        )
    };
    if covered_to >= first {
        let count = apply_range(source.as_mut(), &registry, first, covered_to)?;
        println!("replayed {count} LEZ blocks up to {covered_to}");
    }
    println!("waiting for the Zone sequencer to be ready...");

    let mut ready = false;
    // The batch waiting to land in a block (its message id), if any.
    let mut in_flight: Option<[u8; 32]> = None;
    let mut last_publish_unix = registry
        .read()
        .expect("registry lock")
        .history
        .published
        .last()
        .map_or(0, |p| p.published_at_unix);
    let mut tick = tokio::time::interval(Duration::from_secs(args.poll_sec.max(1)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            event = sdk.next_event() => match event {
                Event::Ready => {
                    if !ready { println!("ready\n"); }
                    ready = true;
                }
                Event::BlocksProcessed { checkpoint, channel_update, finalized, .. } => {
                    save_checkpoint(&checkpoint_path, &checkpoint)?;
                    let mut changed = false;
                    // Our batch is in a Bedrock block: its fee note is spent, the next may go.
                    for tx in channel_update.adopted() {
                        let Some(info) = tx.inscription() else { continue };
                        let msg = msg_bytes(&info.this_msg);
                        let mut registry = registry.write().expect("registry lock");
                        if let Some(entry) = registry.published_mut(&msg)
                            && entry.status == Status::Pending
                        {
                            entry.status = Status::Adopted;
                            entry.tx_hash = tx_bytes(&info.tx_hash);
                            println!("batch [{}..{}] msg {} adopted in a Bedrock block (tx {})", entry.from_lez_block_id, entry.to_lez_block_id, hex::encode(&msg[..4]), hex::encode(&entry.tx_hash[..4]));
                            changed = true;
                        }
                        if in_flight == Some(msg) { in_flight = None; }
                    }
                    // A reorg dropped our batch: publish the same payload again (a new message,
                    // since its parent changed).
                    for tx in channel_update.orphaned() {
                        let Some(info) = tx.inscription() else { continue };
                        let old = msg_bytes(&info.this_msg);
                        let payload = registry.read().expect("registry lock").payload(&old).cloned();
                        let Some(payload) = payload else { continue };
                        let inscription = Inscription::try_from(payload).map_err(|e| anyhow!("payload too large: {e:?}"))?;
                        match sdk.handle().publish(inscription).await {
                            Ok((result, checkpoint)) => {
                                let new_msg = msg_bytes(&result.tx.inscription().this_msg);
                                let new_tx = tx_bytes(&result.inscription_id());
                                registry.write().expect("registry lock").on_republished(old, new_msg, new_tx);
                                save_checkpoint(&checkpoint_path, &checkpoint)?;
                                in_flight = Some(new_msg);
                                println!("batch msg {} orphaned by a reorg, published again as msg {}", hex::encode(&old[..4]), hex::encode(&new_msg[..4]));
                                changed = true;
                            }
                            Err(e) => println!("republish after reorg failed: {e}"),
                        }
                    }
                    // Bedrock finality reached our batch: its root is a valid root from now on.
                    // Backfilled blocks report an empty `channel_update`, so finality is also
                    // the fallback signal that a batch landed.
                    for msg in finalize(&registry, &finalized) {
                        if in_flight == Some(msg) { in_flight = None; }
                        changed = true;
                    }
                    if changed {
                        registry.read().expect("registry lock").history.save(&history_path)?;
                    }
                }
                _ => {}
            },
            _ = tick.tick(), if ready => {
                // 1. LEZ: apply every block closed since the last poll.
                let head = match source.last_block_id() {
                    Ok(head) => head,
                    Err(e) => { println!("LEZ unreachable: {e}"); continue; }
                };
                let applied_to = registry.read().expect("registry lock").head.applied_block_id;
                if head > applied_to
                    && let Err(e) = apply_range(source.as_mut(), &registry, applied_to + 1, head)
                {
                    println!("LEZ read failed: {e}");
                    continue;
                }
                // 2. One batch covering the LEZ blocks not covered yet, when there are changes
                //    or the heartbeat is due, with at most one batch in flight.
                if in_flight.is_some() { continue; }
                let batch = {
                    let registry = registry.read().expect("registry lock");
                    let due = registry.history.published.is_empty()
                        || !registry.pending_changes.is_empty()
                        || now_unix().saturating_sub(last_publish_unix) >= args.heartbeat_sec;
                    if due { registry.next_batch() } else { None }
                };
                let Some(batch) = batch else { continue };
                let payload = batch.encode_to_vec();
                let inscription = match Inscription::try_from(payload.clone()) {
                    Ok(inscription) => inscription,
                    Err(e) => { println!("batch [{}..{}] does not fit one inscription: {e:?}", batch.from_lez_block_id, batch.to_lez_block_id); continue; }
                };
                match sdk.handle().publish(inscription).await {
                    Ok((result, checkpoint)) => {
                        let msg = msg_bytes(&result.tx.inscription().this_msg);
                        let tx_hash = tx_bytes(&result.inscription_id());
                        {
                            let mut registry = registry.write().expect("registry lock");
                            registry.on_published(msg, tx_hash, &batch, payload);
                            registry.history.save(&history_path)?;
                        }
                        save_checkpoint(&checkpoint_path, &checkpoint)?;
                        in_flight = Some(msg);
                        last_publish_unix = now_unix();
                        let described: Vec<_> = batch.changes.iter().map(|c| c.describe()).collect();
                        println!(
                            "batch [{}..{}] published as msg {} (tx {}): root {}.., {}",
                            batch.from_lez_block_id,
                            batch.to_lez_block_id,
                            hex::encode(&msg[..4]),
                            hex::encode(&tx_hash[..4]),
                            hex::encode(&batch.root_after[..4]),
                            if described.is_empty() { "no changes".into() } else { described.join("; ") }
                        );
                    }
                    Err(SeqError::Unavailable { reason }) => println!("not ready to publish ({reason}), retry next poll"),
                    Err(e) => println!("publish failed: {e}"),
                }
            },
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    println!("stopped; state in {}", args.data_dir.display());
    Ok(())
}

/// Mark our batches Finalized as Bedrock finality reaches them, with the L1 slot; returns
/// the message ids that became Finalized now.
fn finalize(registry: &Shared, finalized: &[FinalizedTx]) -> Vec<[u8; 32]> {
    let mut done = vec![];
    for tx in finalized {
        for op in &tx.ops {
            let FinalizedOp::Inscription(info) = op else {
                continue;
            };
            let msg = msg_bytes(&info.this_msg);
            let mut registry = registry.write().expect("registry lock");
            let payload_matches = registry
                .payload(&msg)
                .is_some_and(|p| p[..] == info.payload[..]);
            let Some(entry) = registry.published_mut(&msg) else {
                println!(
                    "finalized inscription msg {} is not one of ours",
                    hex::encode(&msg[..4])
                );
                continue;
            };
            if matches!(entry.status, Status::Finalized { .. }) {
                continue;
            }
            if !payload_matches {
                println!(
                    "finalized msg {} carries a different payload than we published; left as is",
                    hex::encode(&msg[..4])
                );
                continue;
            }
            let l1_slot = tx.l1_slot.into_inner();
            entry.status = Status::Finalized { l1_slot };
            entry.tx_hash = tx_bytes(&tx.tx_hash);
            println!(
                "batch [{}..{}] FINALIZED at L1 slot {l1_slot}: root {}.. is a valid root",
                entry.from_lez_block_id,
                entry.to_lez_block_id,
                hex::encode(&entry.root_after[..4])
            );
            done.push(msg);
        }
    }
    done
}
