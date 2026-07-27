#!/usr/bin/env bash
# HBIT pool: first-time setup on a VPS or mini PC.
#
# This checks what it can check and then tells you exactly what to run. It does
# NOT invent a reward address, does NOT write a passphrase to disk, and does NOT
# start anything that would hold other people's money before you have decided
# where that money goes. Every one of those would be a convenience worth less
# than what it risks.
set -euo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$HERE"

say()  { printf '%s\n' "$*"; }
ok()   { printf '  ok    %s\n' "$*"; }
bad()  { printf '  FAIL  %s\n' "$*"; }
step() { printf '\n== %s ==\n' "$*"; }

fail=0

step "1. What is in this package"
for f in hacash hbit-pool-server hbit-pool-payout hbit-wait-for-node.sh; do
	if [ -x "$f" ]; then ok "$f"; else bad "$f is missing or not executable"; fail=1; fi
done
[ -f hacash.config.ini.example ] && ok "hacash.config.ini.example" || { bad "hacash.config.ini.example missing"; fail=1; }

step "2. This machine"
case "$(uname -m)" in
x86_64 | amd64) ok "architecture $(uname -m)" ;;
*) bad "this build is x86_64 only, found $(uname -m)"; fail=1 ;;
esac
command -v curl >/dev/null 2>&1 && ok "curl present" || { bad "curl is required; apt-get install -y curl"; fail=1; }
# The chain needs room. 2.7 GB measured for a full mainnet sync, and it grows.
avail_gb=$(df -Pk . | awk 'NR==2 {print int($4/1048576)}')
if [ "${avail_gb:-0}" -ge 10 ]; then ok "${avail_gb} GB free"; else bad "only ${avail_gb} GB free; the chain needs well over 3 GB and grows"; fail=1; fi

step "3. The node config"
if [ ! -f hacash.config.ini ]; then
	cp hacash.config.ini.example hacash.config.ini
	say "  created hacash.config.ini from the example"
fi
reward=$(sed -n 's/^[[:space:]]*reward[[:space:]]*=[[:space:]]*//p' hacash.config.ini | head -1)
if [ -z "$reward" ]; then
	bad "reward is empty in hacash.config.ini"
	say "        Put YOUR OWN mainnet address there. There is no default and there"
	say "        will never be one: an address you did not type yourself would pay"
	say "        somebody else, permanently, with no error and no way back."
	fail=1
else
	ok "reward is set to $reward"
	say "        Check that address is yours before going further."
fi
if grep -qE '^[[:space:]]*fast_sync[[:space:]]*=[[:space:]]*true' hacash.config.ini; then
	bad "fast_sync = true builds a chain that cannot be extended; set it to false"
	fail=1
else
	ok "fast_sync is not enabled"
fi

step "4. The wallet passphrase"
if [ -n "${HBIT_WALLET_PASSWORD:-}" ] || [ -n "${HBIT_WALLET_PASSWORD_FILE:-}" ]; then
	ok "a passphrase is configured in the environment"
else
	bad "no passphrase configured"
	say "        Without one the pool writes its wallet key in PLAINTEXT. That file"
	say "        holds every coin the pool owes its miners. Set it before starting:"
	say "          export HBIT_WALLET_PASSWORD='something long you have written down'"
	say "        or point HBIT_WALLET_PASSWORD_FILE at a file only you can read."
	fail=1
fi

if [ "$fail" -ne 0 ]; then
	say ""
	say "Setup stopped. Fix the FAIL lines above and run this again. Nothing was"
	say "started, so nothing is mining and nothing is owed."
	exit 1
fi

step "Ready. Two commands, in this order"
cat <<'NEXT'
  1) Start the node and let it sync. This takes minutes to hours and the pool
     must NOT run before it finishes, or it will serve work on a chain the
     network left behind years ago:

       ./hacash

     Watch the height climb. It is done when the log says "all blocks sync
     finished" and the height stops jumping. In another terminal:

       ./hbit-wait-for-node.sh http://127.0.0.1:8080 0

  2) Start the pool. Pick share_bits from the difficulty in force: a share
     should cost around 2^20 hashes, and the pool refuses anything under 2^16
     and tells you what would work.

       ./hbit-pool-server http://127.0.0.1:8080 pool-wallet.key 0.0.0.0:9777 20 mainnet 300

  To run both as services instead, the unit files are in systemd/ and DEPLOY.md
  explains them.

  BACK UP pool-wallet.key AND pool-wallet.key.state.json, somewhere that is not
  this machine. The first is the money. The second is the record of who is owed
  what. Losing either is not recoverable by anyone, including the author of this
  software.
NEXT
