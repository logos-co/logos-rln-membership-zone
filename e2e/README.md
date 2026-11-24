# End-to-end scenarios

Scripts that drive the whole stack as a user would: a LEZ payer, a registration, a slash, a
proof against the Zone's roots, and, on the local stack, every failure the RFC answers. Bash +
curl + jq + python3, one env file per target, the lez-rln binaries for the chain side.

```text
e2e/
  lib.sh                 shared: state.json, memberships, the LEZ payer, the lez-rln bins, the Zone API
  scenarios/             the flow, on either target: register.sh, proof.sh, slash.sh <target.env>
  testnet/               the Logos testnet: testnet.env, provision.sh (payer, funding), README.md
  local/                 the whole stack on one machine: run.sh (one command), stack.sh, ctl.sh,
                         local.env, scenarios/ (the failure cases), README.md; clones, state and logs
                         land here, gitignored
```

| Target | Bedrock | LEZ | Program | Zone | Use |
| --- | --- | --- | --- | --- | --- |
| `testnet/` | the node on the box (synced, `Online`) | `https://testnet.lez.logos.co` | deployed | the sequencer on the box | by hand, before a release; real LGO, finality in hours |
| `local/` | a dev node in docker (the LEZ checkout's `bedrock/`) | a sequencer built from the LEZ checkout, on that node | deployed by `stack.sh` | own, fresh, plus an indexer | every change; free; finality in seconds; the failure cases |

Everything that holds funds or state lives under `E2E_STATE` (default `e2e/<target>/state`,
gitignored): the testnet payer wallet holds real funds, back it up.

## The flow scenarios (`scenarios/`, both targets)

They take the target's env file and read the Zone's API named in it (`ZONE_HTTP`); the lez-rln
bins (`mint_payer`, `register_member`, `slash_member`; `LEZ_RLN_CHECKOUT`, a release build
found first) send the LEZ transactions; the Zone bins `generate_proof` and `verify_proof`
(`cargo build --release --bins`) make and check the proof.

| Script | Checks |
| --- | --- |
| `scenarios/register.sh <env>` | registration -> leaf and proof at the zone -> root published in a batch; records the membership for `slash.sh` |
| `scenarios/proof.sh <env>` | waits until a batch carries the head root, then an RLN proof from `/merkle_proof` of the last registration (`generate_proof`) verifies against `/roots` (`verify_proof`); the same proof over another message is refused |
| `scenarios/slash.sh <env>` | slash of the last registration -> `removed: slashed`, no proof -> the root removing it published in a batch |

`register.sh` sends one `Register` from the payer, then watches `/merkle_proof/<id_commitment>`
for the leaf with the claimed index and a 20-element path, and `/roots` until a batch carries
that root (`bedrock_status` `pending` or `adopted` first, `finalized` later: seconds on the
local stack, hours on the testnet; a verifier trusts every root a batch carries unless it asks
for finalized ones only). The scripts say the minutes spent while they wait.

## Targets

- `testnet/README.md`: provision a funded payer (hours, once), then the flow by hand.
- `local/README.md`: `bash e2e/local/run.sh all` builds and runs everything, then every scenario,
  the flow and the failure cases, and leaves the logs under `e2e/local/logs/`.
