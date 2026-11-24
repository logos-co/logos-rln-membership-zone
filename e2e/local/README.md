# The local target: the whole stack on one machine

`run.sh` builds and runs everything the Zone needs, under this folder, and drives every
scenario against it: a Bedrock dev node in docker, a LEZ v0.3.2 sequencer built from source on
that node, the registration program deployed, a Zone sequencer and a Zone indexer. Nothing
costs LGO, Bedrock finalizes in seconds, a LEZ block closes every 2 s. One command, a coffee,
and a folder of logs: the way to check a change before it meets the testnet.

```text
e2e/local/
  run.sh            one command for all of it; each step on its own too
  stack.sh          the stack: check, bedrock, payer, lez, program, zone-env, status, down [-v]
  ctl.sh            start, stop and watch the processes from a scenario (zone roles, LEZ sequencer, Bedrock)
  local.env         the target: ports, payer limits, the program and tree as deployed (from state/state.json)
  scenarios/        the failure cases, each on its own
  lez/ lez-rln/     the clones run.sh builds from        (gitignored)
  state/            payer wallet, state.json, the LEZ sequencer home, zone.env, the zone data per role (gitignored)
  logs/             every component's log, every scenario's log (gitignored)
```

## Prerequisites

- git, docker (running), a Rust toolchain with cargo, `cargo risczero` (RISC Zero's cargo
  plugin, for the program's guest: `rzup install cargo-risczero`,
  <https://dev.risczero.com/api/zkvm/install>), `jq`, `python3`, `curl`, `lsof`.
- Access to the two source repositories: `logos-blockchain/logos-execution-zone` (tag
  `v0.3.2`) and `logos-co/logos-lez-rln` (branch `feat/rln-zone-events`, the deployed
  program). `LEZ_SRC` and `LEZ_RLN_SRC` point elsewhere, a URL or a local checkout.
- The LEZ repositories pin `rustc 1.98.1`; rustup downloads it on the first build unless
  `RUSTUP_TOOLCHAIN` names an installed toolchain to use instead.
- Disk: about 7 GB for the two clones with their builds, plus the Zone's own `target/`.
- macOS or Linux; `bash` 3.2 is enough.

## Run

```bash
bash e2e/local/run.sh all              # everything below in order, the stack stays up
bash e2e/local/run.sh status           # where everything stands
bash e2e/local/run.sh down             # stop, wipe the chain and the state; logs stay
bash e2e/local/run.sh purge            # down, then remove the clones and their builds too
```

`all` is `clean-logs` (empties `logs/`; the zone logs append across runs otherwise), `prepare`
(tools, clones), `build` (guest in docker, lez-rln bins, Zone bins), `bedrock` (the dev node),
`up` (payer, LEZ sequencer, program, `zone.env`), `zone` (the sequencer on `:3000`, the indexer
on `:3001`), `scenarios`, `logs`; its own output goes to `logs/run-all.log` too. Each step
resumes on its own: a clone, a built binary, a running node or a published program is kept. The first run
builds for a long time (the guest in docker, the LEZ sequencer, the Zone: half an hour on a
laptop); the second starts in a minute. `bedrock` after a `down` builds the node's docker image
again (`down` wipes it with the chain), ten minutes.

`scenarios` runs, in order and each to its own log, the flow (`register`, `proof`, `slash`,
from `e2e/scenarios/`) and the failure cases (`scenarios/` here), continues past a failure and
exits nonzero if one failed; `bash e2e/local/run.sh scenarios register sequencer-kill` runs the
ones named. A whole pass takes about seven minutes:

```text
[15:45:36]   PASS in 13 s: ID_COMMITMENT=7dee.. LEAF_INDEX=2 ROOT=bd9a.. BATCH=finalized
[15:46:16]   PASS in 23 s: SEQUENCER_RESTART leaf=3 batch=[687..698]
...
[16:33:06]   PASS in 37 s: WRONG_BATCH_CORRECTION wrong=[81..1862] correction=[81..1863] restarts=0 superseded=22 leaf=8
[16:33:08] scenarios: 11 passed
```

By hand, without `run.sh`: `bash e2e/local/stack.sh all` brings the stack up from the two
checkouts (`LEZ_CHECKOUT`, `LEZ_RLN_CHECKOUT`, default `lez/` and `lez-rln/` here; lez-rln's
`run_setup` built and its guest binary staged, `cargo risczero build --manifest-path
methods/guest/Cargo.toml` in `lez-rln/`), `set -a; . e2e/local/state/zone.env; set +a; cargo
run` starts the Zone sequencer, `MODE=indexer DATA_DIR=... HTTP_ADDR=127.0.0.1:3001` an
indexer, then `bash e2e/scenarios/register.sh e2e/local/local.env` and the others. `stack.sh
down -v` wipes the chain, the sequencer home, the payer and `state.json`. Every step resumes.

## The failure cases (`scenarios/`)

Each takes `e2e/local/local.env` and drives the processes itself through `ctl.sh`: the Zone
sequencer, the indexer and, for one scenario, a `rogue` second sequencer (`ZONE_BIN`, default
the release build; data under `state/zone-<role>`, logs appended to `logs/zone-<role>.log`
across restarts, a `==== start` line per start), the LEZ sequencer (`stack.sh lez` resumes it)
and the Bedrock dev node (`docker compose pause`). Each ends with the indexer agreeing with the
sequencer: same batch count, newest root and roots, no stall (a stall on its way out, a wrong
batch whose correction lands a tick later, is waited for and said). Run them after
`register.sh`, one at a time; each registers once more.

| Script | Case | Checks |
| --- | --- | --- |
| `indexer-agrees.sh` | the optimistic flow, judged | a second reader applied every batch: same count, roots, path for the newest membership |
| `sequencer-restart.sh` | the sequencer is down while LEZ and Bedrock move | the indexer sees the registration active with no batch; on restart the sequencer reconciles (`chain: channel tip .. is our newest batch`) and the catch-up batch starts right after the old tip |
| `sequencer-kill.sh` | SIGKILL right after a registration | the restart recovers, drops a publish that never chained if any, inserts the leaf in exactly one applied batch, never hits `would not apply` |
| `indexer-rebuild.sh` | `REBUILD_TREE=true` on the indexer | `tree store dropped` in the log; the tree derived again from LEZ brings the indexer back to what the sequencer says now, every root it had still listed; no stall at the end (a parked batch no correction answered yet is judged again; one a correction answered stays parked) |
| `publish-retry.sh` | the sequencer cannot pay (`FUNDING_PK` unfunded) | the membership turns active, no batch, `in_flight` null, the log says once why; funded again, the batch goes out |
| `lez-stop.sh` | the LEZ sequencer stops | `LEZ Sequencer preview not read` once, the zone answers and its LEZ view stands still; LEZ back: `answers again`, a registration goes through |
| `bedrock-pause.sh` | the Bedrock node paused `BEDROCK_PAUSE_SEC` (default 90) | both readers answer meanwhile, the LIB slot moves again, a registration goes through; above 780 s the silence warning and its recovery line too |
| `wrong-batch-correction.sh` | a wrong batch under the sequencer's key: a `rogue` sequencer on a copy of its data dir, rebuilt with a `TREE_ID` LEZ never carried, publishes the empty root over every block | the sequencer takes the entry into its view and publishes a correction from the wrong batch's `from` (`correction batch [`), the roots it re-covers superseded; the indexer parks the wrong batch when Bedrock finalizes it, then applies the correction, `stall` null; a registration after it is batched on both. A sequencer that stops on a stale SDK tip instead is restarted once; the output says `restarts=` |

After a correction the two readers count differently and agree anyway: the wrong batch stays
parked on the indexer that judged it (its error, the evidence; `parked_count`) and is superseded
on the sequencer that applied it unjudged (`superseded_count`); neither lists its root, and
`superseded_count + parked_count` is the same on both, which `ctl.sh` checks.

Not reproducible with one validator: a Bedrock reorg (Rebuild, the covered-reorg Correction, a
batch orphaned and republished); they stay unit-tested.

## The dev node's epoch

As shipped the dev node rotates its epoch every 100 slots. Bedrock re-prices storage gas at
every rotation and a transaction must pay the price of the epoch that mines it, so every
inscription pending across a rotation is dropped (`with 0 transactions (1 removed)` in the
node log); LEZ v0.3.2 then waits `stale_refund_slots` (300) and rewinds its head. Every 100 s
that stalls the whole stack for five minutes. `run.sh prepare` sets `epoch_config` in
`lez/bedrock/deployment-settings.yaml` to `108 / 108 / 144`, one epoch an hour, finality
unchanged (5 s). The same mechanism is why a Zone sequencer on the testnet should not keep
`STALE_REFUND_SLOTS` at 0 across an epoch rotation.

## Troubleshooting

- `features[@]: unbound variable` from `run_sequencer.sh`: bash 3.2 and an empty
  `LEZ_FEATURES`; `stack.sh` passes `rpc` (the default feature), so this means an old harness.
- The LEZ sequencer's `prove` feature needs Metal and Xcode on macOS; the stack runs it with
  `RISC0_DEV_MODE=1` and the `rpc` feature only.
- A scenario `FAIL` names its log; the zone logs in `logs/zone-<role>.log` carry the
  timestamps to line up with it. `run.sh status` says what is up.
- Port in use (`3000`, `3001`, `3002`, `3040`, `18080`): a previous stack is still up, `down`
  it.
- `docker compose pause` left the node paused (a killed `bedrock-pause` run):
  `(cd e2e/local/lez/bedrock && docker compose unpause)`.
