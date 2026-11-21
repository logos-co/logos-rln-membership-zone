//! The RLN Membership Zone node (the RLN Membership Zone LIP in logos-lips), on the Logos testnet:
//! one binary, two modes.
//!
//! Both follow two channels on Bedrock through the Zone SDK, each kept as a final tier plus a
//! view after LEZ's `chain_state`: the LEZ channel (`lez_chain.rs`, fed by `lez_sync.rs`), whose
//! inscriptions are the LEZ blocks the tree is derived from (`lez.rs`, `reader.rs`), and the Zone
//! channel (`zone_chain.rs`, fed by `zone_sync.rs`), whose inscriptions are the batches
//! (`batch.rs`). The sequencer publishes
//! one batch per change or heartbeat over the LEZ blocks adopted on Bedrock; the indexer
//! publishes nothing and judges every batch against the tree of the LEZ blocks finalized on
//! Bedrock. Both serve the registry over HTTP (`http.rs`).
//!
//! Everything a node must keep lives in `--data-dir`: `key.hex` (the sequencer's accredited
//! key; the channel id is `sha256(CHANNEL_NAME)`; an indexer has none, it signs nothing),
//! `tree/` (sled, the tree of the finalized LEZ blocks) and the small state files beside it,
//! each written when it changed (`registry.rs`): `reader.json`, `zone.json`, `final.jsonl`,
//! `lez_view.json`. The LEZ channel keeps no checkpoint: every start begins at the LIB.

mod batch;
mod http;
mod lez;
mod lez_chain;
mod lez_sync;
mod reader;
mod registry;
mod zone_chain;
mod zone_sync;

use std::{
    fs,
    path::PathBuf,
    sync::{Arc, RwLock},
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow, bail};
use clap::{Parser, ValueEnum};
use lb_core::mantle::{gas::GasCost, ops::channel::ChannelId};
use lb_key_management_system_service::keys::{ED25519_SECRET_KEY_SIZE, Ed25519Key, ZkPublicKey};
use lb_zone_sdk::{
    CommonHttpClient,
    adapter::NodeHttpClient,
    sequencer::{Event, FundingConfig, SequencerCheckpoint, SequencerConfig, ZoneSequencer},
};
use rand::{RngCore, thread_rng};
use reqwest::Url;
use sha2::{Digest as _, Sha256};
use tracing::{error, info, warn};

use crate::{
    http::{AppState, Shared},
    lez::LezSequencer,
    lez_sync::{apply_lez_finalized, lez_checkpoint_at_lib, on_lez_blocks_processed},
    reader::{parse_32, short},
    registry::Registry,
    zone_sync::{
        FINALITY_SCAN_SEC, TickCounters, ZoneBlocks, ZoneChannel, channel_on_chain,
        check_channel_on_chain, checkpoint_at_lib, now_unix, on_blocks_processed, on_tick,
        reconcile_finality, wait_for_following, watch_l1,
    },
};

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
    /// Key, tree store and state files live here.
    #[arg(long, default_value = "data", env = "DATA_DIR")]
    data_dir: PathBuf,
    /// HTTP API bind address.
    #[arg(long, default_value = "127.0.0.1:3000", env = "HTTP_ADDR")]
    http_addr: String,
    /// Name of the Zone channel: its id is `sha256(name)`.
    #[arg(
        long,
        default_value = "rln-membership-zone:testnet:v0.3.2-3",
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

/// The funding of a reader of a channel it is not accredited on: it never publishes, so it
/// never pays; the funding key is a name to the SDK.
fn unfunded() -> Result<FundingConfig> {
    Ok(FundingConfig {
        funding_pk: parse_funding_pk(&"00".repeat(32))?,
        change_pk: None,
        max_tx_fee: GasCost::new(0),
        priority_fee_percent: 0,
    })
}

/// A key that signs nothing: the Zone SDK wants one per instance, a reader of a channel it is
/// not accredited on never uses it. Fresh on every start, never written.
fn reader_key() -> Ed25519Key {
    let mut key_bytes = [0u8; ED25519_SECRET_KEY_SIZE];
    thread_rng().fill_bytes(&mut key_bytes);
    Ed25519Key::from_bytes(&key_bytes)
}

/// The sequencer's accredited key: created on first start (readable by its owner only), read
/// back after.
fn load_or_create_key(path: &PathBuf) -> Result<Ed25519Key> {
    let mut key_bytes = [0u8; ED25519_SECRET_KEY_SIZE];
    if path.exists() {
        let text = fs::read_to_string(path)?;
        key_bytes = parse_32(&text, "key.hex")?;
    } else {
        thread_rng().fill_bytes(&mut key_bytes);
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        options
            .open(path)
            .and_then(|mut file| {
                std::io::Write::write_all(&mut file, hex::encode(key_bytes).as_bytes())
            })
            .with_context(|| format!("write {}", path.display()))?;
        info!("new key written to {}", path.display());
    }
    Ok(Ed25519Key::from_bytes(&key_bytes))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,zone=info,logos_blockchain=error".into()),
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
    wait_for_following(&node).await?;

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
            error!("http server stopped: {e}");
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
        (None, true) => unfunded()?.funding_pk,
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
            if indexer {
                checkpoint
            } else {
                let after = check_channel_on_chain(
                    channel,
                    &signing_key.public_key().to_bytes(),
                    &registry,
                    resumed,
                )?;
                if after == 0 {
                    // The chain's tip is the parent of the next publish; the SDK takes it from
                    // the checkpoint alone, so the checkpoint says what the chain says.
                    let tip = parse_32(&channel.tip_message, "channel tip")?;
                    let mut registry = registry.write().expect("registry lock");
                    let dropped = registry.drop_pending_not_in_view();
                    if dropped > 0 {
                        warn!(
                            "{dropped} publish(es) of ours recorded in flight never chained on \
                             the channel (stale, from an earlier run): dropped"
                        );
                    }
                    let (aligned, pending_dropped) = registry.align_checkpoint_tip(tip);
                    if aligned {
                        warn!(
                            "the Zone SDK checkpoint's tip was not the channel's tip {} on chain: \
                             corrected, the next batch chains on the tip",
                            short(&tip)
                        );
                    }
                    if pending_dropped > 0 {
                        warn!(
                            "{pending_dropped} pending publish(es) in the Zone SDK checkpoint while \
                             nothing of ours is in flight (stale, from an earlier run): dropped"
                        );
                    }
                    if dropped > 0 || aligned || pending_dropped > 0 {
                        registry.save()?;
                    }
                    registry
                        .checkpoint()
                        .map(|c| serde_json::from_value(c.clone()))
                        .transpose()
                        .context("Zone SDK checkpoint after the tip check")?
                } else {
                    // Entries of ours the chain does not hold any more: orphaned, published
                    // again by the tick; the SDK checkpoint moves back to the tip.
                    let tip = parse_32(&channel.tip_message, "channel tip")?;
                    let mut registry = registry.write().expect("registry lock");
                    let orphaned = registry.reconcile_with_chain(tip);
                    registry.save()?;
                    warn!(
                        "{orphaned} batch(es) of ours after the channel tip {} are not on \
                         chain: orphaned, published again at the next tick, the Zone SDK \
                         checkpoint moved back to the tip",
                        short(&tip)
                    );
                    registry
                        .checkpoint()
                        .map(|c| serde_json::from_value(c.clone()))
                        .transpose()
                        .context("Zone SDK checkpoint after the reconciliation")?
                }
            }
        }
        // No message of the channel on chain: nothing to walk, start at the LIB. A batch of
        // ours still in flight is the one thing a stored checkpoint knows better.
        None if registry
            .read()
            .expect("registry lock")
            .in_flight()
            .is_none() =>
        {
            info!("chain: channel not on chain yet; the first batch creates it");
            let checkpoint = checkpoint_at_lib(&node).await?;
            // The finality scan starts where the SDK does.
            registry
                .write()
                .expect("registry lock")
                .seed_anchor(u64::from(checkpoint.lib_slot));
            Some(checkpoint)
        }
        None => checkpoint,
    };
    let sdk_config = SequencerConfig {
        stale_refund_slots: args.stale_refund_slots,
        ..SequencerConfig::new(funding)
    };
    let mut zone_sdk = ZoneSequencer::init_with_config(
        channel_id,
        signing_key,
        node.clone(),
        sdk_config,
        checkpoint,
    );

    // The Zone SDK on the LEZ channel, read-only: a throwaway key, nothing to fund, and no
    // checkpoint on disk: every start begins at the LIB, on the newest finalized LEZ
    // inscription, and the finalized tree is filled up to that block from the LEZ Sequencer
    // under the block's hash (nothing to do when the tree is already there). The SDK then
    // reports the adopted blocks from the LIB on; the ones persisted come back at once.
    let (lez_checkpoint, anchor) = lez_checkpoint_at_lib(&node, lez_channel_id, &source).await?;
    info!(
        "lez: finalized LEZ block {} at the LIB (slot {})",
        anchor.block_id(),
        u64::from(lez_checkpoint.lib_slot)
    );
    if anchor.block_id() >= args.first_lez_block_id {
        apply_lez_finalized(&source, &registry, &anchor).await?;
        let final_block_id = registry
            .read()
            .expect("registry lock")
            .lez
            .final_reader()
            .lez_block_id;
        if final_block_id < anchor.block_id() {
            bail!(
                "the LEZ Sequencer did not serve the history up to LEZ block {}: start again",
                anchor.block_id()
            );
        }
        registry.write().expect("registry lock").save()?;
    }
    let mut lez_sdk = ZoneSequencer::init_with_config(
        lez_channel_id,
        reader_key(),
        node.clone(),
        SequencerConfig::new(unfunded()?),
        Some(lez_checkpoint),
    );

    {
        let registry = registry.read().expect("registry lock");
        info!(
            "RLN Membership Zone {}",
            if indexer { "indexer" } else { "sequencer" }
        );
        info!("node: {node_url}");
        info!(
            "channel: {} ({})",
            hex::encode(channel_id.as_ref()),
            args.channel_id.as_deref().map_or_else(
                || format!("sha256 of {:?}", args.channel_name),
                |_| "given".into()
            )
        );
        info!("tree_id: {}", hex::encode(tree_id));
        info!("lez: {lez_source}");
        info!(
            "lez: finalized tree at LEZ block {}, head at {}",
            registry.lez.final_reader().lez_block_id,
            registry.head().lez_block_id
        );
        info!("http: http://{}", args.http_addr);
        info!(
            "data: {} ({})",
            args.data_dir.display(),
            registry
                .state_sizes()
                .iter()
                .map(|(name, size)| format!("{name} {} KB", size / 1024))
                .collect::<Vec<_>>()
                .join(", ")
        );
        info!(
            "channel: {} final (final msg {}), {} in view, covered to LEZ block {}, first \
             block {}{}",
            registry.zone.final_entries().len(),
            short(&registry.zone.final_msg().into()),
            registry.zone.view().len(),
            registry.covered_to(),
            registry.first_lez_block_id,
            if resumed { " (checkpoint resumed)" } else { "" }
        );
        if let Some(stall) = registry.zone.stall() {
            warn!(
                "the final tier is frozen at batch msg {} (Bedrock slot {}): {}; {} \
                 batch(es) parked after it",
                short(&stall.msg),
                stall.l1_slot,
                stall.error,
                stall.orphans_since
            );
        }
    }
    if let Some(msg) = registry.read().expect("registry lock").in_flight() {
        info!(
            "batch msg {} still in flight from the previous run; no new batch until it lands",
            short(&msg)
        );
    }
    info!("waiting for the Zone SDK to be ready...");

    let zone_channel = ZoneChannel {
        node_url: node_url.clone(),
        id: channel_id,
    };
    let mut ready = false;
    let mut lez_ready = false;
    let mut counters = TickCounters {
        zone_block_unix: now_unix(),
        lez_block_unix: now_unix(),
        ..TickCounters::default()
    };
    let channel_view = zone_sdk.subscribe_channel_view();
    let mut last_publish_unix = now_unix();
    let mut tick = tokio::time::interval(Duration::from_secs(args.poll_sec.max(1)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut finality_tick = tokio::time::interval(Duration::from_secs(FINALITY_SCAN_SEC));
    finality_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The L1 watchdog runs from the start, before the SDK is ready: a node that produces no
    // block never makes it ready.
    let mut watchdog_tick = tokio::time::interval(Duration::from_secs(args.poll_sec.max(1)));
    watchdog_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // The store is closed whatever ends the loop: Ctrl-C, or an error out of an arm.
    let outcome: Result<()> = async {
        loop {
            // rustfmt does not reach into `select!`: every arm is one call, the work is in
            // `zone_sync::on_*` and `lez_sync::on_*`.
            tokio::select! {
                event = zone_sdk.next_event() => match event {
                    Event::Ready => {
                        if !ready {
                            info!("ready");
                        }
                        ready = true;
                    }
                    Event::BlocksProcessed { checkpoint, channel_update, finalized, .. } => {
                        counters.zone_block_unix = now_unix();
                        let tip: [u8; 32] = channel_view.borrow().tip_message.into();
                        on_blocks_processed(
                            &mut zone_sdk,
                            &registry,
                            &zone_channel,
                            ZoneBlocks { tip, checkpoint: &checkpoint, channel_update: &channel_update, finalized: &finalized },
                            &mut counters.lineage_gap,
                        )
                        .await?;
                    }
                    _ => {}
                },
                lez_event = lez_sdk.next_event() => match lez_event {
                    Event::Ready => {
                        if !lez_ready {
                            info!("LEZ channel ready");
                        }
                        lez_ready = true;
                    }
                    Event::BlocksProcessed { channel_update, finalized, .. } => {
                        counters.lez_block_unix = now_unix();
                        on_lez_blocks_processed(
                            &source,
                            &registry,
                            &channel_update,
                            &finalized,
                        )
                        .await?;
                    }
                    _ => {}
                },
                _ = tick.tick(), if ready && lez_ready => {
                    on_tick(
                        &mut zone_sdk,
                        &registry,
                        &source,
                        args.heartbeat_sec,
                        &mut counters,
                        &mut last_publish_unix,
                    )
                    .await?;
                }
                _ = watchdog_tick.tick() => watch_l1(&node, &mut counters).await,
                _ = finality_tick.tick(), if ready => {
                    match reconcile_finality(&node, channel_id, &registry).await {
                        Ok(0) => {}
                        Ok(n) => info!("finality scan: {n} entry(ies) the stream had missed"),
                        Err(e) => warn!("finality scan failed: {e}"),
                    }
                }
                _ = tokio::signal::ctrl_c() => break,
            }
        }
        Ok(())
    }
    .await;
    let closed = registry.write().expect("registry lock").close();
    outcome.and(closed)?;
    info!("stopped; state in {}", args.data_dir.display());
    Ok(())
}
