//! The Zone sequencer of the RLN Membership Zone (`docs/rfc-rln-membership-zone.md`), on the
//! Logos testnet.
//!
//! It reads closed LEZ blocks from a LEZ Sequencer's JSON-RPC and derives the program's events
//! from its transactions (`lez.rs`), derives the tree (`reader.rs`), publishes one batch (`batch.rs`) per change or heartbeat on its channel
//! through the Zone SDK, follows each batch to Bedrock finality, and serves the registry over
//! HTTP (`http.rs`): roots with their status, Merkle proof paths at the newest root in a
//! Bedrock block.
//!
//! Everything it must keep lives in `--data-dir`: `key.hex` (the accredited key; the channel id
//! is `sha256(CHANNEL_NAME)`) and `tree/`, a sled store holding the tree, the reader's state,
//! the published batches and the Zone SDK checkpoint, so a restart opens it and continues.

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

use anyhow::{Context as _, Result, anyhow};
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
    lez::{SEQUENCER_RANGE, SequencerLez},
    registry::{Registry, Status, now_unix},
};

/// The registration program on the testnet LEZ: its account id
/// (`HAxY9SiqUjJQHdtvazFDWFCxn1Jw1dtbUGio8DMC4bBw` in base58).
const PROGRAM_ACCOUNT_ID_HEX: &str =
    "f04803944507677bb79b27b61cd0ad6eea81928f281570347e2df6249495106a";
/// The registry (`tree_id`) the program serves, the first field of every event.
const TREE_ID_HEX: &str = "47b2de4f08080e74e2d4beffe8e5e87e709dd598619c9b322b64f2756979b4cf";
/// LEZ block the program was deployed in: no event comes before it.
const DEFAULT_FIRST_LEZ_BLOCK_ID: u64 = 49_431;

#[derive(Parser, Debug)]
#[command(about = "RLN Membership Zone: Zone sequencer with an HTTP registry API")]
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
    /// Reserve over the mandatory fee, in percent; the excess becomes the tip that ranks the
    /// transaction in a congested mempool. The Zone SDK default is 12.
    #[arg(long, default_value_t = 12, env = "PRIORITY_FEE_PERCENT")]
    priority_fee_percent: u64,
    /// Key, checkpoint and history live here.
    #[arg(long, default_value = "zone-sequencer/data", env = "DATA_DIR")]
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
    /// Drop the tree and read it again from LEZ, keeping the batch history and the Zone SDK
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
    /// queue, while a resubmit of the same transaction keeps its place. The SDK default, 30,
    /// is one finalized block's worth of LIB progress on the testnet.
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

/// Refuse to start on a channel this data dir cannot continue. The Zone SDK learns a
/// channel's tip from finalized history only, so without a checkpoint it chains the next
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
        anyhow::bail!(
            "channel is on chain with accredited keys {:?}, not ours ({ours}): this key cannot \
             write to it; restore the original key.hex or pick another CHANNEL_NAME",
            channel.accredited_keys
        );
    }
    let newest_msg = registry
        .read()
        .expect("registry lock")
        .history
        .published
        .iter()
        .rev()
        .find(|p| p.status != Status::Orphaned)
        .map(|p| hex::encode(p.msg_id));
    let tip = channel.tip_message.to_ascii_lowercase();
    match newest_msg {
        Some(msg) if msg == tip && resumed => {
            println!(
                "  chain     channel tip {}.. is our newest batch",
                &tip[..8]
            );
            Ok(())
        }
        Some(msg) if msg == tip => anyhow::bail!(
            "channel tip {}.. is our newest batch but the data dir holds no Zone SDK checkpoint: \
             the SDK would chain on the root and every batch would be invalid; restore the data \
             dir or pick another CHANNEL_NAME",
            &tip[..8]
        ),
        Some(msg) => anyhow::bail!(
            "channel tip on chain is {}.., our newest batch is {}..: the history does not match \
             the channel (a batch of ours may still be pending, or data from another run was \
             lost); wait, or pick another CHANNEL_NAME",
            &tip[..8],
            &msg[..8]
        ),
        None => anyhow::bail!(
            "channel is already on chain (tip {}..) but this data dir holds no batch: the next \
             batch would chain on the root and be invalid; restore zone-sequencer/data from the \
             run that wrote the channel, or pick another CHANNEL_NAME",
            &tip[..8]
        ),
    }
}

/// Fetch and apply LEZ blocks `from..=to` in spans the source accepts.
async fn apply_range(
    source: &SequencerLez,
    registry: &Shared,
    from: u64,
    to: u64,
) -> Result<usize> {
    let mut applied = 0;
    let mut start = from;
    while start <= to {
        let end = to.min(start + SEQUENCER_RANGE - 1);
        let blocks = source.blocks(start, end).await?;
        let mut registry = registry.write().expect("registry lock");
        for block in &blocks {
            if !block.events.is_empty() {
                let names: Vec<_> = block.events.iter().map(|e| e.name()).collect();
                println!("LEZ block {}: events {}", block.block_id, names.join(", "));
            }
        }
        registry.apply_blocks(&blocks)?;
        registry.save()?;
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

    // LEZ source.
    let program_account_id = parse_32(&args.program_account_id, "PROGRAM_ACCOUNT_ID")?;
    let tree_id = parse_32(&args.tree_id, "TREE_ID")?;
    let source = SequencerLez::new(args.lez_sequencer_url.clone(), program_account_id, tree_id);

    // Registry: the tree store under the data dir, with the batch history and the checkpoint.
    let registry: Shared = Arc::new(RwLock::new(Registry::open(
        &args.data_dir,
        tree_id,
        args.first_lez_block_id,
        args.rebuild_tree,
    )?));

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
        priority_fee_percent: args.priority_fee_percent,
    };
    let node_url: Url = args.node_url.parse().context("invalid NODE_URL")?;
    let node = NodeHttpClient::new(CommonHttpClient::new(None), node_url.clone());
    let checkpoint: Option<SequencerCheckpoint> = registry
        .read()
        .expect("registry lock")
        .checkpoint()
        .map(|c| serde_json::from_value(c.clone()))
        .transpose()
        .context("Zone SDK checkpoint in the tree store")?;
    let resumed = checkpoint.is_some();
    check_channel_on_chain(
        &node_url,
        channel_id,
        &signing_key.public_key().to_bytes(),
        &registry,
        resumed,
    )
    .await?;
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
        println!("RLN Membership Zone sequencer");
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

    source.set_registered_count(
        registry
            .read()
            .expect("registry lock")
            .head
            .stats
            .registered,
    );
    println!("waiting for the Zone sequencer to be ready...");

    let mut ready = false;
    // The batch waiting to land in a block (its message id), if any; after a restart, the
    // newest Pending batch of the history, which the SDK checkpoint keeps resubmitting.
    // The SDK's view of the channel: its `tip_message` is how our own batches are seen landing,
    // since an inscription we published is not echoed in `ChannelUpdate::adopted` on a plain
    // chain extension (only on a branch change).
    let channel_view = sdk.subscribe_channel_view();
    let mut in_flight: Option<[u8; 32]> = registry
        .read()
        .expect("registry lock")
        .history
        .published
        .iter()
        .rev()
        .find(|p| p.status == Status::Pending)
        .map(|p| p.msg_id);
    if let Some(msg) = in_flight {
        println!(
            "batch msg {} still pending from the previous run; no new batch until it lands",
            hex::encode(&msg[..4])
        );
    }
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
                    save_checkpoint(&registry, &checkpoint)?;
                    let mut changed = false;
                    // The channel tip is one of our batches: it and every pending batch before
                    // it (its ancestors in the channel) are in a Bedrock block.
                    let tip = msg_bytes(&channel_view.borrow().tip_message);
                    {
                        let mut registry = registry.write().expect("registry lock");
                        if let Some(at) = registry.history.published.iter().position(|p| p.msg_id == tip) {
                            for entry in registry.history.published.iter_mut().take(at + 1) {
                                if entry.status == Status::Pending {
                                    entry.status = Status::Adopted;
                                    println!("batch [{}..{}] msg {} adopted in a Bedrock block (channel tip)", entry.from_lez_block_id, entry.to_lez_block_id, hex::encode(&entry.msg_id[..4]));
                                    changed = true;
                                    if in_flight == Some(entry.msg_id) { in_flight = None; }
                                }
                            }
                        }
                    }
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
                                save_checkpoint(&registry, &checkpoint)?;
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
                        registry.write().expect("registry lock").save()?;
                    }
                }
                _ => {}
            },
            _ = tick.tick(), if ready => {
                // 1. LEZ: apply every block closed since the last poll.
                let head = match source.last_block_id().await {
                    Ok(head) => head,
                    Err(e) => { println!("LEZ unreachable: {e}"); continue; }
                };
                let applied_to = registry.read().expect("registry lock").head.applied_block_id;
                if head > applied_to
                    && let Err(e) = apply_range(&source, &registry, applied_to + 1, head).await
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
                            registry.save()?;
                        }
                        save_checkpoint(&registry, &checkpoint)?;
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
    registry.write().expect("registry lock").close()?;
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
