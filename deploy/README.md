# Running HBIT on a VPS or a mini PC

This is the deployment path for the pool operator. The pool is not something
miners download; it is a service you run beside your own full node, and miners
point at it.

Two ways are provided and they do the same job. Pick one.

| | Docker Compose | systemd |
|---|---|---|
| Setup | one file, one command | install binaries and units |
| Node RPC exposure | unreachable by construction | depends on your firewall |
| Upgrade | rebuild the image, recreate | replace the binary, restart |
| Needs | Docker on the host | nothing extra |

**Docker is the safer default**, for one specific reason covered below.

---

## The thing to get right, whichever you pick

The node's RPC is the **miner API**. Anything that can reach it can ask for
block templates and submit blocks. It must never be reachable from the internet.
The pool's port must be.

With Compose that is structural: the node service publishes no port at all, so
its RPC exists only on the private network the pool shares with it. There is no
rule to forget.

With systemd it is your firewall's job, and a firewall nobody verified is a
firewall nobody has. The verification step below is not optional.

---

## Docker Compose

### First run

```bash
git clone https://github.com/Moskyera/fullnodedev.git
cd fullnodedev
```

Set the node's reward address. The node **refuses to start** until you do, on
purpose: an earlier version of this project shipped a config with somebody
else's address in it, and anyone who ran it mined into a stranger's wallet.

```bash
$EDITOR deploy/node/hacash.config.ini      # fill in reward = your own address
```

Create the wallet passphrase. It is one half of the wallet; the key file the
pool creates is the other, and neither is worth anything alone.

```bash
mkdir -p deploy/secrets
install -m 0400 /dev/null deploy/secrets/wallet-passphrase
printf '%s' 'a passphrase you have written down somewhere else' \
  > deploy/secrets/wallet-passphrase
```

`deploy/secrets/` is gitignored so it cannot be committed. Write the passphrase
on paper too, somewhere that is not this machine.

Set the chain and the port you want in `deploy/docker-compose.yml` if the
defaults (mainnet, 9777) are not what you want, then:

```bash
docker compose -f deploy/docker-compose.yml up -d --build
docker compose -f deploy/docker-compose.yml logs -f pool
```

The first start syncs the chain, which takes a while and publishes nothing until
it is done. The pool waits for the node to answer before it serves any work.

### Back up before you tell anyone to mine on it

The `pool-data` volume holds the wallet, the PPLNS share window and the
pending-payout ledger. Losing the node volume costs a resync. Losing this one
costs every coin the pool holds and the record of who is owed what.

```bash
docker compose -f deploy/docker-compose.yml stop pool
docker run --rm -v hbit_pool-data:/d -v "$PWD":/out debian:bookworm-slim \
  tar czf /out/hbit-pool-backup-$(date +%F).tgz -C /d .
docker compose -f deploy/docker-compose.yml start pool
```

Copy that archive **and** the passphrase somewhere that is not this machine.
Then prove the backup works before you rely on it: restore it into an empty
volume and run `hbit-pool-payout` with no `--commit`, which pays nothing and
prints the wallet address. If that address matches your pool's, the backup is
real.

### Paying out by hand

The server settles on a timer. To settle manually you must stop it first: both
programs take an exclusive lock on the wallet, because two things settling one
wallet is how a pool pays the same window twice.

```bash
docker compose -f deploy/docker-compose.yml stop pool
docker compose -f deploy/docker-compose.yml run --rm --entrypoint hbit-pool-payout pool \
  http://node:8080 pool-wallet.key mainnet          # dry run, pays nothing
# read the split, then repeat with --commit
docker compose -f deploy/docker-compose.yml start pool
```

### Upgrading

```bash
git pull
docker compose -f deploy/docker-compose.yml up -d --build
```

Never rename or replace anything in the `pool-data` volume during an upgrade.
The wallet file, its `.state.json` and its `.settle.lock` are matched by name; a
renamed wallet means the pool starts empty while the money sits in a file
nothing reads any more.

---

## systemd

For a host without Docker. Units are in `deploy/systemd/`.

```bash
sudo useradd --system --home-dir /var/lib/hbit --shell /usr/sbin/nologin hbit
sudo mkdir -p /opt/hbit /var/lib/hbit/node /var/lib/hbit/pool /etc/hbit
sudo chown -R hbit:hbit /var/lib/hbit

cargo build --locked --release --bin fullnode
cargo build --locked --release -p hbit-pool --bin hbit-pool-server --bin hbit-pool-payout
sudo install -m 0755 target/release/fullnode /opt/hbit/
sudo install -m 0755 target/release/hbit-pool-server /opt/hbit/
sudo install -m 0755 target/release/hbit-pool-payout /opt/hbit/
sudo install -m 0755 deploy/hbit-wait-for-node.sh /opt/hbit/

sudo install -m 0400 -o hbit -g hbit /dev/null /etc/hbit/wallet-passphrase
sudo -u hbit tee /etc/hbit/wallet-passphrase >/dev/null <<< 'your passphrase'

sudo cp deploy/systemd/*.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now hacash-node hbit-pool
journalctl -u hbit-pool -f
```

The passphrase is in a `0400` file rather than an `Environment=` line because
unit files are commonly world readable and `systemctl show hbit-pool` prints
every environment value.

### The firewall, which is the part that matters

Open the pool port. Keep the node's RPC closed.

```bash
sudo ufw allow 9777/tcp                 # miners
sudo ufw allow 3337/tcp                 # chain p2p
sudo ufw deny 8080/tcp                  # node RPC: never from outside
sudo ufw enable
```

Then **verify from another machine**, because the rule you believe you wrote is
not evidence:

```bash
nmap -Pn -p 8080,9777,3337 YOUR.VPS.IP
```

9777 and 3337 open, 8080 filtered or closed. If 8080 answers, stop the pool and
fix it before anyone mines on this.

Also make sure the node's own config binds its RPC to `127.0.0.1` when it is not
in a container. `deploy/node/hacash.config.ini` binds `0.0.0.0`, which is right
inside Docker and wrong on a bare host.

---

## Operating it

Healthy log, roughly every settle interval:

```
[settle] holding back N unit(s) of block income that is not yet buried 16 deep
[settle] submitted payout tx <hash> paying N miner(s) U units; the node holds it
[reorg] our block N orphaned (chain holds <hash>)
```

Orphans are normal: it means the pool noticed one of its blocks losing a race
and did not pay out on income that no longer exists.

A payout that stays pending across several cycles gets a warning naming the
cause. The pool mines coinbase-only blocks unless the node has transactions to
pack, so a payout confirms when a block includes it.

From another machine, the two endpoints a miner cares about:

```bash
curl -s http://YOUR.VPS.IP:9777/terms
curl -s "http://YOUR.VPS.IP:9777/earnings?worker=<a miner's HAC address>"
```

`/terms` reports the pool's real scheme, fee, minimum and maturity, read out of
the same constants settlement uses, so what it advertises cannot drift from what
it does.

---

## Telling miners about it

Miners point the panel or `poworker` at `YOUR.VPS.IP:9777` and set
`pool_worker` to their own HAC address, which is also their payout address. The
pool refuses a share from an address it could not pay, so nobody mines for an id
that would be dropped at settlement.

To have HBIT appear in the panel's pool list with your address filled in, ship a
`pools.json` as described in `docs/POOL-OPERATOR.md`.
