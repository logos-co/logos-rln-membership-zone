//! End-to-end demo of the RLN Membership Zone RFC (`docs/rfc-rln-membership-zone.md`).
//!
//! One process plays every role:
//! - a mock LEZ with the registration program (`lez.rs`), producing one block per tick;
//! - the Zone sequencer: derives changes from new LEZ blocks (`membership_set.rs`) and publishes
//!   one batch (`batch.rs`) per tick on the real local chain through the Zone SDK;
//! - a Zone indexer (`indexer.rs`): re-checks every finalized batch against LEZ and keeps the
//!   registry view a Module would read.
//!
//! The scripted scenario: alice and bob register, alice extends, bob double-signals and is
//! slashed by carol, alice expires and erases. `--censor` makes the Zone sequencer drop the
//! slash from its batch, which the Zone indexer detects (Invariant 3).

mod batch;
mod indexer;
mod lez;
mod membership_set;

use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use clap::Parser;
use lb_common_http_client::BasicAuthCredentials;
use lb_core::mantle::{
    gas::GasCost,
    ops::channel::{ChannelId, MsgId, inscribe::Inscription},
};
use lb_key_management_system_service::keys::{ED25519_SECRET_KEY_SIZE, Ed25519Key, ZkPublicKey};
use lb_zone_sdk::{
    CommonHttpClient,
    adapter::NodeHttpClient,
    sequencer::{Error as SeqError, Event, FinalizedOp, FundingConfig, ZoneSequencer},
};
use prost::Message;
use rand::{RngCore, rngs::ThreadRng, thread_rng};
use reqwest::Url;
use rln::prelude::{Fr, IdentityKeys, PoseidonHash};

use crate::{
    batch::Batch,
    indexer::ZoneIndexer,
    lez::{ConfigState, Instruction, MockLez},
    membership_set::{MembershipSet, fr_to_bytes_le, short},
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
    tick_secs: u64,
    /// Make the Zone sequencer omit the slash from its batch.
    #[arg(long)]
    censor: bool,
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
        // Extend is accepted only in the grace period: alice's expires at 12000 ms, grace 4000 ms.
        5 => vec![Step::Extend {
            who: "alice",
            extra_ms: 4_000,
        }],
        4 => vec![Step::Slash {
            who: "bob",
            by: "carol",
        }],
        12 => vec![Step::Erase { who: "alice" }],
        _ => vec![],
    }
}

struct Member {
    name: &'static str,
    keys: IdentityKeys,
}

impl Member {
    fn id_commitment(&self) -> Fr {
        self.keys.id_commitment()
    }
}

/// Build the registration-program instruction for a scripted step.
fn to_instruction(step: &Step, members: &[Member], lez: &MockLez) -> Instruction {
    let member = |name: &str| {
        members
            .iter()
            .find(|m| m.name == name)
            .expect("scripted member")
    };
    match step {
        Step::Register {
            who,
            rate_limit,
            active_duration_ms,
        } => Instruction::Register {
            payer: (*who).into(),
            id_commitment: member(who).id_commitment(),
            rate_limit: *rate_limit,
            expiry_timestamp_ms: lez.now() + active_duration_ms,
            leaf_index: 0,
        },
        Step::Extend { who, extra_ms } => {
            let id_commitment = member(who).id_commitment();
            let expiry = lez
                .membership(&id_commitment)
                .map_or(0, |m| m.expiry_timestamp_ms);
            Instruction::Extend {
                holder: (*who).into(),
                id_commitment,
                expiry_timestamp_ms: expiry + extra_ms,
            }
        }
        // In reality the slasher learns the secret from `validate_proof`
        // (PROOF_RATE_LIMIT_VIOLATION, `recovered_secret`); here we take it directly.
        Step::Slash { who, by } => Instruction::Slash {
            slasher: (*by).into(),
            identity_secret: *member(who).keys.identity_secret(),
        },
        Step::Erase { who } => Instruction::Erase {
            holder: (*who).into(),
            id_commitment: member(who).id_commitment(),
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

/// Build the next batch from every LEZ block after `zone.lez_to`, ending early at a block
/// boundary when the encoded batch would not fit in one inscription (RFC "Cadence").
/// Returns the batch, its inscription and the Zone sequencer's view after it.
fn build_batch(
    zone: &MembershipSet,
    lez: &MockLez,
    censor: bool,
) -> Result<(Batch, Inscription, MembershipSet)> {
    let from = zone.lez_to + 1;
    let mut next = zone.clone();
    let mut changes = vec![];
    let mut fitted = None;
    for block in lez.blocks(from, lez.last_finalized_block_id()) {
        changes.extend(next.apply_block(block));
        if censor {
            changes.retain(|c| !c.is_slash());
        }
        let batch = Batch {
            lez_from: from,
            lez_to: block.block_id,
            changes: changes.clone(),
            root_after: fr_to_bytes_le(&next.root()).to_vec(),
        };
        match Inscription::try_from(batch.encode_to_vec()) {
            Ok(inscription) => fitted = Some((batch, inscription, next.clone())),
            Err(_) => break,
        }
    }
    fitted.ok_or_else(|| anyhow!("LEZ block {from} alone exceeds one inscription"))
}

fn print_view(indexer: &ZoneIndexer, members: &[Member], lez: &MockLez) {
    for m in members {
        let leaf = indexer
            .membership_set
            .leaf_index(&m.id_commitment())
            .map_or("-".into(), |i| i.to_string());
        println!(
            "      {:5} {}.. leaf {:>2}  {}",
            m.name,
            short(&m.id_commitment()),
            leaf,
            indexer.membership_set.status(&m.id_commitment(), lez)
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
    let mut sequencer = ZoneSequencer::init(channel_id, signing_key, node, funding, None);

    let members: Vec<Member> = ["alice", "bob"]
        .into_iter()
        .map(|name| Member {
            name,
            keys: IdentityKeys::generate::<PoseidonHash, ThreadRng>(&mut thread_rng()),
        })
        .collect();
    let config = ConfigState {
        min_rate_limit: 100,
        max_rate_limit: 600,
        price_per_unit: 10,
        reg_fee: 5,
        min_active_duration_ms: 4_000,
        max_active_duration_ms: 60_000,
        grace_period_duration_ms: 4_000,
        withdrawal_delay_ms: 6_000,
        treasury_account_id: "treasury".into(),
        operator_account_id: "operator".into(),
    };
    let mut lez = MockLez::new(config, &[("alice", 10_000), ("bob", 10_000)]);
    let mut zone = MembershipSet::new(); // the Zone sequencer's own view
    let mut indexer = ZoneIndexer::new();

    println!("RLN Membership Zone demo");
    println!("  node     {node_url}");
    println!("  channel  {}", hex::encode(channel_id.as_ref()));
    println!(
        "  members  {}",
        members
            .iter()
            .map(|m| format!("{} {}..", m.name, short(&m.id_commitment())))
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("  censor   {}", args.censor);
    println!("waiting for the Zone sequencer to be ready...");

    let mut ready = false;
    // The batch waiting to land in a block, if any.
    let mut in_flight: Option<MsgId> = None;
    let mut tick = tokio::time::interval(Duration::from_secs(args.tick_secs));
    // Backfilling a fresh channel takes a while; do not fire the missed ticks in a burst.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            event = sequencer.next_event() => match event {
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
                        match indexer.on_batch(&info.payload, &lez) {
                            Ok(b) => {
                                println!("  zone indexer: finalized batch [{}..{}] verified, root {}..", b.lez_from, b.lez_to, hex::encode(&b.root_after[..4]));
                                print_view(&indexer, &members, &lez);
                            }
                            Err(reason) => println!("  zone indexer: REJECTED: {reason}"),
                        }
                    }
                }
                _ => {}
            },
            _ = tick.tick(), if ready => {
                // 1. LEZ: run this block's scripted transactions, then close the block.
                if lez.last_finalized_block_id() < LAST_BLOCK {
                    let block_id = lez.last_finalized_block_id() + 1;
                    for step in script(block_id) {
                        let instruction = to_instruction(&step, &members, &lez);
                        let label = format!("{instruction:?}").split_whitespace().next().unwrap_or("").to_owned();
                        match lez.submit(instruction) {
                            Ok(()) => println!("LEZ block {block_id}: {label} ok"),
                            Err(e) => println!("LEZ block {block_id}: {label} REVERTED: {e}"),
                        }
                    }
                    lez.produce_block(BLOCK_MS);
                }
                // 2. Zone sequencer: one batch covering the LEZ blocks not covered yet, with at
                //    most one batch in flight (each in-flight transaction holds one fee note).
                if in_flight.is_none() && zone.lez_to < lez.last_finalized_block_id() {
                    let (batch, inscription, next) = build_batch(&zone, &lez, args.censor)?;
                    match sequencer.handle().publish(inscription).await {
                        Ok((result, _checkpoint)) => {
                            in_flight = Some(result.tx.inscription().this_msg);
                            let described: Vec<_> = batch.changes.iter().map(|c| c.describe()).collect();
                            println!("zone sequencer: batch [{}..{}] published: {}", batch.lez_from, batch.lez_to, if described.is_empty() { "no changes".into() } else { described.join("; ") });
                            zone = next;
                        }
                        Err(SeqError::Unavailable { reason }) => println!("zone sequencer: not ready ({reason}), retry next tick"),
                        Err(e) => return Err(anyhow!("publish failed: {e}")),
                    }
                }
            },
            _ = tokio::signal::ctrl_c() => break,
        }
        if indexer.membership_set.lez_to >= LAST_BLOCK || indexer.halted().is_some() {
            break;
        }
    }

    println!(
        "\nfinal registry view (Zone indexer, {} batches verified):",
        indexer.verified
    );
    print_view(&indexer, &members, &lez);
    println!(
        "LEZ balances: alice {}, bob {}, carol {}, treasury {}, operator {}",
        lez.balance("alice"),
        lez.balance("bob"),
        lez.balance("carol"),
        lez.balance("treasury"),
        lez.balance("operator")
    );
    if let Some(reason) = indexer.halted() {
        println!("zone indexer halted: {reason}");
    }
    Ok(())
}
