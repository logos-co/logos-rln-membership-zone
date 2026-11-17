# Setup: the RLN Membership Zone on the Logos testnet

Everything needed to run the Zone end to end: a Logos Blockchain testnet node, the Zone
sequencer (the `zone` binary of this repository) with its HTTP registry API, and the LEZ source it reads, either
the public LEZ Sequencer or one run next to it (`lez-sequencer/`). Written for one Linux box (WSL2 works) that already
runs the node; the Mac side is only where the code is edited.

## 1. What runs where

| Process | Port | Role |
| --- | --- | --- |
| Logos Blockchain node (Bedrock, L1) | 8080 | HTTP API: wallet, mempool, blocks. The Zone SDK publishes batches through it and pays fees from its wallet. Never expose it: the API has no auth and reaches the wallet. |
| Zone sequencer, `zone` | 3000 | Reads LEZ blocks, derives the tree, publishes one batch per change or heartbeat on its channel, serves `/roots`, `/merkle_proof`, `/leaves`. |
| Zone indexer, `zone --mode indexer` | 3001 | Optional, beside the sequencer: derives the same tree, reads the channel's finalized batches and checks each against it, serves the same API, a parked batch as `stall`. |
| LEZ Sequencer | 3040 or public | Source of closed LEZ blocks (`getBlockRange`). Default: `https://testnet.lez.logos.co`. Optional: your own, section 5. |

Data flow: LEZ Sequencer (blocks, transactions) -> Zone sequencer (tree, batches) ->
Bedrock node (channel) and HTTP API (Modules); Bedrock node (channel) -> Zone indexer
(checks) -> HTTP API.

## 2. Prerequisites on the Linux box

```bash
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libssl-dev clang cmake protobuf-compiler unzip git curl jq
sudo apt-get install -y libpcsclite-dev    # lez-rln bins (mint_payer, register_member): pcsc-sys via the LEZ wallet
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

The Zone SDK version must match the node release: the `logos-blockchain` crates in
`Cargo.toml` are git dependencies pinned to the tag of the node in use (0.3.2 today). A
newer node answers the backfill with blocks the older SDK cannot parse (`Failed to fetch
immutable blocks ... Failed to parse response`, repeating forever); bump the tag and
`rust-toolchain.toml` then.

## 3. Zone node: sequencer and indexer

### Install and configure

```bash
cd ~
git clone https://github.com/logos-co/logos-rln-membership-zone
cd logos-rln-membership-zone
cp .env.example .env
sed -i "s/^FUNDING_PK=.*/FUNDING_PK=$(yq .public_keys.PoWClaim ~/keystore.yaml)/" .env
```

`.env` keys (all also flags, see `cargo run -- --help`):

| Key | Default | Meaning |
| --- | --- | --- |
| `NODE_URL` | `http://127.0.0.1:8080` | Logos Blockchain node HTTP API |
| `FUNDING_PK` | | Node wallet key paying the batches (`PoWClaim`) |
| `MAX_TX_FEE` | 1000000 | Fee cap per publish, atomic units |
| `PRIORITY_FEE_PERCENT` | 12 | Reserve over the mandatory fee, the excess is a tip; block builders take the mempool FIFO, so it buys nothing on the testnet (SDK default 12) |
| `CHANNEL_NAME` | `rln-membership-zone:testnet:v0.3.2-3` | Channel id = `sha256(CHANNEL_NAME)`; `CHANNEL_ID` (64 hex) overrides |
| `PROGRAM_ACCOUNT_ID` | `f048...106a` | Registration program on the testnet LEZ (`HAxY9...4bBw` in base58) |
| `TREE_ID` | `47b2de4f...b4cf` | Registry the program serves, first field of every event |
| `FIRST_LEZ_BLOCK_ID` | 49431 | Block the program was deployed in; nothing before it is read |
| `LEZ_CHANNEL_ID` | `0101...01` | The LEZ channel on Bedrock: the LEZ blocks come from its inscriptions, through the Zone SDK (section 4) |
| `LEZ_SEQUENCER_URL` | `https://testnet.lez.logos.co` | LEZ Sequencer RPC that fills the finalized LEZ history fast, pinned by hash to the blocks Bedrock finalized (section 4 or 5) |
| `REBUILD_TREE` | false | Drop the tree and read it again from LEZ, keeping the Zone channel state and its checkpoint (after a decoder fix that had skipped transactions) |
| `POLL_SEC` | 10 | Seconds between publish checks (the sequencer) or judgments (the indexer) |
| `HEARTBEAT_SEC` | 3600 | Publish an empty batch after this long without one |
| `STALE_REFUND_SLOTS` | 0 | LIB slots an unmined batch waits before the Zone SDK re-funds it under a new transaction (0 disables). Off: the mempool is FIFO, a re-funded transaction goes to the back of the queue; the SDK default, 30, re-funded at every finalized block |
| `DATA_DIR` | `data` | `key.hex` (the channel key, the sequencer only) and the tree store `tree/`. Gitignored: copy `key.hex` outside the checkout after the first start, a deleted or re-cloned checkout takes the channel key with it |
| `HTTP_ADDR` | `127.0.0.1:3000` | HTTP API bind address |

### Run

```bash
cd ~/logos-rln-membership-zone
set -a
. ./.env
set +a
cargo run
```

The log goes to stdout through `tracing` with a timestamp and a level. Without `RUST_LOG` the
filter is `warn,zone=info,logos_blockchain=error`: the node's own lines at `info`, the Zone
SDK's at `error` only (its `Pending tx ... expired unmined; orphaned` warnings are noise on the
LEZ channel, whose inscriptions are not ours). `RUST_LOG=warn,zone=info,logos_blockchain=info`
shows the SDK at work.

What happens, in order:

1. First start writes the accredited key to `data/key.hex`. Keep it: the channel can
   only be written by this key. The channel id is `sha256(CHANNEL_NAME)`; there is no
   create step, the first inscription to an unused id creates the channel with the signing
   key as its only accredited key (`posting_timeframe` and `posting_timeout` 0: one writer).
2. The state under `data/` is opened: the tree store `data/tree/` (sled) with the tree of the
   LEZ blocks **finalized** on Bedrock, and the small files beside it (`reader.json`,
   `zone.json`, `final.jsonl`, `lez_view.json`). Every start finds the newest LEZ inscription
   at or below the node's LIB (reading the finalized L1 blocks backwards from it) and fills the
   tree up to that block with the LEZ Sequencer's `getBlockRange`, pinned by the hash chain to
   the block Bedrock finalized: from `FIRST_LEZ_BLOCK_ID` on a fresh start (a minute), from the
   block the store is at after a restart (nothing when it is there already).
3. Two Zone SDK readers follow Bedrock from the node. One follows the LEZ channel (section 4),
   always from the LIB: the LEZ blocks adopted since are folded over the finalized tree (the
   head) and kept in `lez_view.json`, the finalized ones move into the tree. Blocks the
   SDK skips (a stream it dropped and reopened, entries it does not echo after a restart) are
   read from the LEZ Sequencer and taken only when they chain on the head and the adopted
   block chains on them. The other follows the Zone channel:
   one with no message on chain yet starts at the LIB (seconds); one with history is walked
   from genesis the first time (30 to 60 minutes on the testnet; a `408 Request Timeout` on a
   backfill page means the node is slow, the SDK retries the page) and from the checkpoint kept
   in `zone.json` after that (seconds to minutes). Then `ready` and `LEZ channel ready`.
4. Every `POLL_SEC` the sequencer checks whether a batch is due: changes from LEZ blocks
   adopted on Bedrock and not covered yet, `HEARTBEAT_SEC` passed without a batch, or none ever
   published; at most one batch in flight. A batch is folded on the sequencer's own head before
   it is published, so nothing it would park itself reaches Bedrock.
5. The channel is kept the way LEZ's own `chain_state` keeps it: a final tier (the
   inscriptions Bedrock finalized, irreversible) and a view (the unfinalized lineage the Zone
   SDK reports, chained on the last final entry), the head being the final state folded over
   the view. A batch of ours enters the view at publish (`bedrock_status` `pending`, `/` names
   it `in_flight`, until the channel tip shows a Bedrock block holds it: `adopted`) and the
   final tier once in finalized history (`finalized` with its `l1_slot`, 8 to 10 hours later);
   a reorg that drops it from the view (the SDK's `Conflict`, `orphaned`) has it published
   again (the same message id when its parent is unchanged), counted in `orphaned_count`; when
   that publish fails the batch waits in `zone.json` and the tick publishes it before any new
   batch. Finality comes
   from the Zone SDK stream, and every 5 minutes a scan of the finalized L1 blocks from the
   anchor (the slot of the newest final entry) up to the LIB applies what the stream may have
   missed (a restart in between).

Ctrl-C stops it. The next start opens the state and continues: the Zone channel from its
checkpoint, final tier and view, the LEZ channel from the LIB, the finalized tree from where it
was with the adopted LEZ blocks folded over it again.
A batch still in flight from the previous run stays so, no second batch is published over it. Events that happened while the sequencer was down are picked up the same way: the
next batch spans the whole gap.

### Data files

| File | Content | Lose it and |
| --- | --- | --- |
| `data/key.hex` | accredited key (the sequencer; an indexer writes none, it signs nothing) | the channel is lost; start a new one with a new `CHANNEL_NAME` |
| `data/tree/`, `data/reader.json` | the tree of the finalized LEZ blocks (sled) and the reader's state (memberships, counters, the root after every block that changed the tree); one without the other is refused | rebuilt from LEZ on a fresh start |
| `data/zone.json`, `data/final.jsonl` | the Zone channel: view, stall, anchor, in-flight publishes, pending changes, the Zone SDK checkpoint (its channel notes left out: this node never spends them); one line per finalized batch, appended | the sequencer starts fresh: backfill from slot 0, and on a channel that already has batches the next batch chains on the root and is refused (below) |
| `data/lez_view.json` | the adopted LEZ blocks over the finalized tree | the SDK reports them again from the LIB, minutes |

Every file is small (bytes per member, per batch, per adopted block) and written only when it
changed; the node logs their sizes at start and warns when one passes a megabyte. The LEZ
channel keeps no checkpoint on purpose: the SDK's checkpoint carries every note the followed
channel owns on the ledger, which for the LEZ channel is LEZ's whole deposit set, megabytes
rewritten at every event.

Rule: never delete the state on a channel that has batches. The Zone SDK learns the
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
sed -i 's/^CHANNEL_NAME=.*/CHANNEL_NAME=rln-membership-zone:testnet:v0.3.2-x/' .env
rm -rf data/tree data/*.json data/*.jsonl
```

`key.hex` can stay. The old channel costs nothing abandoned. State written by an earlier node
version with another layout is retired the same way: new channel, new state, the key stays.

### Run an indexer

The same binary in `--mode indexer` trusts nobody about the batches. It follows the channel
exactly like the sequencer (the Zone SDK with a throwaway key that is not accredited, so it
only reads; a channel with history is walked from genesis once, no start slot to give; started
before the first batch it begins at the LIB like the sequencer) and reads LEZ exactly like the
sequencer, but judges the batches against the tree of the LEZ blocks **finalized** on Bedrock
only, so what it judges with can never be rewritten under it.
Every batch of the channel is folded the way LEZ folds a block, with the indexer as judge: it
must continue the tip (`from_lez_block_id`), its `changes` must replay to its `root_after`, and
that root must be the indexer's own tree at `to_lez_block_id`. The outcome is LEZ's
`AcceptOutcome`: `applied`; `parked` with the `error` (`kind` = `unexpected_block_id`,
`hash_mismatch`, `derivation_mismatch` or `deserialize`, LEZ's `BlockIngestError` names), after which
the tip is frozen and every later batch parks too, the first one recorded as the `stall`; or
`retryable_failure` (`lez_not_read`: the finalized LEZ blocks have not reached the batch's
`to` yet; every batch behind an undecided one waits with it), judged again at every LEZ poll,
about when the batch itself is final on L1. No
funds, no publishing: only the node's HTTP API and the LEZ source. Run it beside the
sequencer with its own data dir and port:

```bash
set -a; . ./.env; set +a
MODE=indexer DATA_DIR=data-indexer HTTP_ADDR=127.0.0.1:3001 cargo run
```

The outcomes stay inside the node, as in LEZ: `/roots` serves the batches that applied, `/`
serves `batch_count` (those) and `stall` (the first finalized batch that parked, with its
error; `null` while none did). A batch the indexer cannot judge yet (`lez_not_read`) and the
ones behind it are not served until it can, so an indexer's `/roots` trails the sequencer's by
LEZ finality; the live tree is `head.root` in every answer (`verify_proof --finalized-only`
takes only what the channel finalized).

An indexer is meant to run on and off: stopped for days, started to check what finalized
since. Nothing it decides depends on being up: the final tier is re-read from the anchor on
every start, the view comes back from the SDK, a batch it could not judge yet is judged when
LEZ finality reaches it, and a missed inscription cannot stick as a wrong outcome (the log says
`parked` for a finalized batch that failed, with the error).

### HTTP API

```bash
curl -s localhost:3000/ | jq                  # channel, LEZ head, covered block, counters, newest roots
curl -s localhost:3000/roots/5 | jq           # newest 5 roots: the changes that made each, its batches on Bedrock
curl -s localhost:3000/merkle_proof/0 | jq    # membership at leaf 0 and its proof path at the LEZ head
curl -s localhost:3000/merkle_proof/<id_commitment hex> | jq   # the same, by identity commitment
curl -s localhost:3000/leaves | jq            # active memberships with expiry, removed ones with why
curl -s localhost:3000/health
```

Words used the same way in every answer: `head` is where the tree is, `lez_block_id` and
`lez_timestamp_ms` the LEZ block applied last (its timestamp is the clock expiry runs on) and
`root` the tree's root there; a batch's `bedrock_status` is the Zone SDK's word for where it
stands on Bedrock: `pending` (published, no Bedrock block yet), `adopted` (in a block on the
current branch) or `finalized` (`l1_slot` the L1 slot that finalized it, `null` before); a
membership is `active` (its leaf is in the tree and it proves, through its grace period
included) or `removed` for `expired`, `slashed` or `erased`. 32-byte values are hex,
little-endian. Counts end in `_count`. Every key is always present: a value that does not
apply is `null`, never absent.

`/` adds `zone_channel_id`, `lez_source`, `first_lez_block_id`, `lez_events` (the program's
events applied: `registered`, `extended`, `slashed`, `erased`), `private_transaction_count`
(privacy-preserving transactions with a public effect under the registration program; they
carry no instruction and emit no event, so each changed the registry in a way this tree does
not have, and the reader logs a warning; the ones under other programs are skipped silently),
`tree` (`active_leaf_count`, `removed_leaf_count`, and `changes` the rules made beyond the one
insert per `Registered`: `removed_expired` by the LEZ clock, `removed_ended` by a slash or an
erase while active, `reinserted` by an `Extended` that landed after the reader had expired the
leaf), `pending_change_count` (waiting for the next batch), `batch_count` (batches applied,
final tier and view together), `stall` (section "Run an indexer"), `in_flight` (the message
id of our batch no Bedrock block holds yet, `null` otherwise), `orphaned_count` (publishes of
ours a reorg dropped, published again), `newest_published` and `newest_finalized` (root and
`to_lez_block_id` of the newest applied batch, any and finalized).

`/roots` lists the roots batches carry, newest first, one entry per run of batches carrying
the same root: the `root`, `to_lez_block_id` of the newest batch carrying it, and `batches`,
every batch of the run, the producing one first, each with `msg_id`, `parent_msg`,
`from_lez_block_id`, `to_lez_block_id`, `root`, `changes` (the tree changes it made in order:
`kind` `insert` with the `rate_commitment` inserted or `remove` with `null`, and the
`leaf_index`; the producing batch's made the root, heartbeats carry it further with none),
`bedrock_status` and `l1_slot`. The head's own root, ahead of the batches while changes wait,
is `head.root`. A root the tree comes back to (a leaf inserted then removed empties its slot
again) is a new entry, so the window is the newest roots, not the newest values. A validator
reads its policy off the producing batch's `changes`: the roots from the newest back to the
newest that removes a leaf are the ones no removed member can still prove under; older roots
in the window still hold that leaf.

`/merkle_proof` answers 404 for a leaf index or identity commitment neither the tree nor the
LEZ Sequencer knows. The answer carries the membership (`leaf_index`, `id_commitment`,
`active`, `registered_at_lez_block_id` and `registered_tx_hash`, `removed`,
`removed_at_lez_block_id` and `removed_tx_hash`: the LEZ blocks and transactions of its
registration and of the event that removed it, `null` for an expiry, `expiry_timestamp_ms`;
`/leaves` carries the same per membership), `newest_published` and `newest_finalized` (for a
validator to compare with `head.root`), `proof` (`rate_commitment`, `path_elements`,
`identity_path_index`, the `RLNMerkleProof` of Zerokit) while the membership is active, `null`
otherwise, and `lez_preview`: the newest event about it in a LEZ block the LEZ Sequencer
produced but Bedrock has not adopted yet (`lez_block_id`, `event` as the program names it,
`rln_registration::Registered` say, `tx_hash`), `null` when none. A membership only the preview knows answers with `active` false, no `proof` and the
preview: the registration is in LEZ, not on Bedrock yet (minutes as a rule; hours when LEZ
stops inscribing, see `lez.not_on_bedrock` in `/`). Nothing from the preview is ever batched.

## 4. LEZ source: the LEZ channel on Bedrock, and the public LEZ Sequencer for history

The LEZ Sequencer inscribes every LEZ block it produces in the LEZ channel on Bedrock
(`LEZ_CHANNEL_ID`, `0101...01` on the testnet; payload = LEZ's own borsh `Block`). The node
reads that channel through the Zone SDK, as it reads its own: a LEZ block **adopted** (its
inscription in a Bedrock block on the current branch, minutes after the LEZ Sequencer produced
it) is folded over the finalized tree into the head, a **finalized** one moves into the tree
store, a Bedrock reorg (`Conflict`) refolds the head from the finalized tree. A LEZ block the
LEZ Sequencer has only closed is never used: a LEZ Sequencer restart produces such blocks
again, possibly with other contents (seen on the testnet, about once every twenty blocks), and
a batch about one of them would be a statement on Bedrock that LEZ itself no longer makes.
The sequencer batches the adopted blocks, the indexer judges with the finalized ones.

`LEZ_SEQUENCER_URL=https://testnet.lez.logos.co` serves `getBlockRange` (1024 blocks per call)
for one job: the finalized history up to the LIB on a fresh store, and the blocks a finalized
inscription skipped over after a long stop. Those blocks are trusted only through the hash
chain: each must chain on the one before, and the block Bedrock finalized must chain on the
last of them, or the node refuses them. The blocks are decoded with LEZ's own `common` crate
(the header hash is checked against the contents, as the LEZ indexer does). Neither source
serves events, so the program's events are derived from its transactions: every `Register` to
the program is a `Registered`, every `Slash` a `Slashed` of `poseidon(identity_secret)`, and
so on. The tree is the same as from the events; only the input differs from the RFC's. Known
gap: a transaction the program reverted stays in the block (charged, nonce advanced) and
reads like a successful one; the fix is events from the LEZ Sequencer, asked for.

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

The events are derived into the program's own `rln_layouts::events` types (`RlnEvent`), as that
module defines them: `Registered` with the expiry `now + active + grace` (the end of the grace period, from which the membership is
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
`--rebuild-tree` (the Zone channel state and the checkpoint stay, the tree is read again from
LEZ). A change found again in a block after the last one covered goes into the next batch; a
change in a block already covered is in no batch's change list, so the next batch carries the
corrected root whose `changes` do not account for it (a Zone indexer replaying
the batches would park it; the rebuild is an operator's repair, not a protocol path).

Check the sources by hand:

```bash
curl -s -X POST https://testnet.lez.logos.co -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"getLastBlockId","params":[]}'   # the LEZ Sequencer's latest closed block
curl -s -X POST https://testnet.lez.logos.co -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"getChannelId","params":[]}'     # 0101...01, the LEZ channel
curl -s localhost:3000/ | jq .lez     # adopted_block_id (the head), finalized_block_id, adopted_block_count,
                                      # sequencer_block_id (the LEZ Sequencer's newest), not_on_bedrock (the gap:
                                      # LEZ blocks produced and not adopted yet; hundreds = LEZ stopped inscribing)
```

## 5. LEZ source: your own LEZ sequencer (follower)

Why: no dependency on the public endpoint for the finalized history, and a `getEvents` of your
own once that RPC exists; the live blocks already come from the LEZ channel on Bedrock, so
this buys trust in the history fill only. A LEZ sequencer whose key holds no stake does not produce blocks (log: `no stake
entry for our own sequencer key; aborting block production`) but follows the channel and
serves the same JSON-RPC. It is a full LEZ node: RISC Zero execution, RocksDB, a few GB.

What it costs and how it compares to the public endpoint:

- Latency: a follower sees a block once its inscription is in a Bedrock block (the channel's
  adopted view, before finality), so it trails the public sequencer by the L1 inclusion time
  of the inscription (Bedrock slot plus mempool) but leads the LEZ indexer, which waits for
  finality. Sequencer gossip carries transactions, slash approvals and channel-config
  messages, not blocks, so `gossip.bootstrap_peers` does not make a follower faster.
- Initial sync: the channel is replayed from its genesis, every block re-executed through
  RISC Zero (execution only, no proving). Expect hours for the testnet's history, not
  minutes; nothing is measured yet.
- Resources: a Bedrock node of your own (the one the Zone SDK already uses, port 8080), the
  RISC Zero toolchain for the build (the first build takes long; `cargo-risczero` and
  `r0vm` per the LEZ README), RocksDB for the chain, a few GB.
- Events: the sequencer RPC serves none at v0.3.x and v0.4.0-rc1, so a follower feeds the
  Zone sequencer exactly like the public endpoint does (section 4): the program's
  transactions, decoded with `rln-layouts`. A `getEvents` method mirroring the indexer's
  exists as a local LEZ branch (`feat/sequencer-rpc-events`, not proposed upstream yet); if
  it ever ships, the events become a second input of the same derivation.

Versions: run the tag the testnet runs. LEZ `dev` carries the v0.4 wire format
(`BlockBody.reverted`, public transactions as `PublicExecution`) and does not follow the
v0.3.x testnet channel.

What you need from the LEZ team: the testnet genesis (the `genesis` section of their
sequencer config) and the tag the testnet runs.

```bash
cd ~
git clone --depth 1 --branch v0.3.2 https://github.com/logos-blockchain/logos-execution-zone
cd logos-execution-zone

# The home from the template, then fill in the testnet genesis and `funding_key` (any public
# key of the Bedrock node's wallet: a follower inscribes nothing, the field is only required).
# The node writes its `bedrock_signing_key` into the home on first start; no funds needed to
# follow. (`tools/setup_sequencer.py` of v0.3.1 is gone in v0.3.2.)
mkdir -p ~/logos-rln-membership-zone/lez-sequencer/home
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
watch -n 10 'curl -s localhost:3000/roots/3 | jq -c ".roots[] | {root, to_lez_block_id, batches: [.batches[] | {status, l1_slot}]}"'

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

Numbers worth watching: the wallet balance before and after the first batch lands in a
Bedrock block (the real fee per batch), `lib_slot` against `slot` (finality), the time a
batch stays `in_flight` (block production and mempool propagation).

## 7. Troubleshooting

- `Failed to fetch immutable blocks ... Failed to parse response` repeating: Zone SDK older
  than the node. Bump the `logos-blockchain` git tag in `Cargo.toml` to the node's tag.
- `unzip: not found` while building: `sudo apt-get install -y unzip`.
- `missing manifest in toolchain '1.98.1-...'`: a half-installed toolchain;
  `rustup toolchain uninstall 1.98.1 && rustup toolchain install 1.98.1`.
- `batch [A..B] would not apply on our own head (...); stopping before publishing it`: the
  sequencer's next batch failed its own replay (the error says which check), a bug or a store
  from another configuration (`FIRST_LEZ_BLOCK_ID`, `TREE_ID`); the process stops and nothing
  reaches Bedrock. Fix the cause, start again.
- `no LEZ inscription in the N finalized L1 slots below the LIB`: `LEZ_CHANNEL_ID` is not the
  LEZ channel, or LEZ stopped inscribing; check with `getChannelId` (section 4).
- `LEZ blocks A..B read from the LEZ Sequencer under finalized block N` then `LEZ blocks A..B
  not read: ...; finalized block N waits for the next one`: the LEZ Sequencer is unreachable;
  the gap is filled again under the next finalized block, so a short outage heals itself.
  `the LEZ Sequencer's history does not lead to LEZ block N, which Bedrock finalized` stops the
  process with the store intact: that sequencer does not follow Bedrock, point
  `LEZ_SEQUENCER_URL` at one that does and start again.
- A batch stays `in_flight` and `curl localhost:8080/mempool/view` does not list its tx hash:
  the node dropped it as invalid at block building (no log at the default level). Check
  `curl localhost:8080/channel/<channel id hex>`: a `tip_message` that is not your newest
  batch means the batch chained on the wrong parent (see the data files rule above); the
  start-up check now catches this. `curl localhost:8080/cryptarchia/transaction/<tx hash>`
  tells whether a transaction was mined.
- A batch stays `in_flight` for more than a few minutes: run the three node commands of
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
- Two batches in flight after a restart: fixed (the in-flight batch is restored with the
  view); if seen, start a new channel (section 3).
- `LEZ read failed: getBlockRange: {"code":-32008,... "Response is too big"}` repeating:
  fixed (the span is halved down to one block); an older build needs the fix, not a restart.
- `LEZ channel reorg on Bedrock: the head is the finalized tree folded over N adopted
  block(s) again`: a Bedrock reorg moved or dropped LEZ inscriptions; the head follows the new
  branch. `batch msg M claims root R.. at LEZ block N, LEZ now gives S..` (WARN) after it
  means a batch already on Bedrock states a root the new branch contradicts (LEZ produced an
  orphaned block again with other contents): an indexer parks on it and every later one, only
  a new channel mends that.
- `LEZ blocks A..B read from the LEZ Sequencer under adopted block N`: the SDK skipped blocks
  (a `408 Request Timeout` on a large fetch after a long stop, or entries it holds but does
  not echo); they are filled from the LEZ Sequencer, pinned by the hash chain to block N.
  `the LEZ Sequencer's blocks do not lead to adopted LEZ block N (...); not applied` after it
  means its history is not the one Bedrock adopted: nothing applied, the next event tries again.
- `Pending tx ... expired unmined; orphaned` in a burst from the Zone SDK, then `the SDK reports
  a conflict on the channel, but the node's channel tip is still our newest entry`: the SDK
  could not see its restored entries mined (the node answered slowly) and declared them
  orphaned; the node's channel tip says otherwise, so nothing is published again. On the LEZ
  channel the same burst ends in `LEZ channel reorg on Bedrock ...` and the head is rebuilt
  from the finalized tree over what the SDK still holds, the gaps filled from the LEZ
  Sequencer.
- `LEZ block N finalized with hash H.., the view held G..`: the SDK's view and this node's
  disagree; the head is rebuilt from the finalized tree and refilled, nothing to do.
- The first batch of a channel carries a root but `no changes` in the log (empty
  `changes` in `/roots`): fixed (the pending changes are persisted with the tree; before, a
  restart between reading LEZ and publishing lost them). A channel whose first batch is
  like that cannot be replayed by an indexer: start a new one (section 3).
- `channel N final ..., M in view ... (checkpoint resumed)` right after changing
  `CHANNEL_NAME`: the old state is still there; stop, `rm -rf data/tree data/*.json
  data/*.jsonl` (keep `key.hex`), start again.
- `the final tier is frozen at batch msg M (L1 slot S): ...` (WARN) at start, or a batch
  `parked` in the log: a finalized batch did not apply on the final tier (its error says
  which check), every later one parks after it; `/` shows it as `stall`. On the indexer that
  is what it holds against the channel; on the sequencer it means its own batch was malformed. Only a
  new channel mends it.
- `LEZ unreachable` while filling history: network; the fill is tried again at the next
  finalized LEZ block, nothing to do unless it repeats for long.
- WSL2: `ngrok http 3000` inside WSL; from a Windows browser `http://localhost:3000` works
  with the default localhost forwarding.

## 8. Cleanup

Zone sequencer data (keeps the key), and the indexer's (nothing to keep, it reads the chain
again):

```bash
cd ~/logos-rln-membership-zone
rm -rf data/tree data/*.json data/*.jsonl
rm -rf data-indexer
```

Everything of the Zone node, including the key and the build:

```bash
rm -rf ~/logos-rln-membership-zone/data ~/logos-rln-membership-zone/data-indexer ~/logos-rln-membership-zone/target
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
  only (8 to 10 hours). A `getEvents` for the Sequencer RPC exists as a local LEZ branch
  (section 5), not proposed upstream yet.
- The program's expiry rule and the layouts of `Extend`, `Erase`, `Slash` are to be confirmed
  with the program's author; the decoder is a best-effort reading of the chain.
- On restart the Zone sequencer continues from its persisted view (the unfinalized lineage)
  plus the channel's finalized history, as LEZ's own sequencer does; the channel alone would
  not show the recent batches with the testnet's finality.
- The sequencer batches LEZ blocks adopted on Bedrock, minutes after LEZ closes them; a block
  LEZ only closed is not used, since a LEZ Sequencer restart produces it again (RFC Q6). A
  Bedrock reorg that orphans a LEZ inscription already batched stays open; LEZ setting
  `bedrock_status` to `Safe` on adoption would let a reader take this from the RPC.
- A transaction the program reverted stays in a LEZ block and reads like a successful one
  (section 4); events from the LEZ Sequencer would settle it.
- Testnet finality (8 to 10 hours) rules out "final" as the only valid-root reference; the
  Zone node serves every root a batch carries and marks the final ones, the consumer picks
  (RFC Q3).
