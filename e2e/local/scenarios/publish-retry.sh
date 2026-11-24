#!/usr/bin/env bash
# e2e/local/scenarios/publish-retry.sh: the Zone sequencer cannot publish (its funding key
# holds nothing); the batch waits, nothing is lost, and it goes out once publishing works.
#
#   bash e2e/local/scenarios/publish-retry.sh e2e/local/local.env
#
# The sequencer restarts with FUNDING_PK set to an unfunded key. A registration lands on
# Bedrock: the membership turns active at the sequencer, but no batch can carry it; the log
# says once why the publish did not go through (tried again every poll), `in_flight` stays
# null and the batch count does not move. The sequencer restarts with its real funding key:
# the batch is published, the membership gets its path, the indexer agrees.

set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ENV_FILE="${1:?usage: publish-retry.sh <local.env>}"
# shellcheck source=/dev/null
. "$ENV_FILE"
# shellcheck source=/dev/null
. "$ROOT/lib.sh"
# shellcheck source=/dev/null
. "$ROOT/local/ctl.sh"
[ "$NETWORK" = local ] || die "the local stack only"
UNFUNDED_PK=1111111111111111111111111111111111111111111111111111111111111111

lez_rln_env
zone_wait_http sequencer 30
zone_wait_http indexer 30
zone_wait_agree 300
before=$(zone_field sequencer .batch_count)

zone_stop sequencer
mark=$(log_mark "$(zone_role_log sequencer)")
zone_start sequencer "FUNDING_PK=$UNFUNDED_PK"
zone_wait_http sequencer 120

read -r leaf_index id_commitment <<< "$(register_submit)"
for i in $(seq 1 60); do
    answer=$(zone_at sequencer "/merkle_proof/$id_commitment" 2>/dev/null) || answer=""
    [ "$(printf '%s' "$answer" | jq -r '.active' 2>/dev/null)" = "true" ] && break
    [ $((i % 6)) -eq 0 ] && say "  $((i / 6)) min: $(membership_status "$answer")"
    sleep 10
done
[ "$(printf '%s' "$answer" | jq -r '.active' 2>/dev/null)" = "true" ] || die "leaf $leaf_index not active at the sequencer within 10 min"
say "zone: $(zone_wait_log sequencer "$mark" 'publish failed|not ready to publish|does not fit one inscription' 300)"
sleep 30
[ "$(zone_at sequencer "/merkle_proof/$id_commitment" | jq -r '.proof')" = "null" ] || die "a batch carries leaf $leaf_index although the sequencer cannot pay"
[ "$(zone_field sequencer .in_flight)" = "null" ] || die "a publish is in flight although the sequencer cannot pay"
[ "$(zone_field sequencer .batch_count)" = "$before" ] || die "the batch count moved although the sequencer cannot pay"
say "unfunded: leaf $leaf_index active, no batch, batch count stays $before"

zone_stop sequencer
mark=$(log_mark "$(zone_role_log sequencer)")
zone_start sequencer
zone_wait_http sequencer 120
membership=$(membership_wait_batched "$id_commitment" 600)
say "funded again: leaf $leaf_index in a batch to LEZ block $(printf '%s' "$membership" | jq -r '.proof.to_lez_block_id')"
zone_wait_agree 300
echo "PUBLISH_RETRY leaf=$leaf_index batch_count=$(zone_field sequencer .batch_count)"
