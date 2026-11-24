# The testnet target

The Zone against the Logos testnet, by hand, before a release. Real LGO, Bedrock finality in
hours. `testnet.env` names the network; `provision.sh` gives it a funded LEZ payer; the flow
scenarios of `e2e/scenarios/` then run with that env file. State lives under
`e2e/testnet/state/` (gitignored): the payer wallet holds real funds, back it up.

## Prerequisites

- The reader running as the sequencer on the box (the repository README, "Run it"), its
  `.env` naming the same program and tree as `testnet.env`; the scenarios read its API
  (`ZONE_HTTP`).
- The Bedrock node on the box, synced and `Online` (`BEDROCK_HTTP`): its wallet pays the
  deposit that funds the payer.
- A `logos-lez-rln` checkout on branch `feat/rln-zone-events` (the deployed program) with
  `mint_payer`, `register_member` and `slash_member` built (`cargo build --release --bin
  mint_payer --bin register_member --bin slash_member` in its `lez-rln/`; a release build is
  found first, a debug one second). `LEZ_RLN_CHECKOUT` points to it, default `~/logos-lez-rln`.
  On Debian and Ubuntu the bins need `libpcsclite-dev`.
- `curl`, `jq`, `python3`, `yq` (reads the node keystore).
- The Zone bins `generate_proof` and `verify_proof` built (`cargo build --release --bins`,
  the same build as the node), for `proof.sh`.

## Run

```bash
bash e2e/testnet/provision.sh payer     # mint the LEZ payer, no chain needed
bash e2e/testnet/provision.sh fund      # deposit DEPOSIT_AMOUNT from the node wallet, wait for the credit
bash e2e/testnet/provision.sh status    # node, L1 wallet, L2 payer, pending deposit, zone head
bash e2e/scenarios/register.sh e2e/testnet/testnet.env
bash e2e/scenarios/proof.sh e2e/testnet/testnet.env
bash e2e/scenarios/slash.sh e2e/testnet/testnet.env
```

`fund` needs the node `Online` and a note worth `DEPOSIT_AMOUNT` plus fee in its wallet; the
credit lands once the deposit is final on Bedrock, 8 to 10 hours on the testnet, so run it,
leave it, and re-run later (it resumes). A pending deposit the node no longer finds (its tx
index answers 404 for a mined tx too) is reported, not re-sent: `DEPOSIT_RESEND=1` deposits
again, only when the wallet still holds the note. A registration's batch is `pending` or
`adopted` within minutes and `finalized` hours later; a verifier trusts every root a batch
carries unless it asks for finalized ones only.

The failure cases (`e2e/local/scenarios/`) are not for the testnet: they stop and start the
processes, which here are the box's own.
