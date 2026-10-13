//! End-to-end demo of the RLN Membership Zone RFC (`docs/rfc-rln-membership-zone.md`).
//!
//! One process plays every role, named as in the RFC:
//! - a mock LEZ with the registration program (`lez.rs`), producing one block per tick;
//! - a reader (`reader.rs`): the derivation rules and the tree, shared by the three roles below;
//! - the Zone sequencer (`sequencer.rs`): builds one batch (`batch.rs`) per tick from the LEZ
//!   blocks not covered yet and publishes it on the real local chain through the Zone SDK;
//! - a Zone indexer (`indexer.rs`): checks every finalized batch against LEZ;
//! - two Modules (`module.rs`), alice's and bob's: apply every LEZ block to their own tree,
//!   report their membership status and compare their roots with the verified batches.
//!
//! The scripted scenario: alice and bob register, bob double-signals and is slashed by carol,
//! alice extends, expires and erases. `--dishonest-sequencer` makes the Zone sequencer omit
//! bob's slash from its tree and its batch: the Zone indexer marks the batch Rejected (an
//! omission is provable censorship) and every Module, which reads LEZ itself, sees its own root
//! diverge from the batch.

mod batch;
mod indexer;
mod lez;
mod module;
mod reader;
mod sequencer;

use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use clap::Parser;
use lb_common_http_client::BasicAuthCredentials;
use lb_core::mantle::{
    gas::GasCost,
    ops::channel::{ChannelId, MsgId},
};
use lb_key_management_system_service::keys::{ED25519_SECRET_KEY_SIZE, Ed25519Key, ZkPublicKey};
use lb_zone_sdk::{
    CommonHttpClient,
    adapter::NodeHttpClient,
    sequencer::{
        Error as SeqError, Event, FinalizedOp, FundingConfig, ZoneSequencer as SdkSequencer,
    },
};
use prost::Message;
use rand::{RngCore, rngs::ThreadRng, thread_rng};
use reqwest::Url;
use rln::prelude::{IdentityKeys, PoseidonHash};

use crate::{
    batch::Batch,
    indexer::ZoneIndexer,
    lez::{Action, ConfigState, MockLez},
    module::Module,
    reader::short,
    sequencer::ZoneSequencer,
};

/// LEZ timestamp advance per mock block.
const BLOCK_MS: u64 = 2_000;
/// Last scripted LEZ block; the demo ends once the Zone indexer has verified it.
const LAST_BLOCK: u64 = 13;

#[derive(Parser, Debug)]
#[command(about = "RLN Membership Zone end-to-end demo")]
struct Args {
    /// Logos Blockchain node HTTP endpoint (devnet/run.sh start).
    #[arg(long, default_value = "http://127.0.0.1:8080", env = "NODE_URL")]
    node_url: String,
    #[arg(long, env = "NODE_AUTH_USER")]
    node_auth_user: Option<String>,
    #[arg(long, env = "NODE_AUTH_PASSWORD")]
    node_auth_password: Option<String>,
    /// Node wallet key paying the Mantle Transaction fees (devnet/run.sh fund).
    #[arg(long, env = "FUNDING_PK")]
    funding_pk: String,
    #[arg(long, default_value_t = 1_000_000, env = "MAX_TX_FEE")]
    max_tx_fee: u64,
    /// Seconds between ticks (one LEZ block and one batch per tick).
    #[arg(long, default_value_t = 3)]
    tick_sec: u64,
    /// Play a Zone sequencer that omits the slash from its tree and its batch.
    #[arg(long)]
    dishonest_sequencer: bool,
}

/// One scripted registration-program action.
enum Step {
    Register {
        who: &'static str,
        rate_limit: u64,
        active_duration_ms: u64,
    },
    Extend {
        who: &'static str,
        extra_ms: u64,
    },
    Slash {
        who: &'static str,
        by: &'static str,
    },
    Erase {
        who: &'static str,
    },
}

fn script(block_id: u64) -> Vec<Step> {
    match block_id {
        1 => vec![
            Step::Register {
                who: "alice",
                rate_limit: 100,
                active_duration_ms: 12_000,
            },
            Step::Register {
                who: "bob",
                rate_limit: 300,
                active_duration_ms: 30_000,
            },
        ],
        4 => vec![Step::Slash {
            who: "bob",
            by: "carol",
        }],
        // Extend is accepted only in the grace period: alice's expires at 12000 ms, grace 4000 ms.
        5 => vec![Step::Extend {
            who: "alice",
            extra_ms: 4_000,
        }],
        12 => vec![Step::Erase { who: "alice" }],
        _ => vec![],
    }
}

/// Build the registration-program action for a scripted step.
fn to_action(step: &Step, modules: &[Module], lez: &MockLez) -> Action {
    let module = |name: &str| {
        modules
            .iter()
            .find(|m| m.name == name)
            .expect("scripted member")
    };
    match step {
        Step::Register {
            who,
            rate_limit,
            active_duration_ms,
        } => Action::Register {
            payer: (*who).into(),
            id_commitment: module(who).id_commitment(),
            rate_limit: *rate_limit,
            expiry_timestamp_ms: lez.now() + active_duration_ms,
            leaf_index: 0,
        },
        Step::Extend { who, extra_ms } => {
            let id_commitment = module(who).id_commitment();
            let expiry = lez
                .membership(&id_commitment)
                .map_or(0, |m| m.expiry_timestamp_ms);
            Action::Extend {
                holder: (*who).into(),
                id_commitment,
                expiry_timestamp_ms: expiry + extra_ms,
            }
        }
        // In reality the slasher learns the secret from `validate_proof`
        // (PROOF_RATE_LIMIT_VIOLATION, `recovered_secret`); here we take it directly.
        Step::Slash { who, by } => Action::Slash {
            slasher: (*by).into(),
            identity_secret: *module(who).keys.identity_secret(),
        },
        Step::Erase { who } => Action::Erase {
            holder: (*who).into(),
            id_commitment: module(who).id_commitment(),
        },
    }
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

/// One line per Module: its leaf in its own tree and its membership status.
fn print_modules(modules: &[Module], lez: &MockLez) {
    for m in modules {
        let leaf = m
            .reader
            .leaf_index(&m.id_commitment())
            .map_or("-".into(), |i| i.to_string());
        println!(
            "  module {:5} {}.. leaf {:>2}  {}  (applied LEZ block {}, root {}..)",
            m.name,
            short(&m.id_commitment()),
            leaf,
            m.status(lez),
            m.reader.applied_block_id,
            short(&m.reader.root())
        );
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let args = Args::parse();

    // Fresh channel per run: the mock LEZ lives only in this process.
    let mut key_bytes = [0u8; ED25519_SECRET_KEY_SIZE];
    thread_rng().fill_bytes(&mut key_bytes);
    let signing_key = Ed25519Key::from_bytes(&key_bytes);
    let channel_id = ChannelId::from(signing_key.public_key().to_bytes());

    let funding = FundingConfig {
        funding_pk: parse_funding_pk(&args.funding_pk)?,
        change_pk: None,
        max_tx_fee: GasCost::new(args.max_tx_fee),
        priority_fee_percent: FundingConfig::DEFAULT_PRIORITY_FEE_PERCENT,
    };
    let auth = args
        .node_auth_user
        .map(|u| BasicAuthCredentials::new(u, args.node_auth_password));
    let node_url: Url = args.node_url.parse().context("invalid NODE_URL")?;
    let node = NodeHttpClient::new(CommonHttpClient::new(auth), node_url.clone());
    let mut sdk = SdkSequencer::init(channel_id, signing_key, node, funding, None);

    let mut modules: Vec<Module> = ["alice", "bob"]
        .into_iter()
        .map(|name| {
            Module::new(
                name,
                IdentityKeys::generate::<PoseidonHash, ThreadRng>(&mut thread_rng()),
            )
        })
        .collect();
    let config = ConfigState {
        min_rate_limit: 100,
        max_rate_limit: 600,
        price_per_unit: 10,
        registration_fee: 5,
        min_active_duration_ms: 4_000,
        max_active_duration_ms: 60_000,
        grace_period_duration_ms: 4_000,
        withdrawal_delay_ms: 6_000,
        treasury_account_id: "treasury".into(),
        operator_account_id: "operator".into(),
    };
    let mut lez = MockLez::new(config, &[("alice", 10_000), ("bob", 10_000)]);
    let mut zone_sequencer = ZoneSequencer::new(args.dishonest_sequencer);
    let mut indexer = ZoneIndexer::new();

    println!("RLN Membership Zone demo");
    println!("  node     {node_url}");
    println!("  channel  {}", hex::encode(channel_id.as_ref()));
    println!(
        "  modules  {}",
        modules
            .iter()
            .map(|m| format!("{} {}..", m.name, short(&m.id_commitment())))
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("  dishonest sequencer {}", args.dishonest_sequencer);
    println!("waiting for the Zone sequencer to be ready...");

    let mut ready = false;
    // The batch waiting to land in a block, if any.
    let mut in_flight: Option<MsgId> = None;
    let mut tick = tokio::time::interval(Duration::from_secs(args.tick_sec));
    // Backfilling a fresh channel takes a while; do not fire the missed ticks in a burst.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            event = sdk.next_event() => match event {
                Event::Ready => {
                    if !ready { println!("ready\n"); }
                    ready = true;
                }
                Event::BlocksProcessed { channel_update, finalized, .. } => {
                    // Our batch landed in a block: its fee note is spent, the next batch may go.
                    if let Some(pending) = in_flight
                        && channel_update.adopted().iter().any(|tx| tx.inscription().is_some_and(|i| i.this_msg == pending))
                    {
                        println!("zone sequencer: batch adopted in a block");
                        in_flight = None;
                    }
                    for op in finalized.iter().flat_map(|tx| tx.ops.iter()) {
                        let FinalizedOp::Inscription(info) = op else { continue };
                        // Blocks the SDK ingests through its backfill path report an empty
                        // `channel_update`, so finality is the fallback signal that our batch landed.
                        if in_flight == Some(info.this_msg) {
                            in_flight = None;
                        }
                        // The Zone indexer checks the batch against LEZ and says what is wrong;
                        // every Module reads the same batch and only compares its own root.
                        match indexer.on_batch(&info.payload) {
                            Ok(batch) => println!("zone indexer: batch [{}..{}] Verified, root {}..", batch.from_lez_block_id, batch.to_lez_block_id, hex::encode(&batch.root_after[..4])),
                            Err(reason) => println!("zone indexer: batch REJECTED: {reason}"),
                        }
                        let Ok(batch) = Batch::decode(info.payload.as_slice()) else { continue };
                        for m in &modules {
                            match m.compare(&batch) {
                                Ok(Some(())) => println!("  module {:5} root at LEZ block {} matches the batch", m.name, batch.to_lez_block_id),
                                Ok(None) => println!("  module {:5} has not applied LEZ block {} yet", m.name, batch.to_lez_block_id),
                                Err(reason) => println!("  module {:5} DIVERGES from the Zone: {reason}", m.name),
                            }
                        }
                    }
                }
                _ => {}
            },
            _ = tick.tick(), if ready => {
                // 1. LEZ: run this block's scripted actions, then close the block.
                if lez.last_block_id() < LAST_BLOCK {
                    let block_id = lez.last_block_id() + 1;
                    for step in script(block_id) {
                        let action = to_action(&step, &modules, &lez);
                        let label = format!("{action:?}").split_whitespace().next().unwrap_or("").to_owned();
                        match lez.submit(action) {
                            Ok(()) => println!("LEZ block {block_id}: {label} ok"),
                            Err(e) => println!("LEZ block {block_id}: {label} REVERTED: {e}"),
                        }
                    }
                    let block = lez.produce_block(BLOCK_MS).clone();
                    // 2. Every reader applies the closed block to its own tree.
                    indexer.apply_block(&block);
                    let before: Vec<_> = modules.iter().map(|m| m.status(&lez)).collect();
                    for m in &mut modules {
                        m.apply_block(&block);
                    }
                    let after: Vec<_> = modules.iter().map(|m| m.status(&lez)).collect();
                    if before != after || !block.actions.is_empty() {
                        print_modules(&modules, &lez);
                    }
                }
                // 3. Zone sequencer: one batch covering the LEZ blocks not covered yet, with at
                //    most one batch in flight (each in-flight transaction holds one fee note).
                if in_flight.is_none() && zone_sequencer.covered_block_id() < lez.last_block_id() {
                    let (batch, inscription, next) = zone_sequencer.build_batch(&lez)?;
                    match sdk.handle().publish(inscription).await {
                        Ok((result, _checkpoint)) => {
                            in_flight = Some(result.tx.inscription().this_msg);
                            let described: Vec<_> = batch.changes.iter().map(|c| c.describe()).collect();
                            println!("zone sequencer: batch [{}..{}] published: {}", batch.from_lez_block_id, batch.to_lez_block_id, if described.is_empty() { "no changes".into() } else { described.join("; ") });
                            zone_sequencer.commit(next);
                        }
                        Err(SeqError::Unavailable { reason }) => println!("zone sequencer: not ready ({reason}), retry next tick"),
                        Err(e) => return Err(anyhow!("publish failed: {e}")),
                    }
                }
            },
            _ = tokio::signal::ctrl_c() => break,
        }
        // The demo stops at the first rejection: the dishonest sequencer never publishes a
        // correct batch, so the Zone indexer would wait forever.
        if indexer.verified_to >= LAST_BLOCK || !indexer.rejected().is_empty() {
            break;
        }
    }

    println!(
        "\nfinal state (Zone indexer: {} batches Verified, up to LEZ block {}):",
        indexer.verified, indexer.verified_to
    );
    print_modules(&modules, &lez);
    println!(
        "LEZ balances: alice {}, bob {}, carol {}, treasury {}, operator {}",
        lez.balance("alice"),
        lez.balance("bob"),
        lez.balance("carol"),
        lez.balance("treasury"),
        lez.balance("operator")
    );
    for reason in indexer.rejected() {
        println!("zone indexer rejected a batch and waits for a correct one: {reason}");
    }
    Ok(())
}
