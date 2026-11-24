#!/usr/bin/env bash
# e2e/local/scenarios/indexer-agrees.sh: the optimistic flow, judged by a second reader.
#
#   bash e2e/local/scenarios/indexer-agrees.sh e2e/local/local.env
#
# The Zone sequencer published batches (register.sh ran); an indexer beside it read the same
# Bedrock chain and judged every batch. Both must say the same: batch count, newest published
# root, the roots batches carry, no stall (nothing parked), nothing superseded, and the same
# path for the newest membership on record.

set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ENV_FILE="${1:?usage: indexer-agrees.sh <local.env>}"
# shellcheck source=/dev/null
. "$ENV_FILE"
# shellcheck source=/dev/null
. "$ROOT/lib.sh"
# shellcheck source=/dev/null
. "$ROOT/local/ctl.sh"
[ "$NETWORK" = local ] || die "the local stack only"

zone_wait_http sequencer 30
zone_wait_http indexer 30
[ "$(zone_field sequencer .batch_count)" -ge 1 ] || die "the sequencer published no batch yet: run register.sh first"
zone_wait_agree 300

read -r leaf_index id_commitment _ <<< "$(membership_pick || true)"
if [ -n "${id_commitment:-}" ]; then
    seq=$(zone_at sequencer "/merkle_proof/$id_commitment" | jq -c '{leaf_index, active, root: .proof.root, to: .proof.to_lez_block_id}')
    idx=$(zone_at indexer "/merkle_proof/$id_commitment" | jq -c '{leaf_index, active, root: .proof.root, to: .proof.to_lez_block_id}')
    [ "$seq" = "$idx" ] || die "leaf $leaf_index differs: sequencer $seq, indexer $idx"
    say "leaf $leaf_index: both answer $seq"
fi
echo "INDEXER_AGREES batch_count=$(zone_field indexer .batch_count) root=$(zone_field indexer .newest_published.root)"
