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

Prototype on the Logos testnet: the registration program is deployed (LEZ block 49431), the
sequencer publishes on the channel `rln-membership-zone:testnet:v0.3.2-3` and an indexer judges
it. The Module side is not written.

- **Specification**: [`docs/rfc-rln-membership-zone.md`](docs/rfc-rln-membership-zone.md),
  a raw LIP draft: the design, its decisions, the deployment parameters and what comes later.
- **Zone node** (`src/`, bin `zone`), two modes on one binary, both with the HTTP registry
  API. Both read the LEZ blocks from the LEZ channel on Bedrock through the Zone SDK, never
  from a LEZ Sequencer's word alone. The **sequencer** runs on the Logos testnet: it derives
  the tree from the LEZ blocks adopted on Bedrock, publishes batches on its channel through
  the Zone SDK and follows them to finality. The **indexer** judges each batch against the
  tree of the LEZ blocks finalized on Bedrock: the watchdog of the sequencer, no accredited
  key, no funds. Setup, operation and troubleshooting:
  [`docs/setup-rln-membership-zone.md`](docs/setup-rln-membership-zone.md).
- **Not yet written**: the event source once LEZ serves events live, and the Module side.

## How it works

```text
Module --Register / Extend / Erase / Slash--> LEZ registration program   (deposit, leaf index, events)
                                                   |
                                                   v  LEZ blocks, from the LEZ channel on Bedrock (adopted / finalized)
                          Zone sequencer --batch--> Bedrock              (changes + root_after)
                                                   |
                                                   v  batches: applied or parked
   Module (no tree) <--roots, proof path, state-- Zone indexer           (registry access: a Zone node, Bedrock, or its own)
```

1. The Module sends `Register` to the registration program on LEZ.
   The program locks the deposit, assigns the leaf index and emits `Registered`.
2. Every reader (the Zone sequencer, every Zone indexer) applies the same derivation rules to
   the program's events, `Registered`, `Extended`, `Erased`, `Slashed`:
   `Insert` and `Remove`, as in the lez-rln Merkle program, plus expiry by the LEZ clock.
3. The Zone sequencer publishes the changes and the root as a
   `Batch{from_lez_block_id, to_lez_block_id, changes, root_after}` inscription.
   Every Zone indexer recomputes each batch from LEZ and parks any that omits, invents or misorders a change;
   a parked batch's root is never a valid root.
4. The Module reads its registry access (a Zone node's API, the Zone channel through its own
   node, or its own Zone indexer): the valid roots (the applied batches
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
src/                              the Zone node: sequencer and indexer (MODE), HTTP registry API; src/bin/ the proof tools
  src/lez.rs                      LEZ blocks (LEZ's own `common` types) and the program's events, derived from its transactions (rln-layouts)
  src/reader.rs                   a reader: derivation rules over the events and the tree (sled-backed PmTree)
  src/lez_chain.rs                the LEZ chain as the node keeps it: the finalized tree on disk, the adopted blocks folded over it
  src/batch.rs                    batch wire format (protobuf)
  src/zone_chain.rs               the Zone channel as a consumer keeps it, after LEZ's chain_state: final tier, view, folds
  src/registry.rs                 one node's state: LEZ chain, Zone channel, pending changes, checkpoint, in small files beside the tree store
  src/http.rs                     HTTP API: roots, batches, membership with its Merkle proof path, leaves
  src/lez_sync.rs                 the LEZ channel through the Zone SDK: checkpoint at the LIB, history from the LEZ Sequencer under the hash chain, adopted and finalized blocks
  src/zone_sync.rs                the Zone channel through the Zone SDK: start-up checks, extension, conflict, finality, the finality scan, the sequencer's tick, publish and correction
  src/main.rs                     configuration, the two Zone SDK readers, the event loop
  src/bin/generate_proof.rs       an RLN proof over the Zone's /merkle_proof answer (the Module's prover side)
  src/bin/verify_proof.rs         the proof against the Zone's /roots window (the validator side)
e2e/                              testnet scenarios: provision a LEZ payer, register, slash, prove (lib.sh, testnet.env, scenarios/)
lez-sequencer/                    config template to run your own LEZ sequencer for the finalized history
docs/setup-rln-membership-zone.md setup, operation, monitoring, troubleshooting, cleanup
.env.example                      the node's configuration, every env var with its default
```

The Zone SDK and the other `logos-blockchain` crates are git dependencies in `Cargo.toml`,
pinned to the tag of the node release in use (0.3.2).

## Quick start

### Prerequisites

- Rust, with the toolchain pinned in `rust-toolchain.toml`
  to the channel the pinned `logos-blockchain` tag builds with.
- Build tools the `logos-blockchain` crates need on Linux:
  `build-essential pkg-config libssl-dev clang cmake protobuf-compiler unzip`
  (`rust-rapidsnark` downloads a prebuilt library and unzips it at build time).
- `git`, `curl` and `jq`.
- macOS or Linux on x86_64 or aarch64, the platforms the node release binaries are published for.

### Run on the testnet

Needs a synced Logos Blockchain testnet node on `127.0.0.1:8080` whose wallet holds funds
(mined PoW rewards on the `PoWClaim` key are enough).

```bash
git clone https://github.com/logos-co/logos-rln-membership-zone
cd logos-rln-membership-zone

cp .env.example .env             # set FUNDING_PK to the node's PoWClaim public key
set -a; source .env; set +a      # clap reads process env vars, not the .env file
cargo run

curl -s localhost:3000/ | jq                 # channel, LEZ head, event and tree counters
curl -s localhost:3000/roots/5 | jq          # newest roots: the changes that made each, its batches on Bedrock
MODE=indexer DATA_DIR=data-indexer HTTP_ADDR=127.0.0.1:3001 \
    cargo run                # an indexer beside it: follows the channel, judges every batch
curl -s localhost:3000/merkle_proof/0 | jq   # membership at leaf 0 (or by id_commitment) and its proof path
curl -s localhost:3000/leaves | jq           # active and removed memberships at the LEZ head
```

The first start creates the channel (the key in `data/key.hex`, the id
`sha256(CHANNEL_NAME)`), builds the tree of the finalized LEZ blocks up to the node's LIB,
follows the LEZ channel from there and publishes the first batch; later starts resume from the
data dir. Everything else, including running your own LEZ sequencer for the history, is in
[`docs/setup-rln-membership-zone.md`](docs/setup-rln-membership-zone.md).

### Tests

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```

The unit tests cover the derivation rules (a `Registered` event sets the leaf and the proof
path, which proves under Zerokit; expiry removes it at the LEZ timestamp), the state files
surviving a reopen, a crash between two writes and a torn line, the LEZ chain (adopted blocks
folded over the finalized tree, a reorg refolding them, finality moving them), the Zone
channel (final tier, view, parked and waiting batches, orphaned batches published again,
corrections), the HTTP answers and the decoding of LEZ blocks and the program's instructions
(another registry's are dropped, a privacy-preserving transaction is skipped and counted),
including a real testnet block. They are the last unit tests written: from here the logic is
exercised end to end on the testnet (`e2e/`).

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
