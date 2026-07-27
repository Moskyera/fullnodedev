#!/bin/sh
# Block until a Hacash fullnode answers, then exit 0.
#
# Why this exists: systemd's After= only orders the START of two units, it does
# not wait for the first one to be ready. hbit-pool-server refuses to start when
# the node is not answering (correctly: it will not serve work against a node
# that is not there), so on a reboot, and on the first boot of a box whose chain
# is still syncing, the pool would fail and be restarted until the node caught
# up. This turns that into one wait with one message.
#
# It never prompts and never reads a secret. Everything it needs is an argument.
#
# usage: hbit-wait-for-node.sh <node_base_url> [seconds_to_wait]
#   <node_base_url>    e.g. http://127.0.0.1:8080 - the node's API, which is the
#                      [server] listen port in its hacash.config.ini.
#   [seconds_to_wait]  give up after this many seconds and exit non-zero, so
#                      systemd restarts the unit and the wait starts again.
#                      Default 3600. 0 means wait forever.
set -eu

NODE=${1:-}
LIMIT=${2:-3600}

if [ -z "$NODE" ]; then
	echo "hbit-wait-for-node.sh: no node URL given." >&2
	echo "What to do: pass the node's API base URL, e.g." >&2
	echo "  hbit-wait-for-node.sh http://127.0.0.1:8080 3600" >&2
	exit 2
fi

case "$LIMIT" in
*[!0-9]* | "")
	echo "hbit-wait-for-node.sh: [seconds_to_wait] must be a whole number (got '$LIMIT')." >&2
	exit 2
	;;
esac

# Strip a trailing slash so the URL below never becomes a double slash.
NODE=$(printf '%s' "$NODE" | sed 's:/*$::')

if ! command -v curl >/dev/null 2>&1; then
	echo "hbit-wait-for-node.sh: curl is not installed, so this cannot tell whether" >&2
	echo "the node is up, and the pool must not be started blind." >&2
	echo "What to do: apt-get install -y curl   (or dnf install -y curl), then" >&2
	echo "  systemctl restart hbit-pool" >&2
	exit 2
fi

WAITED=0
STEP=5
while :; do
	# -f makes a non-2xx answer a failure, --max-time keeps one hung request from
	# becoming a hung service. The body is discarded except for the readiness
	# check: /query/latest answers {"height":N,"diamond":N} when the node is up.
	if curl -fsS --max-time 5 "$NODE/query/latest" 2>/dev/null | grep -q '"height"'; then
		echo "node at $NODE is answering after ${WAITED}s; starting the pool"
		exit 0
	fi
	if [ "$LIMIT" -ne 0 ] && [ "$WAITED" -ge "$LIMIT" ]; then
		echo "no Hacash fullnode answered at $NODE after ${WAITED}s." >&2
		echo "The pool was NOT started, so nothing was mined and nothing was paid." >&2
		echo "What to do: systemctl status hacash-node, and journalctl -u hacash-node -n 50" >&2
		exit 1
	fi
	# One line a minute, not one every 5 seconds: this can legitimately run for
	# hours on a first sync, and a journal full of dots helps nobody.
	if [ $((WAITED % 60)) -eq 0 ]; then
		echo "waiting for the Hacash fullnode at $NODE (${WAITED}s so far)"
	fi
	sleep "$STEP"
	WAITED=$((WAITED + STEP))
done
