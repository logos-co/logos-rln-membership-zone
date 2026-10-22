# logos-rln-membership-zone

A Logos Zone for RLN (Rate-Limiting Nullifiers) memberships,
with deposits in an LEZ registration program.

The membership set, the Merkle tree of rate commitments, is derived from a registration program
on the Logos Execution Zone (LEZ) by fixed, deterministic rules over the events the program emits:
anyone who reads them computes the same tree. A dedicated Zone computes it and publishes the roots
as batches on Bedrock; Zone indexers compute it too, check every batch, and serve the registry to
the RLN Modules. Deposits, refunds and slashing stay in the registration program.
The RLN Module that [RLN-API](https://lip.logos.co/anoncomms/raw/rln-api.html) specifies
(the Module for short), which runs in every user's node, holds no tree: it proves and validates
against the published roots and reads its proof path and membership state from a Zone indexer,
in its own node when it wants to trust nobody. RLN-API itself does not change; the registry fills
in its `logos` namespace binding.

[logos-lez-rln](https://github.com/logos-co/logos-lez-rln) keeps the tree in LEZ accounts
and hashes it inside the RISC Zero zkVM.
A registration there costs about 9.09M of the 10M execution gas,
so a LEZ block holds one registration and the tree is capped at 512 lifetime leaves.
In this design, registration is a cheap LEZ transaction
and the tree reaches depth 20, the depth Zerokit ships.

## Status

Early design. Nothing is deployed.

- **Specification**: [`docs/rfc-rln-membership-zone.md`](docs/rfc-rln-membership-zone.md),
  a raw LIP draft that ends with the open design questions.
- **Zone sequencer**: [`zone-sequencer/`](zone-sequencer) runs on the Logos testnet with an
  HTTP registry API. It reads the registration program's transactions from a LEZ Sequencer,
  derives the tree, publishes batches on its channel through the Zone SDK and follows them to
  finality. Setup, operation and troubleshooting: [`docs/setup-rln-membership-zone.md`](docs/setup-rln-membership-zone.md).
- **Not yet written**: the Zone indexer (a Zone sequencer without an accredited key that
  checks the batches), the event source once LEZ serves events live, and the Module side.

## How it works

```text
Module --Register / Extend / Erase / Slash--> LEZ registration program   (deposit, leaf index, events)
                                                   |
                                                   v  LEZ blocks + events, as the LEZ Sequencer closes them
                          Zone sequencer --batch--> Bedrock              (changes + root_after)
                                                   |
                                                   v  batches: Verified or Rejected
   Module (no tree) <--roots, proof path, state-- Zone indexer           (own node, or a service)
```

1. The Module sends `Register` to the registration program on LEZ.
   The program locks the deposit, assigns the leaf index and emits `Registered`.
2. Every reader (the Zone sequencer, every Zone indexer) applies the same derivation rules to
   the program's events, `Registered`, `Extended`, `Erased`, `Slashed`:
   `Insert` and `Remove`, as in the lez-rln Merkle program, plus expiry by the LEZ clock.
3. The Zone sequencer publishes the changes and the root as a
   `Batch{from_lez_block_id, to_lez_block_id, changes, root_after}` inscription.
   Every Zone indexer recomputes each batch from LEZ and rejects any that omits, invents or misorders a change;
   a Rejected batch's root is never a valid root.
4. The Module reads its registry access, a Zone indexer: the valid roots (the Verified batches
   from the last removal on), its Merkle proof path at the newest one, its membership state and
   the LEZ clock. It proves against the newest valid root and accepts proofs against any root in
   that window, as RLN-API specifies. A node that wants to trust nobody runs its own Zone
   indexer, synced from another one's snapshot checked against a batch and against LEZ.

The Zone holds no funds.
A Zone sequencer can omit a change, which is provable censorship,
but that only pauses the registry at the Zone indexers until a correct batch arrives.

## Repository layout

```text
docs/rfc-rln-membership-zone.md   the specification (raw LIP draft)
zone-sequencer/                   the Zone sequencer, with its HTTP registry API
  src/lez.rs                      LEZ blocks and the program's events, derived from its transactions
  src/reader.rs                   a reader: derivation rules over the events and the tree
  src/batch.rs                    batch wire format (protobuf)
  src/registry.rs                 published batches with their status on Bedrock, trees per batch, history file
  src/http.rs                     HTTP API: roots, Merkle proof path, leaf status
  src/main.rs                     Zone SDK loop: publish, follow to finality, poll LEZ
lez-sequencer/                    config template to run your own LEZ sequencer as the block source
docs/setup-rln-membership-zone.md setup, operation, monitoring, troubleshooting, cleanup
logos-blockchain/                 git submodule pinned to node release 0.3.2 (Zone SDK path dependencies)
```

## Quick start

### Prerequisites

- Rust, with the toolchain pinned in `rust-toolchain.toml`
  to the channel of the `logos-blockchain` submodule.
- Build tools the `logos-blockchain` crates need on Linux:
  `build-essential pkg-config libssl-dev clang cmake protobuf-compiler unzip`
  (`rust-rapidsnark` downloads a prebuilt library and unzips it at build time).
- `git`, `curl` and `jq`.
- macOS or Linux on x86_64 or aarch64, the platforms the node release binaries are published for.

### Run on the testnet

Needs a synced Logos Blockchain testnet node on `127.0.0.1:8080` whose wallet holds funds
(mined PoW rewards on the `PoWClaim` key are enough).

```bash
git clone --recurse-submodules https://github.com/logos-co/logos-rln-membership-zone
cd logos-rln-membership-zone

cp .env.example .env             # set FUNDING_PK to the node's PoWClaim public key
set -a; source .env; set +a      # clap reads process env vars, not the .env file
cargo run -p zone-sequencer

curl -s localhost:3000/ | jq                 # channel, LEZ head, tree counters
curl -s localhost:3000/roots/5 | jq          # newest roots with their status on Bedrock
curl -s localhost:3000/merkle_proof/0 | jq   # proof path of leaf 0 at the newest adopted root
curl -s localhost:3000/leaves | jq           # every leaf at the LEZ head
```

The first start creates the channel (the key in `zone-sequencer/data/key.hex`, the id
`sha256(CHANNEL_NAME)`), reads every LEZ block since the program was deployed, backfills the
channel from the node and publishes the first batch; later starts resume from the data dir.
Everything else, including running your own LEZ sequencer as the block source, is in
[`docs/setup-rln-membership-zone.md`](docs/setup-rln-membership-zone.md).

### Tests

```bash
cargo test -p zone-sequencer
cargo clippy -p zone-sequencer --all-targets -- -D warnings
```

The unit tests cover the derivation rules (a `Registered` event sets the leaf and the proof
path, expiry removes it at the LEZ timestamp, events of other registries are ignored), the
tree snapshot round trip, and the decoding of LEZ blocks and the program's instructions,
including a real testnet block.

## References

- [RLN-API](https://lip.logos.co/anoncomms/raw/rln-api.html):
  the interface the Module exposes, and the `logos` namespace binding
- [logos-lez-rln](https://github.com/logos-co/logos-lez-rln): the current LEZ-hosted registry
- [logos-rln-modules](https://github.com/logos-co/logos-rln-modules): the Logos Core RLN Modules
- [Zerokit](https://github.com/vacp2p/zerokit): RLN proofs and Merkle trees
- [Bedrock v1.1 Mantle specification](https://lip.logos.co/blockchain/raw/bedrock-v1.1-mantle-specification.html):
  channels and inscriptions
- [LEE v0.3 specification](https://lip.logos.co/blockchain/raw/lez/lee-v0.3-specifications.html)
- Zone SDK: `logos-blockchain/zone-sdk/` (`BRIDGING.md`, `DECENTRALIZED_SEQUENCING.md`, `src/`)
