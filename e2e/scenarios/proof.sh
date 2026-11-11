#!/usr/bin/env bash
# e2e/scenarios/proof.sh: an RLN proof from the Zone's answer, verified against its roots.
#
#   bash e2e/scenarios/proof.sh <network.env> [leaf_index]
#
# The consumer side of the Zone, without the Module: the Zone's answers are fetched here
# (`/merkle_proof/{id_commitment}` for the leaf given, else the newest registration not
# slashed; `/roots/5`) and handed to the bins, which only compute: `generate_proof` proves
# membership with zerokit over that path, `verify_proof` checks the proof against those
# roots, adopted on Bedrock at least. A proof over another message must then fail: same
# roots, wrong signal.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE="${1:?usage: proof.sh <network.env> [leaf_index]}"
# shellcheck source=/dev/null
. "$ENV_FILE"
# shellcheck source=/dev/null
. "$ROOT/lib.sh"

read -r leaf_index id_commitment identity_secret <<< "$(membership_pick "${2:-}")"
[ -n "${identity_secret:-}" ] || die "no registration to prove on record${2:+ at leaf $2}: run register.sh first"

generate_proof=$(zone_bin generate_proof)
verify_proof=$(zone_bin verify_proof)
proof_file="$E2E_STATE/proof-$leaf_index.json"
signal="e2e $(date -u +%FT%TZ)"

say "fetching the Zone's path for leaf $leaf_index"
ZONE_MERKLE_PROOF=$(zone "/merkle_proof/$id_commitment") || die "the zone does not know $id_commitment"
export ZONE_MERKLE_PROOF
[ "$(printf '%s' "$ZONE_MERKLE_PROOF" | jq -r '.active')" = "true" ] || die "leaf $leaf_index is not active"
root=$(printf '%s' "$ZONE_MERKLE_PROOF" | jq -r '.head.root')

# The path is over the root at the LEZ head; the verifier below trusts a root only once a
# Bedrock block holds its batch, so a root the sequencer has just computed is waited for here.
say "the path is over root $root; waiting until a Bedrock block holds it (verify_proof trusts adopted roots)"
root_wait_adopted '.root == $r' --arg r "$root" > /dev/null \
    || die "root $root was not adopted within 30 minutes; the proof would be refused as pending"

say "proving membership of leaf $leaf_index"
RLN_IDENTITY_SECRET="$identity_secret" RLN_ID_COMMITMENT="$id_commitment" \
    "$generate_proof" --signal "$signal" --out "$proof_file" >&2 || die "generate_proof failed"

say "fetching the Zone's roots and verifying (adopted at least)"
ZONE_ROOTS=$(zone /roots/5) || die "cannot read the zone's roots"
export ZONE_ROOTS
"$verify_proof" --proof "$proof_file" --min-status adopted >&2 \
    || die "verify_proof refused a proof over the Zone's own path (its verdict is above)"

say "verifying a tampered proof (another message, same proof) must fail"
tampered="$E2E_STATE/proof-$leaf_index-tampered.json"
jq '.signal = "not the signal the proof binds"' "$proof_file" > "$tampered"
if "$verify_proof" --proof "$tampered" --min-status adopted >&2; then
    die "verify_proof accepted a proof over another message"
fi

echo "LEAF_INDEX=$leaf_index ID_COMMITMENT=$id_commitment PROOF_FILE=$proof_file VERIFIED=true TAMPERED_REFUSED=true"
