#!/usr/bin/env bash
# e2e/local/scenarios/lez-stop.sh: the LEZ sequencer stops (no new LEZ blocks, nothing
# inscribed); the Zone waits, then catches up when LEZ is back.
#
#   bash e2e/local/scenarios/lez-stop.sh e2e/local/local.env
#
# The Wait response of the RFC: nothing to apply, nothing to publish. The LEZ sequencer is
# stopped; the Zone sequencer says once that the LEZ preview is not read, keeps answering,
# and its view of LEZ does not move. The LEZ sequencer is started again (stack.sh lez
# resumes its home); the Zone says it answers again, and a new registration goes through
# end to end, with the indexer agreeing.

set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ENV_FILE="${1:?usage: lez-stop.sh <local.env>}"
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
zone_wait_agree 300

mark=$(log_mark "$(zone_role_log sequencer)")
lez_seq_stop
say "zone: $(zone_wait_log sequencer "$mark" 'LEZ Sequencer preview not read' 120)"
seen=$(zone_field sequencer .lez.sequencer_block_id)
sleep 20
[ "$(zone_field sequencer .mode)" = sequencer ] || die "the zone does not answer with LEZ down"
[ "$(zone_field sequencer .lez.sequencer_block_id)" = "$seen" ] || die "the zone's LEZ sequencer block moved while LEZ was down"
say "zone: up, LEZ sequencer block stays at $seen"

lez_seq_start
say "zone: $(zone_wait_log sequencer "$mark" 'the LEZ Sequencer answers again' 180)"

read -r leaf_index id_commitment <<< "$(register_submit)"
membership_wait_batched "$id_commitment" 600 > /dev/null
zone_wait_agree 300
echo "LEZ_STOP leaf=$leaf_index registered_after_restart=true"
