//! Verify an RLN proof (`generate_proof`'s file) against the valid-root window the Zone
//! node serves: its `GET /roots/{window_size}` answer, the newest roots with the batches that
//! carried them to Bedrock, handed in as JSON (`--roots`, `ZONE_ROOTS`) or fetched from
//! `--zone-url`. This is the validator side of a Module: the proof is valid when the zkSNARK
//! verifies and its root is one of the window. A root counts once a batch carries it; with
//! `--finalized-only`, once that batch is finalized on Bedrock.
//!
//! ```bash
//! ZONE_ROOTS=$(curl -s localhost:3000/roots/5) \
//!     cargo run --bin verify_proof -- --proof proof.json [--finalized-only]
//! ```
//!
//! Exit code 0 when the proof verifies, 1 when it does not, 2 on an error.

use std::{fs, path::PathBuf, process::ExitCode};

use anyhow::{Context as _, Result, anyhow, bail};
use clap::Parser;
use rln::prelude::{
    CanonicalDeserialize, CanonicalSerialize, Fr, Hasher, PoseidonHash, Proof, RLNBuilder,
    RLNProofValues, default_graph_single, default_zkey_single, hash_to_field_le,
};
use serde::Deserialize;

#[derive(Parser)]
#[command(about = "Verify an RLN proof against the roots a Zone node serves")]
struct Args {
    /// The Zone's `/roots/{window_size}` answer, as JSON; fetched from `zone_url` when absent.
    #[arg(long, env = "ZONE_ROOTS", hide_env_values = true)]
    roots: Option<String>,
    /// A Zone node's HTTP API (sequencer or indexer), asked when no `roots` are given.
    #[arg(long, default_value = "http://127.0.0.1:3000", env = "ZONE_URL")]
    zone_url: String,
    /// The proof file `generate_proof` wrote.
    #[arg(long, default_value = "proof.json")]
    proof: PathBuf,
    /// How many of the newest roots count as valid, when they are fetched.
    #[arg(long, default_value_t = 5)]
    window_size: usize,
    /// Count only the roots whose batch is finalized on Bedrock (hours behind); otherwise every
    /// root a batch carries counts, finalized or not. The LEZ head's root (in no batch yet) never
    /// counts.
    #[arg(long)]
    finalized_only: bool,
}

/// The fields of `generate_proof`'s file the verifier reads.
#[derive(Deserialize)]
struct ProofFile {
    signal: String,
    epoch: String,
    rln_identifier: String,
    zone_root: String,
    proof: String,
    proof_values: String,
}

/// The Zone's `/roots/{window_size}` answer, the fields read.
#[derive(Deserialize)]
struct Roots {
    head: Head,
    roots: Vec<RootEntry>,
}

#[derive(Deserialize)]
struct Head {
    lez_block_id: u64,
}

#[derive(Deserialize)]
struct RootEntry {
    root: String,
    to_lez_block_id: u64,
    /// The batches that carried the root, the producing one first.
    batches: Vec<BatchEntry>,
}

#[derive(Deserialize)]
struct BatchEntry {
    #[serde(rename = "finalized")]
    is_finalized: bool,
}

impl RootEntry {
    /// Whether the root counts: a batch carries it, finalized when asked.
    fn counts(&self, finalized_only: bool) -> bool {
        self.batches
            .first()
            .is_some_and(|b| b.is_finalized || !finalized_only)
    }

    fn describe(&self) -> &'static str {
        match self.batches.first() {
            None => "the LEZ head, in no batch yet",
            Some(b) if b.is_finalized => "finalized on Bedrock",
            Some(_) => "in a batch, not finalized yet",
        }
    }
}

fn fr_from_hex(hex: &str, what: &str) -> Result<Fr> {
    let bytes =
        hex::decode(hex.trim_start_matches("0x")).with_context(|| format!("{what}: not hex"))?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow!("{what}: not 32 bytes"))?;
    Fr::deserialize_compressed(&bytes[..]).with_context(|| format!("{what}: not a field element"))
}

fn fr_to_hex(fr: &Fr) -> String {
    let mut out = [0u8; 32];
    fr.serialize_compressed(&mut out[..])
        .expect("an Fr is 32 bytes");
    hex::encode(out)
}

async fn run(args: &Args) -> Result<bool> {
    let file: ProofFile = serde_json::from_slice(
        &fs::read(&args.proof).with_context(|| format!("read {}", args.proof.display()))?,
    )
    .context("proof file")?;
    let proof = Proof::deserialize_compressed(
        hex::decode(&file.proof)
            .context("proof: not hex")?
            .as_slice(),
    )
    .context("proof")?;
    let values = RLNProofValues::deserialize_compressed(
        hex::decode(&file.proof_values)
            .context("proof values: not hex")?
            .as_slice(),
    )
    .context("proof values")?;

    // What the verifier recomputes itself from the public inputs it was given.
    let x = hash_to_field_le(file.signal.as_bytes());
    let external_nullifier = Hasher::<PoseidonHash>::hash_pair(
        hash_to_field_le(file.epoch.as_bytes()),
        hash_to_field_le(file.rln_identifier.as_bytes()),
    );
    if values.external_nullifier() != external_nullifier {
        println!("the proof's external nullifier is not the one of this epoch and identifier");
        return Ok(false);
    }

    let window: Roots = match &args.roots {
        Some(json) => serde_json::from_str(json).context("roots JSON")?,
        None => {
            let url = format!(
                "{}/roots/{}",
                args.zone_url.trim_end_matches('/'),
                args.window_size
            );
            let response = reqwest::get(&url)
                .await
                .with_context(|| format!("GET {url}"))?;
            if !response.status().is_success() {
                bail!(
                    "{url}: {} {}",
                    response.status(),
                    response.text().await.unwrap_or_default()
                );
            }
            response
                .json()
                .await
                .with_context(|| format!("{url}: unexpected answer"))?
        }
    };
    let mut valid_roots = vec![];
    for entry in window
        .roots
        .iter()
        .filter(|e| e.counts(args.finalized_only))
    {
        valid_roots.push(fr_from_hex(&entry.root, "root")?);
    }
    println!(
        "zone at LEZ block {}: {} root(s) in the window, {} carried by a batch{}",
        window.head.lez_block_id,
        window.roots.len(),
        valid_roots.len(),
        if args.finalized_only {
            " finalized on Bedrock"
        } else {
            ""
        }
    );
    // Where the proof's root stands, said before the outcome so a refusal reads with its cause.
    let root = fr_to_hex(&values.root());
    match window.roots.iter().find(|e| e.root == root) {
        Some(entry) => {
            println!(
                "proof root {root} = the Zone's root to LEZ block {} ({})",
                entry.to_lez_block_id,
                entry.describe()
            );
            if !entry.counts(args.finalized_only) {
                println!(
                    "that root is not offered to the verifier{}, so the proof is refused until \
                     its batch gets there",
                    if args.finalized_only {
                        " (--finalized-only)"
                    } else {
                        ""
                    }
                );
            }
        }
        None => println!(
            "proof root {root} is not in the window (taken at the Zone's root {})",
            file.zone_root
        ),
    }

    let rln = RLNBuilder::stateless()
        .graph(default_graph_single().clone())
        .zkey(default_zkey_single().clone())
        .build();
    // `Ok(false)` is the zkSNARK's outcome; `Err` is a public input that does not match the
    // proof (the signal, the roots), a refusal for a validator all the same.
    let verified = match rln.verify_with_roots(&proof, &values, &x, &valid_roots) {
        Ok(verified) => verified,
        Err(e) => {
            println!("the proof does not match its public inputs: {e}");
            false
        }
    };
    println!("VERIFIED={verified}");
    Ok(verified)
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    match run(&args).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}
