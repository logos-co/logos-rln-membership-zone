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
- **Demo**: [`examples/demo`](examples/demo) runs the whole flow of the RFC
  against a real local Logos Blockchain node through the Zone SDK.
  LEZ and the registration program are mocked in memory.
- **Not yet written**: the LEZ registration program, and the production
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
examples/demo/                    end-to-end demo: mock LEZ, Zone sequencer, Zone indexer, Modules
  src/lez.rs                      mock LEZ and registration program (ConfigState, MembershipState, Action, Event)
  src/reader.rs                   a reader: derivation rules over the events and the tree, one root per LEZ block
  src/sequencer.rs                Zone sequencer: builds the batches
  src/batch.rs                    batch wire format (protobuf)
  src/indexer.rs                  Zone indexer: follows LEZ, checks the batches, serves roots, paths and state
  src/module.rs                   Module: no tree; status, generate and validate through its registry access
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
cargo run -p demo                # add `-- --dishonest-sequencer` to see a Zone indexer catch an omitted slash

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
3. Bob double-signals and carol slashes him; his whole deposit goes to the treasury.
4. Alice's membership expires and she erases it.

On each tick LEZ produces one block with the events of its actions, the Zone indexer applies it,
and the Zone sequencer publishes one batch.
After every finalized batch the Zone indexer prints its verdict and the valid roots it now serves;
each Module then prints its membership status and readiness, proves against the newest valid root,
and the other Modules validate that root against their window.
Alice and bob read the shared Zone indexer. Carol's Module starts at LEZ block 6 with her own
Zone indexer, synced from the shared one's snapshot (root checked against the batch, leaves against
LEZ) and following LEZ from there:

```text
LEZ block 4: Slash ok
LEZ block 4: events Slashed
zone sequencer: batch [4..4] published: Remove leaf 1
zone indexer: batch [4..4] Verified; valid roots: 7b1e52a0@4
  module alice 2c8d0a91.. leaf  0  MEMBERSHIP_ACTIVE                    ready
  module bob   d4e67f13.. leaf  -  MEMBERSHIP_SLASHED                   ready
  module alice proves against root 7b1e52a0.. (path of 20); bob accepts
...
final state (Zone indexer: 9 batches Verified, up to LEZ block 13, LEZ at block 13):
  module alice 2c8d0a91.. leaf  -  MEMBERSHIP_ERASED                    ready
  module bob   d4e67f13.. leaf  -  MEMBERSHIP_SLASHED                   ready
  module carol 9a0b1c2d.. leaf  -  MEMBERSHIP_UNKNOWN                   ready
LEZ balances: alice 9995, bob 6995, carol 0, treasury 3000, operator 10
```

Identity commitments and roots differ per run; the balances do not.

With `--dishonest-sequencer`, the Zone sequencer censors bob's slash:
it keeps the leaf in its own tree, so its batch has neither the `Remove` nor the right root.
The Zone indexer rejects that batch with
`changes differ from LEZ: Remove leaf 1 omitted (provable censorship)`;
no newer root becomes valid, so the registry pauses until a correct batch continues from the last
Verified one. Bob's membership state on LEZ already says `MEMBERSHIP_SLASHED`,
but a proof he made against the newest valid root still validates until that batch arrives,
which is the exposure the withdrawal delay covers.

### Tests

```bash
cargo test -p demo
```

The unit tests cover:

- the registration program: tree capacity, events only for successful actions, the grace period,
  a membership's own withdrawal delay surviving a configuration change;
- the derivation rules: expiry order, `Extended`, slashing after expiry, one root per LEZ block,
  events of other trees ignored;
- the Zone indexer: a Rejected batch changes nothing and adds no valid root, the valid-root window
  resets at a removal and is bounded in LEZ blocks, and a sync from a snapshot is checked against
  the batch and against LEZ;
- the Module: the membership status cases of the RFC, also ahead of the valid roots,
  and generate and validate against the window, readiness included.

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
