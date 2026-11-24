#!/usr/bin/env bash
# e2e/local/scenarios/bedrock-pause.sh: the Bedrock node stops answering for a while (docker
# pause); every reader waits and resumes when blocks flow again.
#
#   BEDROCK_PAUSE_SEC=90 bash e2e/local/scenarios/bedrock-pause.sh e2e/local/local.env
#
# The dev node is paused for BEDROCK_PAUSE_SEC (default 90). Meanwhile the Zone sequencer
# must keep answering. After the unpause the node's LIB slot must move again and a new
# registration must go through end to end (the Zone SDK streams survived), with the indexer
# agreeing. With BEDROCK_PAUSE_SEC above 780 the Zone's own silence alarm is checked too:
# the warning after 10 min on both channels, and the recovery line afterwards.

set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ENV_FILE="${1:?usage: bedrock-pause.sh <local.env>}"
# shellcheck source=/dev/null
. "$ENV_FILE"
# shellcheck source=/dev/null
. "$ROOT/lib.sh"
# shellcheck source=/dev/null
. "$ROOT/local/ctl.sh"
[ "$NETWORK" = local ] || die "the local stack only"
PAUSE="${BEDROCK_PAUSE_SEC:-90}"

lez_rln_env
zone_wait_http sequencer 30
zone_wait_http indexer 30
zone_wait_agree 300
mark=$(log_mark "$(zone_role_log sequencer)")

bedrock_pause
trap 'bedrock_unpause' EXIT
elapsed=0
while [ "$elapsed" -lt "$PAUSE" ]; do
    sleep 30
    elapsed=$((elapsed + 30))
    [ "$(zone_field sequencer .mode 2>/dev/null)" = sequencer ] || die "the zone sequencer does not answer with Bedrock paused ($elapsed s)"
    [ "$(zone_field indexer .mode 2>/dev/null)" = indexer ] || die "the zone indexer does not answer with Bedrock paused ($elapsed s)"
    say "  $elapsed s paused: both readers answer"
done
trap - EXIT
bedrock_unpause
bedrock_wait_moving 180

if [ "$PAUSE" -ge 780 ]; then
    say "zone: $(zone_wait_log sequencer "$mark" 'no Bedrock block from the Zone SDK' 60)"
    say "zone: $(zone_wait_log sequencer "$mark" 'Bedrock blocks reach both Zone SDK streams again' 300)"
fi
if zone_log_has sequencer "$mark" 'that stream is dead'; then
    die "the Zone SDK stream died during the pause: $(log_since "$(zone_role_log sequencer)" "$mark" | grep -E -m1 'stream is dead')"
fi

read -r leaf_index id_commitment <<< "$(register_submit)"
membership_wait_batched "$id_commitment" 600 > /dev/null
zone_wait_agree 300
echo "BEDROCK_PAUSE seconds=$PAUSE leaf=$leaf_index registered_after=true"
