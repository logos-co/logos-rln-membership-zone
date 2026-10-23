# Setup: the RLN Membership Zone on the Logos testnet

Everything needed to run the Zone end to end: a Logos Blockchain testnet node, the Zone
sequencer (`zone-sequencer/`) with its HTTP registry API, and the LEZ source it reads, either
the public LEZ Sequencer or one run next to it (`lez-sequencer/`). Written for one Linux box (WSL2 works) that already
runs the node; the Mac side is only where the code is edited.

## 1. What runs where

| Process | Port | Role |
| --- | --- | --- |
| Logos Blockchain node (Bedrock, L1) | 8080 | HTTP API: wallet, mempool, blocks. The Zone SDK publishes batches through it and pays fees from its wallet. Never expose it: the API has no auth and reaches the wallet. |
| Zone sequencer, `zone-sequencer` | 3000 | Reads LEZ blocks, derives the tree, publishes one batch per change or heartbeat on its channel, serves `/roots`, `/merkle_proof`, `/leaf`, `/leaves`. |
| LEZ Sequencer | 3040 or public | Source of closed LEZ blocks (`getBlockRange`). Default: `https://testnet.lez.logos.co`. Optional: your own, section 5. |

Data flow: LEZ Sequencer (blocks, transactions) -> Zone sequencer (tree, batches) ->
Bedrock node (channel) and HTTP API (Modules).

## 2. Prerequisites on the Linux box

```bash
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libssl-dev clang cmake protobuf-compiler unzip git curl jq
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source ~/.cargo/env
```

`rust-toolchain.toml` of each repository installs its own toolchain on first `cargo` run
(this repository 1.99.0, the LEZ repository 1.98.1; both can coexist). `unzip` is needed by
`rust-rapidsnark`, a build dependency of the node crates, which downloads a prebuilt library
and unzips it.

Node: a synced testnet node on `127.0.0.1:8080` (for example `logosctl` with
`blockchain_module v0.3.2`). Check:

```bash
curl -s localhost:8080/cryptarchia/info | jq
```

`state` must be `Online`, `phase` `Following`. Note `slot` against `lib_slot`: finality is the
gap between them, 8 to 10 hours on the testnet (block every ~30 s, `security_param` 1000).

Funds: the node wallet key that pays the batches. Mined PoW rewards land on the `PoWClaim`
key of the node keystore:

```bash
PK=$(yq .public_keys.PoWClaim ~/keystore.yaml)
curl -s localhost:8080/wallet/$PK/balance | jq
```

A batch costs one `CHANNEL_INSCRIBE` (59 exec gas) plus the fee `TRANSFER` (590) and the
bytes of the transaction, capped by `MAX_TX_FEE` (default 1,000,000 atomic = 0.001 LGO per
publish, the real fee is far lower). No funds are moved anywhere else.

The Zone SDK version must match the node release: the git submodule `logos-blockchain` is
pinned to the tag of the node in use (0.3.2 today). A newer node answers the backfill with
blocks the older SDK cannot parse (`Failed to fetch immutable blocks ... Failed to parse
response`, repeating forever); bump the submodule and `rust-toolchain.toml` then.

## 3. Zone sequencer

### Install and configure

```bash
cd ~
git clone --recurse-submodules https://github.com/logos-co/logos-rln-membership-zone
cd logos-rln-membership-zone
cp .env.example .env
sed -i "s/^FUNDING_PK=.*/FUNDING_PK=$(yq .public_keys.PoWClaim ~/keystore.yaml)/" .env
```

`.env` keys (all also flags, see `cargo run -p zone-sequencer -- --help`):

| Key | Default | Meaning |
| --- | --- | --- |
| `NODE_URL` | `http://127.0.0.1:8080` | Logos Blockchain node HTTP API |
| `FUNDING_PK` | | Node wallet key paying the batches (`PoWClaim`) |
| `MAX_TX_FEE` | 1000000 | Fee cap per publish, atomic units |
| `PRIORITY_FEE_PERCENT` | 12 | Reserve over the mandatory fee, the excess is a tip; block builders take the mempool FIFO, so it buys nothing on the testnet (SDK default 12) |
| `CHANNEL_NAME` | `rln-membership-zone:testnet:v0.3.0` | Channel id = `sha256(CHANNEL_NAME)`; `CHANNEL_ID` (64 hex) overrides |
| `PROGRAM_ACCOUNT_ID` | `f048...106a` | Registration program on the testnet LEZ (`HAxY9...4bBw` in base58) |
| `TREE_ID` | `47b2de4f...b4cf` | Registry the program serves, first field of every event |
| `FIRST_LEZ_BLOCK_ID` | 49431 | Block the program was deployed in; nothing before it is read |
| `LEZ_SEQUENCER_URL` | `https://testnet.lez.logos.co` | Where the LEZ blocks come from (section 4 or 5) |
| `REBUILD_TREE` | false | Drop the tree and read it again from LEZ, keeping the batch history and the checkpoint (after a decoder fix that had skipped transactions) |
| `POLL_SEC` | 10 | Seconds between LEZ polls |
| `HEARTBEAT_SEC` | 3600 | Publish an empty batch after this long without one |
| `STALE_REFUND_SLOTS` | 0 | LIB slots an unmined batch waits before the Zone SDK re-funds it under a new transaction (0 disables). Off: the mempool is FIFO, a re-funded transaction goes to the back of the queue; the SDK default, 30, re-funded at every finalized block |
| `DATA_DIR` | `zone-sequencer/data` | `key.hex` and the tree store `tree/` |
| `HTTP_ADDR` | `127.0.0.1:3000` | HTTP API bind address |

### Run

```bash
cd ~/logos-rln-membership-zone
set -a
. ./.env
set +a
RUST_LOG=warn,logos_blockchain=info cargo run -p zone-sequencer
```

What happens, in order:

1. First start writes the accredited key to `zone-sequencer/data/key.hex`. Keep it: the channel can
   only be written by this key. The channel id is `sha256(CHANNEL_NAME)`; there is no
   create step, the first inscription to an unused id creates the channel with the signing
   key as its only accredited key (`posting_timeframe` and `posting_timeout` 0: one writer).
2. The tree store `zone-sequencer/data/tree/` (sled) is opened: on a fresh start the tree is
   built by reading every LEZ block from `FIRST_LEZ_BLOCK_ID` to the head, so every
   registration made before the sequencer existed is applied and the first batch covers all
   of them; later starts continue from the block the store is at.
3. The Zone SDK backfills the channel from the node: from slot 0 the first time (30 to 60
   minutes on the testnet), from the checkpoint kept in the tree store after that (seconds
   to minutes).
   Then `ready`.
4. Every `POLL_SEC` the new LEZ blocks are applied; a batch goes out when there are
   changes, when `HEARTBEAT_SEC` passed without one, or when none was ever published; at
   most one batch in flight.
5. Each batch is `pending` from publish, `adopted` once in a Bedrock block, `finalized`
   once in finalized history (8 to 10 hours later, the log prints the L1 slot), or
   `orphaned` after a reorg, in which case it is published again.

Ctrl-C stops it. The next start opens the tree store and continues: the Zone SDK from its
checkpoint, the batches from the history, the tree from where it was, and only the LEZ
blocks closed since are read.
A batch still `pending` from the previous run is kept in flight, no second batch is published
over it. Events that happened while the sequencer was down are picked up the same way: the
next batch spans the whole gap.

### Data files

| File | Content | Lose it and |
| --- | --- | --- |
| `zone-sequencer/data/key.hex` | accredited key | the channel is lost; start a new one with a new `CHANNEL_NAME` |
| `zone-sequencer/data/tree/` | sled store: the tree, the reader's state, the published batches, the Zone SDK checkpoint | the sequencer starts fresh: backfill from slot 0, tree rebuilt from LEZ, and on a channel that already has batches the next batch chains on the root and is refused (below) |

Rule: never delete the tree store on a channel that has batches. The Zone SDK learns the
channel tip from finalized history only (8 to 10 hours behind on the testnet), so without
the checkpoint it chains the next batch on the root; the chain refuses it (`InvalidParent`),
silently, and every later batch too. The sequencer checks `GET /channel/:id` on the node at
start and refuses to run when the channel on chain does not match its data dir (another key,
a tip it does not know, or no batch at all); the fix is to restore the data dir or to start a
new channel.

### Start a new channel

When the channel holds batches you do not want to continue from (for example test batches
that never got mined), change the name and start clean:

```bash
sed -i 's/^CHANNEL_NAME=.*/CHANNEL_NAME=rln-membership-zone:testnet:v0.3.0-1/' .env
rm -rf zone-sequencer/data/tree
```

`key.hex` can stay. The old channel costs nothing abandoned.

### HTTP API

```bash
curl -s localhost:3000/ | jq                  # channel, LEZ head, covered block, counters, newest roots
curl -s localhost:3000/roots/5 | jq           # newest 5 distinct roots, the batch that produced each
curl -s localhost:3000/batches/5 | jq         # newest 5 batches as published, heartbeats included
curl -s localhost:3000/merkle_proof/0 | jq    # membership at leaf 0 and its proof path at the LEZ head
curl -s localhost:3000/merkle_proof/<id_commitment hex> | jq   # the same, by identity commitment
curl -s localhost:3000/leaves | jq            # active memberships with expiry, removed ones with why
curl -s localhost:3000/health
```

Words used the same way in every answer: `lez_block_id` and `lez_timestamp_ms` are the LEZ
block the tree is at (its timestamp is the clock expiry runs on) and `root` the tree's root
there; `status` is always a batch's status on Bedrock (`pending`, `adopted`, `orphaned`,
`finalized` with its `l1_slot`); a membership is `active` (its leaf is in the tree and it
proves, through its grace period included) or `removed` for `expired`, `slashed` or `erased`.
32-byte values are hex, little-endian.

`/` adds `zone_channel_id`, `lez_source`, `first_lez_block_id` and `covered_to_lez_block_id`
(the last block a published batch covers), `lez_events` (the program's events applied:
`registered`, `extended`, `slashed`, `erased`), `tree` (`active_leaves`, `removed_leaves`,
and `changes` the rules made beyond the one insert per `Registered`: `removed_expired` by the
LEZ clock, `removed_ended` by a slash or an erase while active, `reinserted` by an `Extended`
that landed after the reader had expired the leaf), `pending_changes` (waiting for the next
batch), `published_batches`, `newest_adopted` and `newest_finalized` (root and
`to_lez_block_id` of the newest batch in a Bedrock block, and in finalized history).

A batch entry (`/batches`, `/roots`) carries `root`, `from_lez_block_id`, `to_lez_block_id`,
`change_count`, `published_at_unix`, `status`, `tx_hash` and `msg_id`. `/batches` lists the
batches as published, so a heartbeat shows the previous root with `change_count` 0. `/roots`
lists distinct roots: each with the batch that produced it (its `change_count` is what made
the root, and it is the first of that root's batches to reach `adopted` and `finalized`) plus
`carried_to_lez_block_id`, the `to_lez_block_id` of the newest batch carrying the root.

`/merkle_proof` answers 404 for an unknown leaf index or identity commitment. The answer
carries the membership (`leaf_index`, `id_commitment`, `active`, `removed`,
`expiry_timestamp_ms`), `newest_adopted` and `newest_finalized` (for a validator to compare
with `root`), and `proof` (`rate_commitment`, `path_elements`, `identity_path_index`, the
`RLNMerkleProof` of Zerokit) while the membership is active, `null` otherwise.

## 4. LEZ source: the public LEZ Sequencer (default)

`LEZ_SEQUENCER_URL=https://testnet.lez.logos.co`. The Zone sequencer polls `getLastBlockId` and reads
`getBlockRange` (1024 blocks per call, closed blocks within seconds of being produced, no
finality wait). The RPC serves no events, so the program's events are derived from its
transactions: a block holds only transactions that succeeded, so every `Register` to the
program is a `Registered`, every `Slash` a `Slashed` of `poseidon(identity_secret)`, and so
on. The tree is the same as from the events; only the input differs from the RFC's.

The instructions are decoded with the registration program's own layouts crate
(`rln-layouts`: the `Instruction` enum, the membership timing helpers), so the sequencer never
parses them by hand. For reference, borsh, variants in declaration order, durations in
seconds; every value a handler needs travels in the instruction as a claim the chain asserts,
so a transaction in a block carries true values, and the leaf index is the config's
`next_leaf_index` at registration:

```text
0 Initialize { tree_id, price_per_unit u128, treasury_account_id [32],
               max_total_rate_limit u64, active_duration_sec u32, grace_period_sec u32 }
1 Register   { tree_id, id_commitment, rate_limit u64, now_ms u64, price_per_unit u128,
               active_duration_sec u32, grace_period_sec u32, leaf_index u64 }   Registered
2 Slash      { tree_id, id_commitment, identity_secret, rate_limit u64 }         Slashed
3 Extend     { tree_id, id_commitment, now_ms u64, price_per_unit u128, rate_limit u64,
               expiry_timestamp_ms u64 }                                         Extended
4 Erase      { tree_id, id_commitment, rate_limit u64, now_ms u64 }              Erased
```

The events are derived as the program's own `events` module defines them: `Registered` with
the expiry `now + active + grace` (the end of the grace period, from which the membership is
expired), `Extended` with the expiry the instruction claims, `Slashed`
(`poseidon(identity_secret)` must be the `id_commitment`, checked), `Erased`. The reader
applies them as the program's reference reader does: `Registered` inserts the leaf,
`Extended` keeps the new expiry and puts the leaf back if the reader had already expired it
(the chain accepts a clock claim a little behind its clock), `Slashed` and `Erased` end the
membership and remove the leaf if still active, and after each block every active leaf whose
expiry is at or before the block's timestamp is removed. The expiry is the end of the grace
period, so a membership stays in the tree and keeps proving through its grace period; only
the chain's view of it changes there (it can be extended, not yet erased). On the testnet a
membership lasts 30 days plus a 7-day grace period. A transaction for another `tree_id` of
the same program is dropped.

A transaction the decoder cannot read is reported with its bytes and skipped; the tree is then
missing a change until the decoder is fixed and the sequencer restarted once with
`--rebuild-tree` (the batch history and the checkpoint stay, the tree is read again from
LEZ). A change found again in a block after the last one covered goes into the next batch; a
change in a block already covered is in no batch's change list, so the next batch carries the
corrected root with a `change_count` that does not account for it (a Zone indexer replaying
the batches would reject it; the rebuild is an operator's repair, not a protocol path).

Check the source by hand:

```bash
curl -s -X POST https://testnet.lez.logos.co -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"getLastBlockId","params":[]}'
curl -s -X POST https://testnet.lez.logos.co -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"getChannelId","params":[]}'    # 0101...01, the LEZ channel
```

## 5. LEZ source: your own LEZ sequencer (follower)

Why: no dependency on the public endpoint, and the blocks come from your own re-execution of
the LEZ channel on Bedrock (the RFC's "trusts nobody" source) at L1 adoption latency, not
finality. A LEZ sequencer whose key holds no stake does not produce blocks (log: `no stake
entry for our own sequencer key; aborting block production`) but follows the channel and
serves the same JSON-RPC. It is a full LEZ node: RISC Zero execution, RocksDB, a few GB.

What you need from the LEZ team: the testnet genesis (the `genesis` section of their
sequencer config), the tag the testnet runs, and optionally `gossip.bootstrap_peers` to
receive blocks live instead of only through Bedrock.

```bash
cd ~
git clone --depth 1 --branch v0.3.1 https://github.com/logos-blockchain/logos-execution-zone
cd logos-execution-zone

# A home with its own wallet and Bedrock identity; no funds needed to follow.
tools/setup_sequencer.py ~/logos-rln-membership-zone/lez-sequencer/home \
    --node-url http://127.0.0.1:8080

# Replace the home's config with the template, then fill in the testnet genesis.
cp ~/logos-rln-membership-zone/lez-sequencer/sequencer_config.json \
   ~/logos-rln-membership-zone/lez-sequencer/home/sequencer_config.json
$EDITOR ~/logos-rln-membership-zone/lez-sequencer/home/sequencer_config.json

export RUST_LOG=warn,sequencer_service=info
cargo run --release --features prove -p sequencer_service -- \
    ~/logos-rln-membership-zone/lez-sequencer/home/sequencer_config.json \
    --home ~/logos-rln-membership-zone/lez-sequencer/home \
    --port 3040 --listen-address 127.0.0.1
```

`--features prove` executes the guest programs in process (no `r0vm` install). The first
build takes a while (RISC Zero prover). Then point the Zone sequencer at it:

```bash
sed -i 's#^LEZ_SEQUENCER_URL=.*#LEZ_SEQUENCER_URL=http://127.0.0.1:3040#' .env
```

Verify it follows: `getLastBlockId` on `127.0.0.1:3040` should approach the public one.
The home (`lez-sequencer/home/`) is gitignored.

## 6. Monitoring

```bash
# Zone sequencer
curl -s localhost:3000/ | jq
watch -n 10 'curl -s localhost:3000/roots/3 | jq -c ".roots[] | {to_lez_block_id, status, l1_slot}"'

# Node
curl -s localhost:8080/cryptarchia/info | jq '.cryptarchia_info | {slot, height, lib_slot, state}'
curl -s localhost:8080/wallet/$(yq .public_keys.PoWClaim ~/keystore.yaml)/balance | jq
curl -s localhost:8080/mempool/view | jq
curl -s localhost:8080/mantle/gas-prices | jq

# LEZ
lez() { curl -s -X POST "${LEZ_SEQUENCER_URL:-https://testnet.lez.logos.co}" -H 'content-type: application/json' -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\",\"params\":${2:-[]}}" | jq; }
lez getLastBlockId
lez getBlock '[50056]'          # base64 of the borsh block
lez getAccount '["HAxY9SiqUjJQHdtvazFDWFCxn1Jw1dtbUGio8DMC4bBw"]'
```

Numbers worth watching: the wallet balance before and after the first `adopted` (the real
fee per batch), `lib_slot` against `slot` (finality), the time from `pending` to `adopted`
(block production and mempool propagation).

## 7. Troubleshooting

- `Failed to fetch immutable blocks ... Failed to parse response` repeating: Zone SDK older
  than the node. Bump the `logos-blockchain` submodule to the node's tag.
- `unzip: not found` while building: `sudo apt-get install -y unzip`.
- `missing manifest in toolchain '1.98.1-...'`: a half-installed toolchain;
  `rustup toolchain uninstall 1.98.1 && rustup toolchain install 1.98.1`.
- A batch stays `pending` and `curl localhost:8080/mempool/view` does not list its tx hash:
  the node dropped it as invalid at block building (no log at the default level). Check
  `curl localhost:8080/channel/<channel id hex>`: a `tip_message` that is not your newest
  batch means the batch chained on the wrong parent (see the data files rule above); the
  start-up check now catches this. `curl localhost:8080/cryptarchia/transaction/<tx hash>`
  tells whether a transaction was mined.
- A batch stays `pending` for more than a few minutes: run the three node commands of
  section 6 (`mempool/view`, `gas-prices`, `cryptarchia/info`). A long `mempool/view` with
  the batch still in it means a congested mempool: block builders take it in FIFO order up to
  the block's gas limit, a tip buys nothing, and the pool evicts after 9 hours. Position in
  the queue: `curl -s localhost:8080/mempool/view | jq -r '.[]' | grep -n <tx hash prefix>`;
  the wait is roughly that position divided by the transactions per block, times 36 s. A
  resubmit of the same transaction keeps its place; a re-fund (new transaction) loses it,
  hence `STALE_REFUND_SLOTS=0`. Transaction gone from the
  mempool with no warning: the node dropped it (fee below the floor, or the funding note
  changed). Still in the mempool while `height` grows: producers do not see or do not accept
  it. `height` not growing: the testnet is stalled.
- `Re-funded stale tx A -> B` in the log: the Zone SDK rebuilt the fee transaction of an
  unmined batch; the message id stays, batches are tracked by it. Off by default
  (`STALE_REFUND_SLOTS=0`): with the SDK default window it happened at every finalized block
  and sent the batch to the back of the mempool queue each time.
- Two batches `pending` after a restart: fixed (the in-flight batch is restored from the
  history); if seen, start a new channel (section 3).
- WSL2: `ngrok http 3000` inside WSL; from a Windows browser `http://localhost:3000` works
  with the default localhost forwarding.

## 8. Cleanup

Zone sequencer data (keeps the key):

```bash
cd ~/logos-rln-membership-zone
rm -rf zone-sequencer/data/tree
```

Everything of the Zone sequencer, including the key and the build:

```bash
rm -rf ~/logos-rln-membership-zone/zone-sequencer/data ~/logos-rln-membership-zone/target
```

Own LEZ sequencer: `rm -rf ~/logos-rln-membership-zone/lez-sequencer/home` and, to free
the build, `rm -rf ~/logos-execution-zone/target` (or the whole clone).

A LEZ Indexer from earlier experiments (`poc/lez-indexer/`, no longer part of the repo):

```bash
pkill -f indexer_service
rm -rf ~/logos-rln-membership-zone/poc
```

## 9. Open points this setup exposes

- The LEZ Sequencer RPC serves no events; the LEZ Indexer serves them for finalized blocks
  only (8 to 10 hours). Asked of the LEZ team: `getEvents` on the Sequencer RPC.
- The program's expiry rule and the layouts of `Extend`, `Erase`, `Slash` are to be confirmed
  with the program's author; the decoder is a best-effort reading of the chain.
- On restart the Zone sequencer continues from its local history, not from the channel as
  the RFC wants; with the testnet's finality the channel alone would not show the recent
  batches.
- Testnet finality (8 to 10 hours) rules out "finalized" as the valid-root reference; the
  Zone sequencer serves the newest adopted root, finality is the later audit (RFC Q3).
