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
| `ACTIVE_DURATION_MS` | unset | Membership lifetime after the timestamp a `Register` carries; unset = leaves never expire (section 4) |
| `POLL_SEC` | 10 | Seconds between LEZ polls |
| `HEARTBEAT_SEC` | 600 | Publish an empty batch after this long without one |
| `STALE_REFUND_SLOTS` | 0 | LIB slots an unmined batch waits before the Zone SDK re-funds it under a new transaction (0 disables). Off: the mempool is FIFO, a re-funded transaction goes to the back of the queue; the SDK default, 30, re-funded at every finalized block |
| `DATA_DIR` | `zone-sequencer/data` | Key, checkpoint, history, snapshots |
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
2. The tree is restored from `zone-sequencer/data/snapshots/` or, on a fresh start, built by reading
   every LEZ block from `FIRST_LEZ_BLOCK_ID` to the head: every registration made before the
   sequencer existed is applied, the first batch covers all of them.
3. The Zone SDK backfills the channel from the node: from slot 0 the first time (30 to 60
   minutes on the testnet), from `zone-sequencer/data/checkpoint.json` after that (seconds to minutes).
   Then `ready`.
4. Every `POLL_SEC` the new LEZ blocks are applied; a batch goes out when there are
   changes, when `HEARTBEAT_SEC` passed without one, or when none was ever published; at
   most one batch in flight.
5. Each batch is `pending` from publish, `adopted` once in a Bedrock block, `finalized`
   once in finalized history (8 to 10 hours later, the log prints the L1 slot), or
   `orphaned` after a reorg, in which case it is published again.

Ctrl-C stops it. The next start resumes: checkpoint for the channel, `history.json` for the
batches, snapshots for the tree, and only the LEZ blocks after the last batch are read again.
A batch still `pending` from the previous run is kept in flight, no second batch is published
over it. Events that happened while the sequencer was down are picked up the same way: the
next batch spans the whole gap.

### Data files

| File | Content | Lose it and |
| --- | --- | --- |
| `zone-sequencer/data/key.hex` | accredited key | the channel is lost; start a new one with a new `CHANNEL_NAME` |
| `zone-sequencer/data/checkpoint.json` | Zone SDK resume point (channel tip, pending txs) | backfill from slot 0 again; pending txs are forgotten |
| `zone-sequencer/data/history.json` | published batches: range, root, status, msg and tx hash | the sequencer thinks nothing was published and publishes from `FIRST_LEZ_BLOCK_ID` again; Zone indexers would reject the overlap. Do not delete it alone |
| `zone-sequencer/data/snapshots/<msg_id>.json` | reader state at each batch | the tree is replayed from LEZ, nothing lost |

Rule: never delete `checkpoint.json` on a channel that has batches. The Zone SDK learns the
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
rm -f zone-sequencer/data/checkpoint.json zone-sequencer/data/history.json
rm -rf zone-sequencer/data/snapshots
```

`key.hex` can stay. The old channel costs nothing abandoned.

### HTTP API

```bash
curl -s localhost:3000/ | jq                  # channel, LEZ head, covered block, batch count, tree counters
curl -s localhost:3000/roots/5 | jq           # newest 5 roots with status, msg_id, tx_hash, l1_slot
curl -s localhost:3000/merkle_proof/0 | jq    # proof path of leaf 0 at the newest adopted root, or at the head while none is in a block
curl -s "localhost:3000/merkle_proof/0?at=head" | jq        # at the tree of the LEZ head
curl -s "localhost:3000/merkle_proof/0?at=finalized" | jq   # at the newest finalized root
curl -s localhost:3000/leaf/<id_commitment hex> | jq
curl -s localhost:3000/leaves | jq            # every live leaf with expiry, removed leaves with cause
curl -s localhost:3000/health
```

`/merkle_proof` answers 404 when the index holds no leaf at the served root, 503 for
`?at=finalized` while nothing is finalized. The answer carries the `status` of the root:
`head` (the sequencer's own tree, nothing on Bedrock yet), `adopted` or `finalized`. `path_elements` and `identity_path_index` are the
`RLNMerkleProof` of Zerokit; `root` is the root to prove against.

Exposing the API: `ngrok http 3000` from the same box (inside WSL is fine). Do not expose
8080.

## 4. LEZ source: the public LEZ Sequencer (default)

`LEZ_SEQUENCER_URL=https://testnet.lez.logos.co`. The Zone sequencer polls `getLastBlockId` and reads
`getBlockRange` (1024 blocks per call, closed blocks within seconds of being produced, no
finality wait). The RPC serves no events, so the program's events are derived from its
transactions: a block holds only transactions that succeeded, so every `Register` to the
program is a `Registered`, every `Slash` a `Slashed` of `poseidon(identity_secret)`, and so
on. The tree is the same as from the events; only the input differs from the RFC's.

The transaction layout was read off the testnet (`zone-sequencer/src/lez.rs`, `decode_instruction`):

```text
0 InitConfig { tree_id, price_per_unit u128, account [32], registration_fee u64,
               grace_period_sec u32, withdrawal_delay_sec u32 }              no event
1 Register   { tree_id, id_commitment, rate_limit u64, registered_at_ms u64,
               deposit u128, grace_period_sec u32, withdrawal_delay_sec u32,
               leaf_index u64 }                                             Registered
2 Extend, 3 Erase, 4 Slash: assumed, RFC fields, none seen on chain yet
```

A `Register` carries the client's timestamp, not the expiry the program computes, so the
Zone sequencer cannot know when a leaf expires. `ACTIVE_DURATION_MS` sets the lifetime (expiry =
`registered_at_ms` + lifetime); unset, leaves never expire. To be replaced by the program's
rule once confirmed.

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
cat ~/logos-rln-membership-zone/zone-sequencer/data/history.json | jq

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
rm -f zone-sequencer/data/checkpoint.json zone-sequencer/data/history.json
rm -rf zone-sequencer/data/snapshots
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
