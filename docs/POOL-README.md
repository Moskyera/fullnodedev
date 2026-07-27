HBIT Pool - RUNNING A POOL (optional)
By Mosky
=====================================

WHAT THIS IS, AND WHO NEEDS IT
------------------------------
Most people who downloaded this package do NOT need anything in this file.

If you want to mine, you are already done: run SETUP.bat (Windows) or
./SETUP-LINUX.sh (Linux), open the miner panel, and point it at a pool or at
your own fullnode. Mining does not require running a pool.

HBIT is the other side of that. It is a mining pool: a program that serves work
to OTHER PEOPLE'S miners, keeps track of the shares they find, submits the
blocks it finds, and pays everybody out. You need it only if you intend to run
a pool that other people mine on. Two programs do that job:

  hbit-pool-server   serves work, counts shares, submits blocks, pays out on a
                     timer. This is the pool.
  hbit-pool-payout   pays out by hand, run only while the server is stopped.

Running a pool means holding other people's money and answering for it. If that
is not what you set out to do, close this file and go mine.


WHAT IT WILL DO WITH YOUR MONEY
-------------------------------
The pool mines into a wallet of its own, holds that income, and then pays it
out to the miners who found the shares. Concretely:

  - Every block the pool finds is mined into ITS OWN wallet, not yours. The
    money lands in the wallet file described in the next section.
  - It splits that balance over the last 4096 accepted shares (this is PPLNS)
    and pays each miner their proportion, automatically, every 5 minutes by
    default.
  - It takes NO fee. Everything it mines goes to the miners. Running the pool
    does not pay you anything, and you personally earn only from your own
    hashrate, on the same terms as everyone else.
  - Each payout transaction costs a 0.01 HAC network fee, funded out of a 0.5
    HAC reserve the pool keeps in its wallet. So running the pool costs you the
    fees. That reserve is not skimmed from miners; whatever of it is not needed
    is paid out later.
  - Income from a block the pool just found is held back for 16 blocks, which
    is about 80 minutes on mainnet, before it can be paid out. This is
    deliberate. Paying it sooner and then losing the block to a chain reorg
    would mean paying out money the chain never delivered, out of your pocket.
  - A miner whose share of a cycle rounds below 0.1 HAC is paid nothing that
    cycle. Their money is not taken from them: it stays in the wallet and is
    part of the next cycle.

The pool tells miners all of this itself, at http://<your pool>/terms, read out
of the same constants it settles with. A miner can check what it is owed and
what it was paid at http://<your pool>/earnings?worker=<their address>.


THE WALLET FILE: LOSE IT AND THE MONEY IS GONE
----------------------------------------------
The first time you start hbit-pool-server it CREATES a wallet key file at the
path you gave it (the examples below call it pool-wallet.key, in the same
folder as the program). That file holds the private key to every coin the pool
has taken in and not yet paid out, including money that belongs to your miners.

  - There is no copy of that key anywhere else. Not on a server, not in this
    package, not with the author. Nobody can send it to you.
  - Delete the file, lose the disk, or reinstall over it, and every coin in the
    pool wallet is gone permanently. That includes the miners' money, which you
    will still owe them.
  - Back it up the day you create it, before any miner connects, and keep the
    backup somewhere the mining PC dying does not take with it.

The pool writes two more files next to it. Keep them in the same backup:

  pool-wallet.key.state.json   share accounting and the pending-payout ledger
  pool-wallet.key.settle.lock  the lock described at the end of this file

Losing the .state.json does not lose coins, but it loses the record of who is
owed what and what has already been paid.


THE PASSPHRASE: HALF OF THE KEY
-------------------------------
Set HBIT_WALLET_PASSWORD before you start the pool and the wallet file is
stored encrypted (Argon2id + AES-256-GCM), so a stolen backup, an old drive or
a disk snapshot is useless to whoever finds it. Without it, the key file is
plaintext and the pool warns you about that every time it starts.

Windows PowerShell, in the same window you are about to start the pool in:

  $env:HBIT_WALLET_PASSWORD = "a long passphrase you have written down"

Linux:

  export HBIT_WALLET_PASSWORD='a long passphrase you have written down'

It must be at least 8 characters. If you would rather not put it in the
environment, put it in a file and set HBIT_WALLET_PASSWORD_FILE to that file's
path instead.

NEITHER HALF WORKS ALONE. The encrypted key file without the passphrase is
noise, and the passphrase without the key file is a string of words. There is
no reset, no recovery question and no support address. So back up BOTH, and
back them up together:

  - Write the passphrase down somewhere physical. Do not keep it only on the
    machine that holds the key file, and do not keep it only in your head.
  - Test the pair before you trust it. Stop hbit-pool-server but leave your
    fullnode running, copy the backed-up key file into an empty folder, set the
    passphrase in that window, and run hbit-pool-payout there with no --commit,
    which pays nothing:

      .\hbit-pool-payout.exe http://127.0.0.1:9777 http://127.0.0.1:8080 mainnet pool-wallet.key

    It prints a line starting "wallet  =". If that address is your pool's
    address, the backup works. The node has to be reachable for this: the tool
    reads the balance before it prints the address, and gives up if it cannot.
  - Encrypting the file today does not reach backwards. Any backup or snapshot
    taken while the file was still plaintext still contains the bare private
    key. Treat those as live secrets.


STARTING IT
-----------
You need your own Hacash fullnode running and synced first. In this package
that is hacash.exe (Windows) or ./hacash (Linux), with hacash.config.ini set up
by SETUP.bat / SETUP-LINUX.sh, and its RPC on port 8080.

Windows PowerShell, from the folder you extracted this package into:

  $env:HBIT_WALLET_PASSWORD = "a long passphrase you have written down"
  .\hbit-pool-server.exe http://127.0.0.1:8080 pool-wallet.key 0.0.0.0:9777 24 mainnet

Linux, from the same folder:

  export HBIT_WALLET_PASSWORD='a long passphrase you have written down'
  ./hbit-pool-server http://127.0.0.1:8080 pool-wallet.key 0.0.0.0:9777 24 mainnet

Reading that command left to right: the fullnode to use, the wallet file to
create or open, the address and port to serve miners on, how hard a share is,
and which chain. Every one of them is explained in hbit-pool.example.ini, which
is a worksheet, not a config file: no program reads it.

Replace 0.0.0.0:9777 with 127.0.0.1:9777 while you are testing. 0.0.0.0 means
any machine that can reach this PC can mine here, which is what a real pool
wants and is also what puts this program on the network. Do not open a port to
it until the wallet is encrypted and backed up.

The `mainnet` argument is required and has no default. The server proves it
against your node's own current block before serving any work, and refuses to
start if they disagree, because a pool running the wrong rule mines work the
node throws away forever without saying so.


NEVER RUN hbit-pool-payout WHILE THE SERVER IS RUNNING
------------------------------------------------------
hbit-pool-payout is for paying out by hand. It must only run while
hbit-pool-server is STOPPED.

Both programs decide what to pay from the wallet's confirmed balance, and a
payout still sitting in the mempool does not reduce that balance. Run both at
once and each one sees the full balance, each one believes it is the only payer,
and the same shares get paid TWICE out of your own funds.

The programs enforce this themselves: each takes an exclusive lock on the wallet
and the second one refuses to start, printing

  REFUSING to run: another hbit-pool-server or hbit-pool-payout already holds ...

That message is not a bug and there is no lock file to delete. Stop the server,
wait for it to actually exit, then run the tool.

Run it dry first. Without --commit it pays nothing and just prints the split it
would make. Read that, then run the same command again with --commit on the end,
then start the server back up.


THE REST
--------
POOL-OPERATOR.md, in this same folder, is the full runbook: the payout timing,
what each startup refusal means, how to tell your miners your pool's address,
and what to do if you think the key leaked. Read it before real miners connect.
