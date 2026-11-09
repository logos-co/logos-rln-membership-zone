//! Generate an RLN proof for a membership a Zone node serves: the Merkle proof path is
//! the Zone's `GET /merkle_proof/{id_commitment}` answer, handed in as JSON (`--merkle-proof`,
//! `ZONE_MERKLE_PROOF`) or fetched from `--zone-url`; the identity secret comes from the caller
//! (`register_member` prints it as `IDENTITY_SECRET_HASH`); the proof goes to a JSON file for
//! `verify_proof`. This is what a Module does with the Zone's answer, minus the libp2p hop.
//!
//! ```bash
//! ZONE_MERKLE_PROOF=$(curl -s localhost:3000/merkle_proof/<id_commitment>) \
//! RLN_IDENTITY_SECRET=<64 hex> RLN_ID_COMMITMENT=<64 hex> \
//!     cargo run --bin generate_proof -- --out proof.json
//! ```

use std::{fs, path::PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use clap::Parser;
use rln::prelude::{
    CanonicalDeserialize, CanonicalSerialize, Fr, Hasher, PoseidonHash, RLNBuilder, RLNMerkleProof,
    RLNWitnessInput, SecretFr, default_graph_single, default_zkey_single, hash_to_field_le,
};
use serde::{Deserialize, Serialize};

#[derive(Parser)]
#[command(about = "RLN proof for a membership a Zone node serves")]
struct Args {
    /// The Zone's `/merkle_proof/{id_commitment}` answer, as JSON; fetched from `zone_url`
    /// when absent.
    #[arg(long, env = "ZONE_MERKLE_PROOF", hide_env_values = true)]
    merkle_proof: Option<String>,
    /// A Zone node's HTTP API (sequencer or indexer), asked when no `merkle_proof` is given.
    #[arg(long, default_value = "http://127.0.0.1:3000", env = "ZONE_URL")]
    zone_url: String,
    /// The identity secret, 32 bytes little-endian in hex (`IDENTITY_SECRET_HASH` of
    /// `register_member`).
    #[arg(long, env = "RLN_IDENTITY_SECRET")]
    identity_secret: String,
    /// The identity commitment, 32 bytes little-endian in hex; must be `poseidon(secret)`.
    #[arg(long, env = "RLN_ID_COMMITMENT")]
    id_commitment: String,
    /// The membership's rate limit, the `user_message_limit` of the proof; the Zone's
    /// `rate_commitment` must be `poseidon(id_commitment, rate_limit)`.
    #[arg(long, default_value_t = 100)]
    rate_limit: u64,
    /// The signal the proof binds, hashed to the field (`x`).
    #[arg(long, default_value = "Hello, RLN Membership Zone!")]
    message: String,
    /// The epoch, hashed to the field and combined with the identifier into the external
    /// nullifier.
    #[arg(long, default_value = "1231028105")]
    epoch: String,
    #[arg(long, default_value = "rln/logos-rln-relay/v2.0.0")]
    rln_identifier: String,
    /// The message id within the epoch, below the rate limit.
    #[arg(long, default_value_t = 0)]
    message_id: u64,
    /// Where the proof goes, for `verify_proof`.
    #[arg(long, default_value = "proof.json")]
    out: PathBuf,
}

/// The Zone's `/merkle_proof/{id}` answer, the fields the proof needs.
#[derive(Deserialize)]
struct Membership {
    leaf_index: u64,
    active: bool,
    lez_block_id: u64,
    root: String,
    proof: Option<ProofPath>,
}

#[derive(Deserialize)]
struct ProofPath {
    rate_commitment: String,
    path_elements: Vec<String>,
    identity_path_index: Vec<u8>,
}

/// What `generate_proof` writes and `verify_proof` reads: the proof, its public values and the
/// inputs that produced them, every field element 32 bytes little-endian in hex.
#[derive(Serialize)]
struct ProofFile {
    message: String,
    epoch: String,
    rln_identifier: String,
    message_id: u64,
    rate_limit: u64,
    id_commitment: String,
    leaf_index: u64,
    /// The Zone's root the path was taken at, and the LEZ block.
    zone_root: String,
    zone_lez_block_id: u64,
    proof: String,
    proof_values: String,
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

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let mut identity_secret = fr_from_hex(&args.identity_secret, "identity secret")?;
    let id_commitment = fr_from_hex(&args.id_commitment, "id_commitment")?;
    if Hasher::<PoseidonHash>::hash_single(identity_secret) != id_commitment {
        bail!("the identity secret does not hash to the id_commitment");
    }

    let membership: Membership = match &args.merkle_proof {
        Some(json) => serde_json::from_str(json).context("merkle proof JSON")?,
        None => {
            let url = format!(
                "{}/merkle_proof/{}",
                args.zone_url.trim_end_matches('/'),
                args.id_commitment
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
    if !membership.active {
        bail!(
            "leaf {} is not active, nothing to prove",
            membership.leaf_index
        );
    }
    let path = membership
        .proof
        .ok_or_else(|| anyhow!("the membership is active but carries no proof path"))?;

    let user_message_limit = Fr::from(args.rate_limit);
    let rate_commitment = Hasher::<PoseidonHash>::hash_pair(id_commitment, user_message_limit);
    if fr_to_hex(&rate_commitment) != path.rate_commitment {
        bail!(
            "the Zone's rate_commitment is not poseidon(id_commitment, {}): another rate limit",
            args.rate_limit
        );
    }
    let path_elements = path
        .path_elements
        .iter()
        .map(|e| fr_from_hex(e, "path element"))
        .collect::<Result<Vec<_>>>()?;
    let merkle_proof = RLNMerkleProof::new(path_elements, path.identity_path_index);

    let x = hash_to_field_le(args.message.as_bytes());
    let external_nullifier = Hasher::<PoseidonHash>::hash_pair(
        hash_to_field_le(args.epoch.as_bytes()),
        hash_to_field_le(args.rln_identifier.as_bytes()),
    );
    let witness = RLNWitnessInput::new_single()
        .identity_secret(SecretFr::from(&mut identity_secret))
        .user_message_limit(user_message_limit)
        .merkle_proof(merkle_proof)
        .x(x)
        .external_nullifier(external_nullifier)
        .message_id(Fr::from(args.message_id))
        .build()
        .context("witness")?;

    let rln = RLNBuilder::stateless()
        .graph(default_graph_single().clone())
        .zkey(default_zkey_single().clone())
        .build();
    let (proof, values) = rln.generate_proof(&witness).context("generate the proof")?;
    if fr_to_hex(&values.root()) != membership.root {
        bail!(
            "the proof's root {} is not the Zone's root {}: the path does not match the tree",
            fr_to_hex(&values.root()),
            membership.root
        );
    }

    let mut proof_bytes = vec![];
    proof
        .serialize_compressed(&mut proof_bytes)
        .context("serialize the proof")?;
    let mut values_bytes = vec![];
    values
        .serialize_compressed(&mut values_bytes)
        .context("serialize the proof values")?;
    let file = ProofFile {
        message: args.message,
        epoch: args.epoch,
        rln_identifier: args.rln_identifier,
        message_id: args.message_id,
        rate_limit: args.rate_limit,
        id_commitment: args.id_commitment,
        leaf_index: membership.leaf_index,
        zone_root: membership.root,
        zone_lez_block_id: membership.lez_block_id,
        proof: hex::encode(proof_bytes),
        proof_values: hex::encode(values_bytes),
    };
    fs::write(&args.out, serde_json::to_vec_pretty(&file)?)
        .with_context(|| format!("write {}", args.out.display()))?;

    println!("LEAF_INDEX={}", file.leaf_index);
    println!("ROOT={}", file.zone_root);
    println!(
        "NULLIFIER={}",
        fr_to_hex(
            &values
                .nullifier()
                .expect("a single message-id proof has one nullifier")
        )
    );
    println!("PROOF_FILE={}", args.out.display());
    Ok(())
}
