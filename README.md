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
- **PoC**: [`poc/`](poc) is a Zone sequencer for the Logos testnet with an HTTP registry API.
  It publishes batches on its channel through the Zone SDK and follows them to finality;
  until its LEZ event source is wired, LEZ is a stand-in clock that closes empty blocks.
- **Deployed elsewhere**: the registration program is on the testnet LEZ
  (`HAxY9SiqUjJQHdtvazFDWFCxn1Jw1dtbUGio8DMC4bBw`, block 49431).
- **Not yet written**: the LEZ event source of the PoC, and the production
  Zone sequencer and Zone indexer.

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
poc/                              PoC: Zone sequencer on the testnet with an HTTP registry API
  src/lez.rs                      LEZ blocks and events, the `LezSource` trait, the ticking stand-in
  src/reader.rs                   a reader: derivation rules over the events and the tree
  src/batch.rs                    batch wire format (protobuf)
  src/registry.rs                 published batches with their status on Bedrock, trees per batch, history file
  src/http.rs                     HTTP API: roots, Merkle proof path, leaf status
  src/main.rs                     Zone SDK loop: publish, follow to finality, poll LEZ
  lez-indexer/indexer_config.json LEZ Indexer config that keeps only the registration program's events
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

### Run the PoC on the testnet

The PoC needs a synced Logos Blockchain testnet node on `127.0.0.1:8080` whose wallet holds
funds (mined PoW rewards on the `PoWClaim` key are enough).

```bash
git clone --recurse-submodules https://github.com/logos-co/logos-rln-membership-zone
cd logos-rln-membership-zone

cp .env.example .env             # set FUNDING_PK to the node's PoWClaim public key
set -a; source .env; set +a      # clap reads process env vars, not the .env file
cargo run -p rln-zone-poc
```

What happens:

1. First start writes the accredited key to `poc/data/key.hex`. The channel id is
   `sha256(CHANNEL_NAME)`; the key signing the first inscription to that id becomes its
   accredited key, there is no separate create step. Keep the key file: without it the channel
   cannot be written again.
2. The Zone SDK backfills the channel from the node and reports `ready`; the first batch goes
   out at once, then one batch whenever the tree changed or after `HEARTBEAT_SEC` without one,
   at most one in flight. Every publish is paid from the node wallet (`FUNDING_PK`), capped at
   `MAX_TX_FEE`.
3. Each batch is `pending` from publish, `adopted` once the Zone SDK reports it in a Bedrock
   block, `finalized` once it is in finalized history (the log prints the L1 slot), or
   `orphaned` after a reorg, in which case it is published again. A finalized batch's root is
   a valid root.
4. The HTTP API serves the registry on `HTTP_ADDR`:

```bash
curl -s localhost:3000/                 # channel, LEZ head, covered block, batch count
curl -s localhost:3000/roots/5          # newest 5 roots with their status
curl -s localhost:3000/merkle_proof/0   # proof path of leaf 0 at the newest finalized root
curl -s localhost:3000/leaf/<id_commitment hex>
```

Ctrl-C stops it; the next start resumes from `poc/data/checkpoint.json` and `history.json`
and rebuilds the trees from LEZ.

### Read the registration program's events

The RFC derives the tree from the program's events. In LEZ 0.3.x the LEZ Sequencer RPC
serves blocks but not events, and the LEZ Indexer RPC serves events (`getEvents`,
`subscribeToEvents`) for finalized blocks only, so the PoC reads a LEZ Indexer that follows
the LEZ channel from the same testnet node. Run one next to the PoC:

```bash
git clone --depth 1 --branch v0.3.1 https://github.com/logos-blockchain/logos-execution-zone
cd logos-execution-zone
RUST_LOG=info cargo run --release --features prove -p indexer_service -- \
    ../logos-rln-membership-zone/poc/lez-indexer/indexer_config.json \
    --port 8779 --data-dir ../logos-rln-membership-zone/poc/lez-indexer
```

`--features prove` executes the guest programs in process, so no `r0vm` install is needed.
The config points `bedrock_config.addr` at the node, `channel_id` at the testnet LEZ channel
(`0101...01`, what `getChannelId` on `https://testnet.lez.logos.co` returns) and keeps only the
registration program's events through `event_filter.sources`
(`HAxY9SiqUjJQHdtvazFDWFCxn1Jw1dtbUGio8DMC4bBw`, deployed in LEZ block 49431, registry
`47b2de4f...b4cf`); a filter that keeps nothing serves nothing. The indexer re-executes the LEZ channel from its genesis, trailing the LEZ
Sequencer by Bedrock finality. The PoC's `LEZ_SOURCE` for it is the next step
(`lez.rs`, `LezSource`).

The Zone SDK must match the node's release (the submodule tag): a node on a newer release
answers the backfill with blocks the older SDK cannot parse, which shows as
`Failed to fetch immutable blocks ... Failed to parse response` repeating forever.

### Tests

```bash
cargo test -p rln-zone-poc
cargo clippy -p rln-zone-poc --all-targets -- -D warnings
```

The unit tests cover the derivation rules: a `Registered` event sets the leaf and the proof
path, expiry removes it at the LEZ timestamp, events of other registries are ignored.

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
