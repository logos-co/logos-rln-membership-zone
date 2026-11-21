# End-to-end scenarios on the testnet

Scripts that drive the whole stack as a user would: a LEZ payer, a registration, a slash, a
proof against the Zone's roots. Bash + curl + jq + python3, one network env file per target, the
lez-rln binaries for the chain side. Everything that holds funds or state lives under `E2E_STATE` (default
`e2e/state/<network>`, gitignored): the payer wallet holds real funds, back it up.

| Target | Bedrock | LEZ | Program | Zone | Use |
| --- | --- | --- | --- | --- | --- |
| `testnet` | the node on the box (synced, `Online`) | `https://testnet.lez.logos.co` | deployed | the sequencer on the box | by hand, before a release; real LGO |
| `local` | (to come) LEZ dev node | own sequencer | own deploy | own, fresh | every change; free |

## Prerequisites

- The Zone node running as the sequencer (the README, "Run it"), its `.env`
  naming the same program and tree as the network env file; the scenarios read its API.
- A `logos-lez-rln` checkout on branch `feat/rln-zone-events` (the deployed program) with
  `mint_payer`, `register_member` and `slash_member` built (`cargo build
  --release --bin mint_payer --bin register_member --bin slash_member` in
  its `lez-rln/`; a release build is found first, a debug one second). `LEZ_RLN_CHECKOUT`
  points to it, default `~/logos-lez-rln`.
- `curl`, `jq`, `python3`, `yq` (reads the node keystore).
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

## Scenarios

| Script | Checks |
| --- | --- |
| `scenarios/register.sh <env>` | registration -> leaf and proof at the zone -> root published in a batch; records the membership for `slash.sh` |
| `scenarios/proof.sh <env>` | waits until a batch carries the head root, then an RLN proof from `/merkle_proof` of the last registration (`generate_proof`) verifies against `/roots` (`verify_proof`); the same proof over another message is refused |
| `scenarios/slash.sh <env>` | slash of the last registration -> `removed: slashed`, no proof -> the root removing it published in a batch |
