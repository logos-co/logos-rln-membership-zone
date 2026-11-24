#!/usr/bin/env bash
# e2e/local/scenarios/sequencer-restart.sh: the Zone sequencer is down while LEZ and Bedrock
# move on; it comes back, reconciles with its channel and catches up.
#
#   bash e2e/local/scenarios/sequencer-restart.sh e2e/local/local.env
#
# The sequencer is stopped (SIGTERM). A registration lands on LEZ and its block is adopted on
# Bedrock meanwhile (the indexer sees the membership active, with no batch yet). On restart
# the sequencer must find its channel tip as its own newest batch, apply the LEZ blocks it
# missed, and publish the batch carrying the registration, chained right after the old tip
# (`from_lez_block_id` = old `to_lez_block_id` + 1). The indexer agrees afterwards.

set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ENV_FILE="${1:?usage: sequencer-restart.sh <local.env>}"
# shellcheck source=/dev/null
. "$ENV_FILE"
# shellcheck source=/dev/null
. "$ROOT/lib.sh"
# shellcheck source=/dev/null
. "$ROOT/local/ctl.sh"
[ "$NETWORK" = local ] || die "the local stack only"

lez_rln_env
zone_wait_http sequencer 30
zone_wait_http indexer 30
[ "$(zone_field sequencer .batch_count)" -ge 1 ] || die "the sequencer published no batch yet: run register.sh first"
zone_wait_agree 300
old_to=$(zone_field sequencer .newest_published.to_lez_block_id)
say "newest batch ends at LEZ block $old_to"

zone_stop sequencer
read -r leaf_index id_commitment <<< "$(register_submit)"

say "the registration must reach Bedrock while the sequencer is down: watching the indexer"
for i in $(seq 1 60); do
    answer=$(zone_at indexer "/merkle_proof/$id_commitment" 2>/dev/null) || answer=""
    [ "$(printf '%s' "$answer" | jq -r '.active' 2>/dev/null)" = "true" ] && break
    [ $((i % 6)) -eq 0 ] && say "  $((i / 6)) min: $(membership_status "$answer")"
    sleep 10
done
[ "$(printf '%s' "$answer" | jq -r '.active' 2>/dev/null)" = "true" ] || die "the indexer did not see leaf $leaf_index active within 10 min"
[ "$(printf '%s' "$answer" | jq -r '.proof')" = "null" ] || die "the indexer has a path for leaf $leaf_index while the sequencer is down"
say "indexer: leaf $leaf_index active on Bedrock, no batch carries it (the sequencer is down)"

mark=$(log_mark "$(zone_role_log sequencer)")
zone_start sequencer
zone_wait_http sequencer 120
say "reconciled: $(zone_wait_log sequencer "$mark" 'chain: channel tip .* is our (newest|last landed) batch' 180)"

membership=$(membership_wait_batched "$id_commitment" 600)
to=$(printf '%s' "$membership" | jq -r '.proof.to_lez_block_id')
from=$(zone_at sequencer /roots/50 | jq -r --argjson to "$to" \
    '[.roots[].batches[] | select(.to_lez_block_id == $to and .superseded == false)] | first | .from_lez_block_id')
[ "$from" = "$((old_to + 1))" ] || die "the catch-up batch starts at LEZ block $from, the old tip ended at $old_to"
say "catch-up batch [$from..$to] chains on the old tip"
zone_wait_agree 300
echo "SEQUENCER_RESTART leaf=$leaf_index batch=[$from..$to]"
