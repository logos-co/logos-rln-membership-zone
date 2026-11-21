# RLN Membership Zone

A Logos Zone for RLN (Rate-Limiting Nullifiers) memberships, with deposits in an LEZ
registration program.

The membership set, the Merkle tree of rate commitments, is derived from a registration program
on the Logos Execution Zone (LEZ) by fixed rules over the program's events: anyone who reads LEZ
computes the same tree. This Zone computes it and publishes the roots as batches on Bedrock; Zone
indexers compute it too, check every batch, and serve the registry to the RLN Modules
([RLN-API](https://lip.logos.co/anoncomms/raw/rln-api.html)). Deposits, refunds and slashing stay
in the registration program; the Module holds no tree. Registration is one cheap LEZ transaction
and the tree has depth 20, the depth Zerokit ships.

## Status

Prototype on the Logos testnet: the registration program is deployed (LEZ block 49431), the
sequencer publishes on the channel `rln-membership-zone:testnet:v0.3.2-3` and an indexer judges
it. The Module side is not written.

- **Specification**: the RLN Membership Zone LIP,
  [logos-lips PR #548](https://github.com/logos-co/logos-lips/pull/548)
  ([`docs/anoncomms/raw/rln-membership-zone.md`](https://github.com/logos-co/logos-lips/blob/docs/rln-membership-zone/docs/anoncomms/raw/rln-membership-zone.md)
  on the branch; `https://lip.logos.co/anoncomms/raw/rln-membership-zone.html` once merged):
  the design, its decisions, the deployment parameters and what comes later.
- **Zone node** (`src/`, bin `zone`): one binary, two modes, the same HTTP registry API. Both
  read the LEZ blocks from the LEZ channel on Bedrock through the Zone SDK, never from a LEZ
  Sequencer's word alone. The **sequencer** derives the tree from the LEZ blocks adopted on
  Bedrock, publishes batches on its channel through the Zone SDK and follows them to finality.
  The **indexer** judges each batch against the tree of the LEZ blocks finalized on Bedrock: the
  watchdog of the sequencer, no accredited key, no funds.
- **Not yet written**: the event source once LEZ serves events live, and the Module side.

## How it works

```text
Module --Register / Extend / Erase / Slash--> LEZ registration program   (deposit, leaf index, events)
                                                   |
                                                   v  LEZ blocks, from the LEZ channel on Bedrock (adopted / finalized)
                          Zone sequencer --batch--> Bedrock              (changes + root_after)
                                |                  |
                                |                  v  batches: applied or parked (hours later, at the finalized LEZ blocks)
                                |                Zone indexer            (the watchdog; registry access for a node that trusts nobody)
                                v
   Module (no tree) <--roots, proof path, state-- (registry access: the Zone sequencer's API as a rule, or Bedrock, or an own indexer)
```

1. The Module sends `Register` to the registration program on LEZ; the program locks the
   deposit, assigns the leaf index and emits `Registered`.
2. Every reader (the Zone sequencer, every Zone indexer) applies the same derivation rules to the
   program's events: `Insert` and `Remove`, plus expiry by the LEZ clock.
3. The Zone sequencer publishes `Batch{from_lez_block_id, to_lez_block_id, changes, root_after}`
   as one inscription. Every Zone indexer recomputes each batch from LEZ and parks any that
   omits, invents or misorders a change; a parked batch's root is never a valid root.
4. The Module reads its registry access (a Zone node's API, the Zone channel through its own
   node, or its own Zone indexer): the valid roots, its Merkle proof path at the newest one, its
   membership state and the LEZ clock. It proves against the newest valid root and accepts
   proofs against any root in the window, as RLN-API specifies.

The Zone holds no funds. A Zone sequencer can omit a change, which is provable censorship, but
that only pauses the registry at the Zone indexers until a correct batch arrives.

## Repository layout

```text
src/                              the Zone node: sequencer and indexer (MODE), HTTP registry API; src/bin/ the proof tools
  src/lez.rs                      LEZ blocks (LEZ's own `common` types) and the program's events, derived from its transactions (rln-layouts)
  src/reader.rs                   a reader: derivation rules over the events and the tree (sled-backed PmTree)
  src/lez_chain.rs                the LEZ chain as the node keeps it: the finalized tree on disk, the adopted blocks folded over it
  src/batch.rs                    batch wire format (protobuf)
  src/zone_chain.rs               the Zone channel as a consumer keeps it, after LEZ's chain_state: final tier, view, folds
  src/registry.rs                 one node's state: LEZ chain, Zone channel, pending changes, checkpoint, in small files beside the tree store
  src/http.rs                     HTTP API: roots, batches, membership with its Merkle proof path, leaves
  src/lez_sync.rs                 the LEZ channel through the Zone SDK: checkpoint at the LIB, history from the LEZ Sequencer under the hash chain, adopted and finalized blocks
  src/zone_sync.rs                the Zone channel through the Zone SDK: start-up checks, extension, conflict, finality, the finality scan, the sequencer's tick, publish and correction, the L1 watchdog
  src/main.rs                     configuration, the two Zone SDK readers, the event loop
  src/bin/generate_proof.rs       an RLN proof over the Zone's /merkle_proof answer (the Module's prover side)
  src/bin/verify_proof.rs         the proof against the Zone's /roots window (the validator side)
e2e/                              testnet scenarios: provision a LEZ payer, register, slash, prove (lib.sh, testnet.env, scenarios/)
systemd/                          user units to run the sequencer and an indexer as services
lez-sequencer/                    config template to run your own LEZ sequencer for the finalized history
.env.example                      the node's configuration, every env var with its default
```

The Zone SDK and the other `logos-blockchain` crates are git dependencies in `Cargo.toml`,
pinned to the tag of the node release in use (0.3.2); LEZ's `common` and `lee` crates to the
LEZ tag (v0.3.2); the program's layouts to a `logos-lez-rln` commit.

## Run it

Written for one Linux box (WSL2 works) that runs a synced Logos Blockchain testnet node.

| Process | Port | Role |
| --- | --- | --- |
| Logos Blockchain node (Bedrock) | 8080 | HTTP API: wallet, mempool, blocks. The Zone SDK publishes batches through it and pays fees from its wallet. Never expose it: no auth, reaches the wallet. |
| Zone sequencer, `zone` | 3000 | Reads the LEZ blocks from the LEZ channel on Bedrock, derives the tree, publishes one batch per change or heartbeat, serves the API. |
| Zone indexer, `zone` with `MODE=indexer` | 3001 | Optional, beside the sequencer: derives the same tree, judges every batch of the channel against it, serves the same API, a parked batch as `stall`. |
| LEZ Sequencer | public, or 3040 | The finalized LEZ history (`getBlockRange`, pinned by hash to the inscriptions Bedrock finalized) and the preview of blocks not on Bedrock yet. Default `https://testnet.lez.logos.co`; or your own, below. |

### Prerequisites

```bash
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libssl-dev clang cmake protobuf-compiler unzip git curl jq
sudo apt-get install -y libpcsclite-dev    # the lez-rln bins the e2e scripts use (mint_payer, register_member)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source ~/.cargo/env
```

`rust-toolchain.toml` installs the toolchain on the first `cargo` run. `unzip` is needed by
`rust-rapidsnark`, a build dependency of the node crates.

The node must be synced: `curl -s localhost:8080/cryptarchia/info | jq` with `state` `Online`
and `phase` `Following` (the zone waits for `Following` before it starts). `slot` minus
`lib_slot` is the finality gap, 8 to 10 hours on the testnet (a block every ~30 s,
`security_param` 1000).

The node wallet key that pays the batches: mined PoW rewards land on the `PoWClaim` key of the
node keystore, `yq .public_keys.PoWClaim ~/keystore.yaml`; the balance is at
`curl -s localhost:8080/wallet/<pk>/balance`. A batch costs one `CHANNEL_INSCRIBE` plus the fee
transfer and the bytes, capped by `MAX_TX_FEE` (0.001 LGO by default, the real fee is far lower).

The Zone SDK must match the node release: a newer node answers the backfill with blocks the
older SDK cannot parse (`Failed to fetch immutable blocks ... Failed to parse response`,
repeating); bump the `logos-blockchain` tag in `Cargo.toml` and `rust-toolchain.toml` then.

### Configure

```bash
git clone https://github.com/logos-co/logos-rln-membership-zone && cd logos-rln-membership-zone
cp .env.example .env
sed -i "s/^FUNDING_PK=.*/FUNDING_PK=$(yq .public_keys.PoWClaim ~/keystore.yaml)/" .env
```

Every key is also a flag (`cargo run -- --help`):

| Key | Default | Meaning |
| --- | --- | --- |
| `MODE` | `sequencer` | `sequencer` publishes the batches, `indexer` reads and judges them |
| `NODE_URL` | `http://127.0.0.1:8080` | Logos Blockchain node HTTP API |
| `FUNDING_PK` | | Node wallet key paying the batches (`PoWClaim`) |
| `MAX_TX_FEE` | 1000000 | Fee cap per publish, atomic units |
| `PRIORITY_FEE_PERCENT` | 12 | Reserve over the mandatory fee, the excess is a tip; block builders take the mempool FIFO, so it buys nothing on the testnet |
| `CHANNEL_NAME` | `rln-membership-zone:testnet:v0.3.2-3` | Channel id = `sha256(CHANNEL_NAME)`; `CHANNEL_ID` (64 hex) overrides |
| `PROGRAM_ACCOUNT_ID` | `f048...106a` | Registration program on the testnet LEZ (`HAxY9...4bBw` in base58) |
| `TREE_ID` | `47b2de4f...b4cf` | Registry the program serves, first field of every event |
| `FIRST_LEZ_BLOCK_ID` | 49431 | Block the program was deployed in; nothing before it is read |
| `LEZ_CHANNEL_ID` | `0101...01` | The LEZ channel on Bedrock: the LEZ blocks come from its inscriptions, through the Zone SDK |
| `LEZ_SEQUENCER_URL` | `https://testnet.lez.logos.co` | LEZ Sequencer RPC that fills the finalized LEZ history fast, pinned by hash to the blocks Bedrock finalized |
| `REBUILD_TREE` | false | Drop the tree and read it again from LEZ, keeping the Zone channel state and its checkpoint (after a decoder fix); one start |
| `POLL_SEC` | 10 | Seconds between publish checks (the sequencer) or judgments (the indexer) |
| `HEARTBEAT_SEC` | 3600 | Publish an empty batch over the LEZ blocks adopted since after this long without one |
| `STALE_REFUND_SLOTS` | 0 | LIB slots an unmined batch waits before the Zone SDK re-funds it under a new transaction; off, since a re-funded transaction goes to the back of the FIFO mempool |
| `DATA_DIR` | `data` | `key.hex` (the channel key, the sequencer only), the tree store `tree/` and the state files beside it. Gitignored: copy `key.hex` outside the checkout after the first start |
| `HTTP_ADDR` | `127.0.0.1:3000` | HTTP API bind address |

### Run

```bash
set -a; . ./.env; set +a      # clap reads process env vars, not the .env file
cargo run                      # or cargo build --release --bins && target/release/zone
```

The log goes to stdout through `tracing`. Without `RUST_LOG` the filter is
`warn,zone=info,logos_blockchain=error`; `RUST_LOG=warn,zone=info,logos_blockchain=info` shows
the Zone SDK at work. What happens:

1. The node's `phase` is checked; the zone waits until it is `Following`.
2. First start writes the accredited key to `data/key.hex`. Keep it: only this key can write
   the channel. There is no create step, the first inscription to an unused id creates the
   channel with this key as its only accredited key.
3. The state under `data/` is opened. On a fresh store the tree of the LEZ blocks finalized on
   Bedrock is built up to the newest LEZ inscription at or below the node's LIB, from the LEZ
   Sequencer's `getBlockRange` pinned by the hash chain to that inscription (a minute). The
   sequencer checks `GET /channel/:id` on the node: the channel on chain must be the one in the
   data dir (same key, a tip it knows); batches of ours after the tip on chain, orphaned by a
   reorg while the node was down, are published again; the Zone SDK checkpoint takes the
   channel's tip from the chain.
4. Two Zone SDK readers follow Bedrock from the node. One follows the LEZ channel from the LIB:
   the LEZ blocks adopted since are folded over the finalized tree (the head), the finalized
   ones move into the tree store. The other follows the Zone channel: from the LIB when it has
   no message on chain yet (seconds), from genesis the first time it has history (30 to 60
   minutes on the testnet), from the checkpoint in `zone.json` after that. Then `ready` and
   `LEZ channel ready`.
5. Every `POLL_SEC` the sequencer publishes when due (changes from adopted LEZ blocks not yet
   covered, `HEARTBEAT_SEC` passed, or nothing published yet), at most one batch in flight, each
   batch folded on its own head first so nothing it would park reaches Bedrock. A reorg that
   orphans a batch has it published again, the same message id; LEZ contradicting a batch
   already on Bedrock gets a correction batch (every leaf, from `FIRST_LEZ_BLOCK_ID`) that every
   reader applies and that supersedes every root before it. A finality scan every 5 minutes
   applies what the stream may have missed; an L1 watchdog says once when no Bedrock block has
   come for 10 minutes and whether the node stands still or the streams are dead.

Ctrl-C stops it; the next start continues from the state. Events that happened while the
sequencer was down are picked up by the next batch, which spans the whole gap.

### Data files

| File | Content | Lose it and |
| --- | --- | --- |
| `data/key.hex` | the accredited key (sequencer only) | the channel is lost; start a new one with a new `CHANNEL_NAME` |
| `data/tree/`, `data/reader.json` | the tree of the finalized LEZ blocks (sled) and the reader's state; one without the other is refused | rebuilt from LEZ on a fresh start |
| `data/zone.json`, `data/final.jsonl` | the Zone channel: view, anchor, in-flight publishes, pending changes, batches to publish again, the Zone SDK checkpoint; one line per finalized entry | the start-up check refuses to run; restore the files or start a new `CHANNEL_NAME` |
| `data/lez_view.json` | the adopted LEZ blocks over the finalized tree | the SDK reports them again from the LIB, minutes |

Every file is small and written only when it changed, in an order a crash between two writes
cannot break (`final.jsonl`, `zone.json`, `lez_view.json`, `reader.json`; a torn last line of
`final.jsonl` is dropped and the file written whole at the next save).

Never delete the state on a channel that has batches: the Zone SDK learns a channel's tip from
finalized history only (hours behind), so without the checkpoint it chains the next batch on a
stale parent, which the chain refuses silently. The start-up check catches a data dir that does
not continue the channel on chain; the fix is to restore it or to start a new channel.

### Start a new channel

```bash
sed -i 's/^CHANNEL_NAME=.*/CHANNEL_NAME=rln-membership-zone:testnet:v0.3.2-x/' .env
rm -rf data/tree data/*.json data/*.jsonl    # key.hex stays; the old channel costs nothing abandoned
```

### Indexer

The same binary in `MODE=indexer` trusts nobody about the batches: it follows the channel with
a throwaway key that is not accredited (it only reads), reads LEZ exactly like the sequencer, and
judges every batch against the tree of the LEZ blocks **finalized** on Bedrock, which can never
be rewritten under it. A batch must continue the tip (`from_lez_block_id`), its `changes` must
replay to its `root_after`, and that root must be the indexer's own tree at `to_lez_block_id`.
The outcome is LEZ's `AcceptOutcome`: `applied`; `parked` with the `error` (`unexpected_block_id`,
`hash_mismatch`, `deserialize`, `derivation_mismatch`), after which the tip is frozen and every
later batch parks too, the first one recorded as the `stall`, until a correction applies on the
frozen tip; or `retryable_failure` (`lez_not_read`: the finalized LEZ blocks have not reached the
batch yet; every batch behind it waits), judged again at every poll. No funds, no publishing.

```bash
set -a; . ./.env; set +a
MODE=indexer DATA_DIR=data-indexer HTTP_ADDR=127.0.0.1:3001 cargo run
```

`/roots` serves the batches that applied and no correction superseded, so an indexer's `/roots`
trails the sequencer's by LEZ finality; `/` serves `stall`. An indexer can run on and off: the
final tier is re-read from the anchor on every start, a batch it could not judge yet is judged
when LEZ finality reaches it; after `REBUILD_TREE=true` it judges again what its old tree parked.

### As a service

`systemd/` holds user units (`systemctl --user`, no root) for the sequencer and an indexer. They
run the release binary from `~/logos-rln-membership-zone`, read `.env` (the indexer then
`systemd/indexer.env`, which overrides `MODE`, `DATA_DIR` and `HTTP_ADDR`), log to the journal,
stop the node with SIGINT so it closes its store, and restart it 30 s after a failure.

```bash
cargo build --release --bins
mkdir -p ~/.config/systemd/user
cp systemd/zone-sequencer.service systemd/zone-indexer.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now zone-sequencer zone-indexer
loginctl enable-linger "$USER"        # once: keep them running after logout

systemctl --user status zone-sequencer zone-indexer
journalctl --user -u zone-sequencer -f            # live log; -u zone-indexer for the other
journalctl --user -u zone-sequencer --since -2h -o cat --no-pager
git pull && cargo build --release --bins && systemctl --user restart zone-sequencer zone-indexer
```

A one-off start with another setting (`REBUILD_TREE=true target/release/zone` under
`set -a; . ./.env; set +a`) runs with the service stopped.

## HTTP API

```bash
curl -s localhost:3000/ | jq                  # channel, LEZ head, counters, stall, newest roots
curl -s localhost:3000/roots/5 | jq           # newest 5 roots: the changes that made each, its batches on Bedrock
curl -s localhost:3000/merkle_proof/0 | jq    # membership at leaf 0 and its proof path at the newest valid root
curl -s localhost:3000/merkle_proof/<id_commitment hex> | jq   # the same, by identity commitment
curl -s localhost:3000/leaves | jq            # active memberships with expiry, removed ones with why
curl -s localhost:3000/health
```

Words used the same way in every answer: `head` is where the tree is (`lez_block_id`,
`lez_timestamp_ms`, the clock expiry runs on, and `root`); a batch's `bedrock_status` is the Zone
SDK's word for where it stands on Bedrock, `pending` (published, no Bedrock block yet),
`adopted` (in a block on the current branch) or `finalized` (`l1_slot` the slot that finalized
it); a membership is `active` or `removed` for `expired`, `slashed` or `erased`. 32-byte values
are hex, little-endian; counts end in `_count`; a value that does not apply is `null`, never
absent.

- `/` adds `zone_channel_id`, `lez_source`, `mode`, `first_lez_block_id`, `lez_events` (the
  program's events applied), `private_transaction_count` (privacy-preserving transactions under
  the program, which carry no instruction: each changed the registry in a way this tree does
  not have), `tree` (`active_leaf_count`, `removed_leaf_count`, `changes` made beyond the one
  insert per `Registered`), `pending_change_count`, `batch_count` (applied and not superseded),
  `stall`, `in_flight` (our batch no Bedrock block holds yet), `lez` (`adopted_block_id`,
  `finalized_block_id`, `adopted_block_count`, `sequencer_block_id`, `not_on_bedrock`: LEZ blocks
  produced and not adopted yet; hundreds mean LEZ stopped inscribing), `orphaned_count`
  (publishes of ours a reorg orphaned, published again; cumulative), `superseded_count`
  (batches a correction superseded), `newest_published`, `newest_finalized`, `endpoints`.
- `/roots` lists the roots batches carry, newest first, one entry per run of batches carrying
  the same root: `root`, `from_lez_block_id`, `to_lez_block_id`, and `batches` (the producing
  one first), each with `msg_id`, `parent_msg`, the range, `root`, `changes` (`kind` `insert`
  with the `rate_commitment` or `remove`, `leaf_index`, `lez_tx_hash`, `null` for an expiry),
  `bedrock_status`, `l1_slot`. A root the tree comes back to is a new entry. A validator reads
  its policy off the producing batch's `changes`: the roots from the newest back to the newest
  that removes a leaf are the ones no removed member can still prove under.
- `/merkle_proof` answers 404 for a leaf or commitment neither the tree nor the LEZ Sequencer
  knows, 400 for a path that is neither a decimal leaf index nor 32 hex bytes. It carries the
  membership as the LEZ head knows it (`leaf_index`, `id_commitment`, `rate_limit`, `active`,
  `registered_at_lez_block_id`, `registered_lez_tx_hash`, `removed`, `removed_at_lez_block_id`,
  `removed_lez_tx_hash`, `expiry_timestamp_ms`), `newest_published`, `newest_finalized`, `proof`
  (the Merkle proof path at the newest valid root, the tree of the newest batch that applied:
  `rate_commitment`, `path_elements`, `identity_path_index`, `root`, `to_lez_block_id`; `null`
  while no batch carries the leaf, or once it left that tree) and `lez_preview` (the newest
  event about the membership in a LEZ block the LEZ Sequencer produced but Bedrock has not
  adopted yet; nothing from it is ever batched).
- `/leaves` lists `active` and `removed` memberships at the LEZ head with the same keys as
  `/merkle_proof` plus `expires_in_ms`.

## LEZ source

The LEZ Sequencer inscribes every block it produces in the LEZ channel on Bedrock
(`LEZ_CHANNEL_ID`, payload LEZ's own borsh `Block`). The node reads that channel through the
Zone SDK: an **adopted** LEZ block (its inscription in a Bedrock block on the current branch,
minutes after production) is folded over the finalized tree into the head, a **finalized** one
moves into the tree store, a Bedrock reorg refolds the head. A block the LEZ Sequencer has only
closed is never used: a LEZ Sequencer restart produces such blocks again, possibly with other
contents. The sequencer batches the adopted blocks, the indexer judges with the finalized ones.

`LEZ_SEQUENCER_URL` serves `getBlockRange` for one job: the finalized history on a fresh store,
and the blocks a finalized inscription skipped over after a long stop; those blocks are trusted
only through the hash chain down to the block Bedrock finalized. Neither source serves events,
so the program's events are derived from its transactions with the program's own layouts crate
(`rln-layouts`): every `Register` to the program is a `Registered`, every `Slash` a `Slashed` of
`poseidon(identity_secret)`, and so on; `Registered` carries the expiry `now + active + grace`
(the end of the grace period), a membership proves through its grace period. Known gap: a
transaction the program reverted stays in a v0.3.2 block without a marker and reads like a
success; LEZ v0.4 marks it, events settle it. A transaction the decoder cannot read is reported
and skipped; after a decoder fix, `REBUILD_TREE=true` once reads the tree again.

```bash
lez() { curl -s -X POST "${LEZ_SEQUENCER_URL:-https://testnet.lez.logos.co}" -H 'content-type: application/json' -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\",\"params\":${2:-[]}}" | jq; }
lez getLastBlockId                 # the LEZ Sequencer's newest block
lez getChannelId                   # 0101...01, the LEZ channel
lez getBlock '[50056]'             # base64 of the borsh block
curl -s localhost:3000/ | jq .lez  # adopted / finalized / sequencer block ids, not_on_bedrock
```

### Your own LEZ sequencer

A LEZ sequencer whose key holds no stake does not produce blocks but follows the channel and
serves the same JSON-RPC: no dependency on the public endpoint for the history fill. It is a
full LEZ node (RISC Zero execution, RocksDB, a few GB; the first build takes long; the initial
sync replays the channel from genesis, hours). Run the tag the testnet runs; you need the
testnet genesis from the LEZ team.

```bash
git clone --depth 1 --branch v0.3.2 https://github.com/logos-blockchain/logos-execution-zone ~/logos-execution-zone
mkdir -p lez-sequencer/home && cp lez-sequencer/sequencer_config.json lez-sequencer/home/
$EDITOR lez-sequencer/home/sequencer_config.json    # the testnet genesis; funding_key = any public key of the node wallet
cd ~/logos-execution-zone && RUST_LOG=warn,sequencer_service=info cargo run --release --features prove -p sequencer_service -- \
    ~/logos-rln-membership-zone/lez-sequencer/home/sequencer_config.json \
    --home ~/logos-rln-membership-zone/lez-sequencer/home --port 3040 --listen-address 127.0.0.1
sed -i 's#^LEZ_SEQUENCER_URL=.*#LEZ_SEQUENCER_URL=http://127.0.0.1:3040#' ~/logos-rln-membership-zone/.env
```

`lez-sequencer/home/` is gitignored. `getLastBlockId` on `127.0.0.1:3040` should approach the
public one.

## Monitoring

```bash
curl -s localhost:3000/ | jq '{in_flight, batch_count, pending_change_count, stall, orphaned_count, superseded_count, lez, head, newest_published}'
watch -n 10 'curl -s localhost:3000/roots/3 | jq -c ".roots[] | {root, to_lez_block_id, batches: [.batches[] | {bedrock_status, l1_slot}]}"'
curl -s localhost:8080/cryptarchia/info | jq '.cryptarchia_info | {slot, height, lib_slot, state}'   # height must move
curl -s localhost:8080/wallet/$(yq .public_keys.PoWClaim ~/keystore.yaml)/balance | jq
curl -s localhost:8080/mempool/view | jq
echo "LEZ lag: $(( $(curl -s localhost:8080/cryptarchia/info | jq '.cryptarchia_info.slot') - $(curl -s localhost:8080/channel/0101010101010101010101010101010101010101010101010101010101010101 | jq '.tip_slot') )) slots"   # < 500 as a rule
```

Worth watching: the wallet balance before and after a batch lands (the real fee), `lib_slot`
against `slot` (finality), how long a batch stays `in_flight`, `not_on_bedrock` and the LEZ lag
(LEZ inscribing), the node's `height` (a node whose `height` stands still, `Online` or not, is
stuck: restart it, the zone follows).

## Troubleshooting

Start-up refusals, each with the store intact:

- `Failed to fetch immutable blocks ... Failed to parse response` repeating: Zone SDK older than
  the node; bump the `logos-blockchain` tag.
- `the node is in InitialBlockDownload (...); waiting until it follows the chain` (INFO, once
  per phase): the node is catching up; the zone starts when it is `Following`.
- `batch [A..B] would not apply on our own head (...); stopping before publishing it`: a bug or a
  store from another configuration (`FIRST_LEZ_BLOCK_ID`, `TREE_ID`); nothing reaches Bedrock.
- `no LEZ inscription in the N finalized Bedrock slots below the LIB`: `LEZ_CHANNEL_ID` is not
  the LEZ channel, or LEZ stopped inscribing for long; the scan reads further back.
- `finalized LEZ block N does not chain on the final tier (...): start once with REBUILD_TREE=true`;
  `zone.json starts at LEZ block N, FIRST_LEZ_BLOCK_ID says M`, `reader.json follows registry X,
  not Y`, `<dir> holds tree but not reader.json`: the data dir belongs to another configuration
  or lost a file; `the LEZ Sequencer did not serve the history up to LEZ block N: start again`.
- The channel checks of the sequencer (`channel is on chain with accredited keys ..., not ours`,
  `channel is on chain (tip X..) but this data dir holds no batch`, `... holds no Zone SDK
  checkpoint`): the data dir does not continue the channel on chain; restore it or start a new
  `CHANNEL_NAME`. `N batch(es) of ours after the channel tip X.. are not on chain: orphaned,
  published again at the next tick` (WARN) and `the Zone SDK checkpoint's tip was not the
  channel's tip T.. on chain: corrected` (WARN) are handled, nothing to do.

While running:

- `LEZ blocks A..B read from the LEZ Sequencer under finalized block N` then `... not read: ...;
  finalized block N waits for the next one`: the LEZ Sequencer is unreachable, the gap is filled
  under the next finalized block. `the LEZ Sequencer's history does not lead to LEZ block N,
  which Bedrock finalized` stops the process: that sequencer does not follow Bedrock. A
  `getBlockRange: {"code":-32008,... "Response is too big"}` inside such a line is halved down
  to one block automatically.
- A batch stays `in_flight` for more than a few minutes: `mempool/view` lists it, the mempool
  is congested (FIFO, a tip buys nothing, eviction after 9 hours); not listed, the node dropped
  it (`curl localhost:8080/cryptarchia/transaction/<tx hash>` says whether it was mined), or
  the node's `height` does not move (stuck node).
- `the channel's root R.. at LEZ block N is not the tree's S..: correction batch [F..H] restates
  the tree (K leaves), every root before it is superseded` (WARN) after `LEZ channel reorg on
  Bedrock ...` and `batch msg M.. claims root R.. ..., LEZ now gives S..`: a Bedrock reorg
  replaced a LEZ block a batch covered; the correction applies on every reader, moves a frozen
  tip, and the roots before it leave `/roots` (`superseded_count`). No restart, no new channel.
- `the final tier is frozen at batch msg M.. (Bedrock slot S): ...` (WARN) or a batch `parked`:
  a finalized batch did not apply (its error says which check), `/` shows it as `stall`; on the
  indexer that is the evidence it holds until the correction applies.
- `channel entry msg X.. chains on Y.., which the view does not hold: the lineage has a gap`
  (WARN, once per gap): the SDK reported entries out of order; the view mends itself.
- `Pending tx ... expired unmined; orphaned` in a burst from the Zone SDK, then `the SDK reports
  a conflict on the channel, but the node's channel tip is still our newest entry`: the SDK
  could not see its entries mined (slow node); the node's tip says otherwise, nothing is
  published again. `LEZ block N finalized with hash H.., the view held G..`: the view is
  refilled from the finalized tree, nothing to do.
- `batch [A..B] does not fit one inscription`, `not ready to publish (...)`, `publish failed:
  ...`, `LEZ Sequencer preview not read: ...`, each `said once`: the next poll tries again; the
  first should not happen (a batch stops at the last LEZ block whose changes fit).
- `no Bedrock block from the Zone SDK for N min on both channels; the node's tip is at slot S,
  height H, LIB slot L, phase P; whether its height moves is said within 3 min` (WARN, once):
  the L1 watchdog. Then either `the node's height stands at H ... the node, or Bedrock,
  produces no block and the Zone waits` (restart or check the node; the zone is fine) or `the
  node's height moved A -> B and the Zone SDK still reports no block: Bedrock runs, both streams
  are dead, restart this process` (the SDK reconnects only when the node closes its stream,
  not when it stalls). One stream silent while the other receives blocks: `the Zone SDK stream
  on the X channel reported no Bedrock block for N min while the Y channel's did: that stream
  is dead, restart this process`. `Bedrock blocks reach both Zone SDK streams again` ends it.
- `the Zone SDK published msg X.. on parent P.. but the channel's tip is T..: the SDK's tip is
  stale ...` (fatal, after a publish line ending in `(stale: the pin moved)`): the Zone SDK
  restores the channel's tip from its checkpoint only, never from the node, and it had fallen
  behind; the publish can never land. The process stops on purpose, the service restarts it,
  the start corrects the checkpoint from the chain and drops the stale publish (`... dropped`,
  WARN), and the next tick publishes on the tip.
- `channel: N final ... (checkpoint resumed)` right after changing `CHANNEL_NAME`: the old state
  is still there; stop, `rm -rf data/tree data/*.json data/*.jsonl` (keep `key.hex`), start again.

## Cleanup

```bash
rm -rf data/tree data/*.json data/*.jsonl    # the sequencer's state, the key stays
rm -rf data-indexer                          # the indexer's: nothing to keep
rm -rf data data-indexer target              # everything, key and build included
rm -rf lez-sequencer/home ~/logos-execution-zone/target   # an own LEZ sequencer
```

## Tests

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```

The unit tests cover the derivation rules (a `Registered` event sets the leaf and the proof
path, which proves under Zerokit; expiry removes it at the LEZ timestamp), the state files
surviving a reopen, a crash between two writes and a torn line, the LEZ chain (adopted blocks
folded over the finalized tree, a reorg refolding them, finality moving them), the Zone channel
(final tier, view, parked and waiting batches, orphaned batches published again, corrections),
the HTTP answers and the decoding of LEZ blocks and the program's instructions, including a real
testnet block. They are the last unit tests written: from here the logic is exercised end to
end on the testnet (`e2e/`).

## References

- [RLN Membership Zone](https://github.com/logos-co/logos-lips/pull/548): this Zone's specification (logos-lips)
- [RLN-API](https://lip.logos.co/anoncomms/raw/rln-api.html): the interface the Module exposes, and the `logos` namespace binding
- [logos-lez-rln](https://github.com/logos-co/logos-lez-rln): the current LEZ-hosted registry and the registration program
- [logos-rln-modules](https://github.com/logos-co/logos-rln-modules): the Logos Core RLN Modules
- [Zerokit](https://github.com/vacp2p/zerokit): RLN proofs and Merkle trees
- [Bedrock v1.1 Mantle specification](https://lip.logos.co/blockchain/raw/bedrock-v1.1-mantle-specification.html): channels and inscriptions
- [LEE v0.3 specification](https://lip.logos.co/blockchain/raw/lez/lee-v0.3-specifications.html)
- Zone SDK: `logos-blockchain/zone-sdk/` (`BRIDGING.md`, `DECENTRALIZED_SEQUENCING.md`, `src/`)
