# End-to-end scenarios on the testnet

Scripts that drive the whole stack as a user would: a LEZ payer, a registration, a slash, a
proof against the Zone's roots. Bash + curl + jq + python3, one network env file per target, the
lez-rln binaries for the chain side. Everything that holds funds or state lives under `E2E_STATE` (default
`e2e/state/<network>`, gitignored): the payer wallet holds real funds, back it up.

| Target | Bedrock | LEZ | Program | Zone | Use |
| --- | --- | --- | --- | --- | --- |
| `testnet` | the node on the box (synced, `Online`) | `https://testnet.lez.logos.co` | deployed | the sequencer on the box | by hand, before a release; real LGO |
| `local` | a dev node in docker (the LEZ checkout's `bedrock/`) | a sequencer built from the LEZ checkout, on that node | deployed by `local-stack.sh` | own, fresh | every change; free; finality in seconds |

## Prerequisites

- The reader running as the sequencer (the README, "Run it"), its `.env`
  naming the same program and tree as the network env file; the scenarios read its API.
- A `logos-lez-rln` checkout on branch `feat/rln-zone-events` (the deployed program) with
  `mint_payer`, `register_member` and `slash_member` built (`cargo build
  --release --bin mint_payer --bin register_member --bin slash_member` in
  its `lez-rln/`; a release build is found first, a debug one second). `LEZ_RLN_CHECKOUT`
  points to it, default `~/logos-lez-rln`.
- `curl`, `jq`, `python3`, `yq` (reads the node keystore; testnet only).
- For the `local` target: docker, `lsof`, the LEZ checkout at v0.3.2 and lez-rln's `run_setup`
  with the guest binary (section Local).
- The zone bins `generate_proof` and `verify_proof` built (`cargo build --release --bins`, the
  same build as the node), for `proof.sh`.

## Testnet

```bash
bash e2e/provision-testnet.sh payer     # mint the LEZ payer, no chain needed
bash e2e/provision-testnet.sh fund      # deposit DEPOSIT_AMOUNT from the node wallet, wait for the credit
bash e2e/provision-testnet.sh status    # node, L1 wallet, L2 payer, pending deposit, zone head
bash e2e/scenarios/register.sh e2e/testnet.env
```

`fund` needs the node `Online` and a note worth `DEPOSIT_AMOUNT` plus fee in its wallet; the
credit lands once the deposit is final on Bedrock, 8 to 10 hours on the testnet, so run it,
leave it, and re-run later (it resumes). A pending deposit the node no longer finds (its tx index
answers 404 for a mined tx too) is reported, not re-sent: `DEPOSIT_RESEND=1` deposits again,
only when the wallet still holds the note. `register.sh` sends one `Register` from the payer,
then watches `/merkle_proof/<id_commitment>` for the leaf with the claimed index and a
20-element path, and `/roots` until a batch carries that root (`bedrock_status` `pending` or
`adopted` first, `finalized` hours later;
a verifier trusts every root a batch carries unless it asks for finalized ones only). The
scripts say the minutes spent while they wait.

## Local

The whole stack on one machine, built and run by `local-stack.sh` from the two checkouts
(`LEZ_CHECKOUT`, default `~/logos-execution-zone` at v0.3.2, and `LEZ_RLN_CHECKOUT`); docker
for the Bedrock dev node, `run_setup` of lez-rln built and its guest binary staged
(`cargo risczero build --manifest-path methods/guest/Cargo.toml` in `lez-rln/`, docker). The
dev node finalizes in seconds (`security_param` 5, 1 s slots), the LEZ sequencer closes a block
every 2 s and inscribes each one, the payer is funded at the LEZ genesis, so nothing waits for
hours and nothing costs real LGO.

```bash
bash e2e/local-stack.sh all          # check, bedrock (docker), payer, lez (builds, long once), program, zone-env
set -a; . e2e/state/local/zone.env; set +a; cargo run           # the Zone sequencer, in another shell
bash e2e/scenarios/register.sh e2e/local.env                    # then slash.sh, proof.sh the same way
bash e2e/local-stack.sh status
bash e2e/local-stack.sh down         # stop; `down -v` wipes the chain, the sequencer home, the payer and state.json
```

Every step resumes: a dev node that answers, a payer on record, a sequencer on its port or a
program in `state.json` is kept. `down -v` then `all` starts a fresh chain (the Zone's
`data-local` must go too). The LEZ sequencer runs in the background with its log under
`e2e/state/local/`; the first `lez` step builds it from the checkout, which takes long.

## Scenarios

| Script | Checks |
| --- | --- |
| `scenarios/register.sh <env>` | registration -> leaf and proof at the zone -> root published in a batch; records the membership for `slash.sh` |
| `scenarios/proof.sh <env>` | waits until a batch carries the head root, then an RLN proof from `/merkle_proof` of the last registration (`generate_proof`) verifies against `/roots` (`verify_proof`); the same proof over another message is refused |
| `scenarios/slash.sh <env>` | slash of the last registration -> `removed: slashed`, no proof -> the root removing it published in a batch |
