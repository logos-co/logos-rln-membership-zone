# logos-rln-membership-zone

A Logos Zone for RLN (Rate-Limiting Nullifiers) memberships,
with deposits in an LEZ registration program.

A dedicated Zone maintains the membership set, the Merkle tree of rate commitments,
and publishes it as batches on Bedrock.
Deposits, refunds and slashing stay in a registration program on the Logos Execution Zone (LEZ).
Modules consume the registry through [RLN-API](https://lip.logos.co/anoncomms/raw/rln-api.html)
unchanged, under the `logos` namespace binding.

[logos-lez-rln](https://github.com/logos-co/logos-lez-rln) keeps the tree in LEZ accounts
and hashes it inside the RISC Zero zkVM.
A registration there costs about 9.09M of the 10M execution gas,
so a LEZ block holds one registration and the tree is capped at 512 lifetime leaves.
In this design, registration is a cheap LEZ transaction
and the tree reaches depth 20, the depth Zerokit ships.

## Status

Early design. Nothing is deployed.

- **Specification**: [`docs/rfc-rln-membership-zone.md`](docs/rfc-rln-membership-zone.md),
  a raw LIP draft that opens with the questions still to decide.
- **Demo**: [`examples/demo`](examples/demo) runs the whole flow of the RFC
  against a real local Logos Blockchain node through the Zone SDK.
  LEZ and the registration program are mocked in memory.
- **Not yet written**: the LEZ registration program, and the production
  Zone sequencer and Zone indexer.

## How it works

```text
Module --Register--> LEZ registration program            (deposit, leaf index)
                          |
                          v  finalized LEZ blocks
                     Zone sequencer --batch--> Bedrock    (changes + root_after)
                                                  |
                                                  v  finalized batches
Module <--roots, Merkle proof path, status-- Zone indexer (re-derives every batch from LEZ)
```

1. A Module sends `Register` to the registration program on LEZ.
   The program locks the deposit and assigns the leaf index.
2. A Zone sequencer follows the finalized LEZ blocks and derives the membership-set changes
   (insert, extend, expire, erase, slash) with fixed, deterministic rules.
   It publishes them as a `Batch{lez_from, lez_to, changes, root_after}` inscription.
3. Every Zone indexer replays the finalized batches and re-derives each one from LEZ.
   It rejects any batch that omits, invents or misorders a change.
4. Modules read roots, Merkle proof paths and membership status from a Zone indexer,
   then generate and validate RLN proofs as RLN-API specifies.

The Zone holds no funds.
A dishonest Zone sequencer can delay a change but cannot forge one,
and anyone can prove an omission from public data.

## Repository layout

```text
docs/rfc-rln-membership-zone.md   the specification (raw LIP draft)
examples/demo/                    end-to-end demo: mock LEZ, Zone sequencer, Zone indexer
  src/lez.rs                      mock LEZ and registration program (ConfigState, MembershipState)
  src/membership_set.rs           membership set, derivation rules, membership status
  src/batch.rs                    batch wire format (protobuf)
  src/indexer.rs                  Zone indexer verification (invariants)
  src/main.rs                     scripted scenario, published through the Zone SDK
devnet/run.sh                     local single-node Logos Blockchain (start, fund, status, stop, reset)
logos-blockchain/                 git submodule pinned to node release 0.3.0 (Zone SDK path dependencies)
```

## Quick start

### Prerequisites

- Rust, with the toolchain pinned in `rust-toolchain.toml`
  to the channel of the `logos-blockchain` submodule.
- `git`, `curl` and `jq`, for `devnet/run.sh`.
- macOS or Linux on x86_64 or aarch64, the platforms the node release binaries are published for.

### Run the demo

```bash
git clone --recurse-submodules https://github.com/logos-co/logos-rln-membership-zone
cd logos-rln-membership-zone

devnet/run.sh start              # download node 0.3.0 and start a local single-node chain
devnet/run.sh fund               # mine PoW tickets and claim them to the funding key

cp .env.example .env
set -a; source .env; set +a      # clap reads process env vars, not the .env file
cargo run -p demo                # add `-- --censor` to see a Zone indexer catch an omitted slash

devnet/run.sh stop               # stop the node when done
```

`devnet/run.sh start` runs the upstream standalone deployment:
no peers, 1 s slots, HTTP on `127.0.0.1:8080`,
and all data under `devnet/`, which is gitignored.
The Zone SDK pays for every publish from the node wallet.
If a publish fails with "Wallet does not have enough funds", run `devnet/run.sh fund` again.
`devnet/run.sh reset` stops the node and wipes the chain data.
The public devnet does not work here: its fleet runs unreleased builds,
and released binaries cannot sync with them.

### What the demo does

The demo runs a short script:

1. Alice and bob register.
2. Alice extends her membership during her grace period.
3. Bob double-signals and carol slashes him.
4. Alice's membership expires and she erases it.

On each tick LEZ produces one block and the Zone sequencer publishes one batch.
After every finalized batch, the Zone indexer prints the registry view:

```text
zone sequencer: batch [2..12] published: Remove leaf 1 (Slashed); Update leaf 0 expiry 16000ms; Remove leaf 0 (Expired)
  zone indexer: finalized batch [2..12] verified, root 3e1f1922..
      alice a5f54b28.. leaf  -  MEMBERSHIP_ERASED
      bob   54f94d5e.. leaf  -  MEMBERSHIP_SLASHED
LEZ balances: alice 9995, bob 6995, carol 300, treasury 2700, operator 10
```

With `--censor`, the Zone sequencer drops the slash from its batch.
The Zone indexer rejects that batch and halts with
`changes differ from LEZ (provable censorship or forgery): Remove leaf 1 (Slashed)`.

### Tests

```bash
cargo test -p demo
```

The unit tests cover:

- the registration-program checks: tree capacity, grace period, single-use membership accounts;
- the derivation rules: expiry order, `Extend`, slashing after expiry;
- the membership status table of the RFC.

## References

- [RLN-API](https://lip.logos.co/anoncomms/raw/rln-api.html):
  the interface Modules expose, and the `logos` namespace binding
- [logos-lez-rln](https://github.com/logos-co/logos-lez-rln): the current LEZ-hosted registry
- [logos-rln-modules](https://github.com/logos-co/logos-rln-modules): the Logos Core RLN Modules
- [Zerokit](https://github.com/vacp2p/zerokit): RLN proofs and Merkle trees
- [Bedrock v1.1 Mantle specification](https://lip.logos.co/blockchain/raw/bedrock-v1.1-mantle-specification.html):
  channels and inscriptions
- [LEE v0.3 specification](https://lip.logos.co/blockchain/raw/lez/lee-v0.3-specifications.html)
- Zone SDK: `logos-blockchain/zone-sdk/` (`BRIDGING.md`, `DECENTRALIZED_SEQUENCING.md`, `src/`)
