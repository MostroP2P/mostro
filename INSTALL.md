# Mostro Setup on Digital Ocean

> **Note**: This guide is based on the wonderful [nostr relay guide](https://github.com/BlockChainCaffe/Nostr-Relay-Setup-Guide/blob/main/README.md)

## Intro

This is a step-by-step, complete guide on how to set up a Mostro server on Digital Ocean. The steps are based mostly on Digital Ocean droplet but it can be easily applied to any other VPS provider like AWS, OVH, Linode etc.

It will use the code on this repository and SQLite database.

## Requirements

- Digital Ocean account with some cash on it **OR** any other cloud/VPS provider will do, just change this steps accordingly
- root privileges

> **Please note**: All commands are executed as **root**.

## droplet

- Pick your droplet, for example:
  - Basic (shared CPU), Disk: SSD - 2vCPU - 2 GB Ram - 60 GB SSD disk - 3 TB transfer
  - x86
  - Linux Ubuntu

Create a SSH key and add it as a new key to your Digital Ocean account

```bash
ssh root@1.2.3.4
```

- otherwise:
  - get means to access the VPS via ssh
  - check with your provider or manually setup the firewall (iptables, ufw)
  - connect to the instance with ssh

## Installation Steps

### Perform the updates

```bash
apt update
apt upgrade -y
```

### Install rust tools (press 1 when asked)

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source /root/.cargo/env
```

Check that rust is installed and in your path

```bash
rustc --version
cargo --version
```

### Install dependencies

```bash
apt-get install cmake build-essential libsqlite3-dev libssl-dev pkg-config git sqlite3 -y
```

### Compile Mostro

```bash
cd /opt
mkdir mostro
git clone https://github.com/MostroP2P/mostro.git
cd mostro
cargo build --release
```

### Install Mostro

```bash
install target/release/mostrod /usr/local/bin
```

## Mostro Configuration

### Create a mostro user to run the service and turn into that user

```bash
adduser --disabled-login mostro  # keep pressing enter until it ends
```

### Update the configuration file

```bash
cd /opt/mostro
```

Create a new settings file from `/opt/mostro/mostro/settings.tpl.toml` and save it to `/opt/mostro`:

```bash
cp /opt/mostro/mostro/settings.tpl.toml /opt/mostro/settings.toml
```

Update the file `/opt/mostro/settings.toml` with your favourite editor.

Here some parameters you might want to change:

- **lnd_cert_file**: path to tls.cert file
- **lnd_macaroon_file**: path to macaroon file
- **lnd_grpc_host**: lnd gRPC host and port
- **nsec_privkey** : Your mostro private key
- **relays** : List of relays you want to connect to

## Database

The data is saved in a sqlite db file named by default `mostro.db`, this file is saved on the root directory of the project and can be change just editing the `url` var on the `[database]` section in `settings.toml` file.

Before start building we need to initialize the database, for this we need to use `sqlx_cli`:

```bash
$ cargo install sqlx-cli --version 0.6.2
$ ./init_db.sh
```

Check the DB files are there

```bash
ls -al /opt/mostro
drwxrwxr-x root root 4.0 KB Fri Jun 14 15:52:07 2024 .
drwxr-x--- root root 4.0 KB Sat Jun 15 15:50:32 2024 ..
.rw-r--r-- root root  52 KB Fri May 31 16:35:34 2024 mostro.db
.rw-r--r-- root root  32 KB Sat Jun 15 15:28:23 2024 mostro.db-shm
.rw-r--r-- root root  16 KB Fri Jun 14 15:57:24 2024 mostro.db-wal
```

## Clean compilation artifacts

Since the instance you are using has little disk space you don't want to waste valuable disk space. Once successfully compiled the compilation artifacts can use up to **2Gb** of space.

In order to reclaim that space, once the compiled binaries are installed and moved under /usr/local/bin, you can clean them up with

```bash
cargo clean
```

### First Test

Start mostrod as a stand alone foreground process and check the logs

```bash
/usr/local/bin/mostrod -d /opt/mostro
```

Stop it with CTRL+C

### Give the permissions to the user mostro

```bash
chown -R mostro:mostro /opt/mostro
```

## Service Setup

### Systemd run script

Create a file **/etc/systemd/system/mostro.service** with the following content

```bash
[Unit]
Description=Mostro daemon
After=network.target

[Service]
Type=simple
User=mostro
WorkingDirectory=/home/mostro
Environment=RUST_LOG=info
ExecStart=/usr/local/bin/mostrod -d /opt/mostro
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

Then enable and start the mostro service

```bash
systemctl daemon-reload
systemctl enable mostro.service
systemctl start mostro.service
```

### Check that the service is running

```bash
systemctl status mostro.service
```

```bash
ps aux | grep mostrod
mostro    174139  0.0  2.0 925284 40824 ?        Sl   Jun14   0:58 /usr/local/bin/mostrod -d /opt/mostro
```

### Run an external check

From another pc/server/vps (your laptop) check that mostro is sending and receiving events

First we need to install two tools, `nostreq` and `nostcat`:

```bash
cargo install nostreq
cargo install nostcat
```

Now we connect with one of the relays we added to the `settings.toml` file:

```bash
nostreq --kinds 38383 --limit 5 --authors your-mostro-pubkey | nostcat --stream wss://random.nostr.relay | jq
```

In few minutes you should see a nostr event with Mostro settings:

```json
[
  "EVENT",
  "7005a443-d1ba-4f58-9046-70f9881c979d",
  {
    "tags": [
      ["d", "info-your-mostro-pubkey"],
      ["mostro_pubkey", "your-mostro-pubkey"],
      ["mostro_version", "0.12.1"],
      ["mostro_commit_id", "466ef06d2c113fb026e46491d7cb27955c41b531"],
      ["max_order_amount", "20000"],
      ["min_order_amount", "100"],
      ["expiration_hours", "24"],
      ["expiration_seconds", "900"],
      ["fee", "0.006"],
      ["hold_invoice_expiration_window", "900"],
      ["hold_invoice_cltv_delta", "298"],
      ["invoice_expiration_window", "900"],
      ["y", "mostrop2p"],
      ["z", "info"]
    ],
    "content": "",
    "sig": "7195fe1cdcd51e8947160d70b74a17f144924f5497aad4c5852e3f27177cc165360b04eb0243a4884772d3901e004f9213ca1d08c64f3284be9f1640aea1af5e",
    "id": "06df0bfbd4f30cfd8680f5ac6397f0b0bfdec38384f8c78f70a7fb70dcc53842",
    "pubkey": "your-mostro-pubkey",
    "created_at": 1718483696,
    "kind": 38383
  }
]
```

### Logs

#### Mostro logs

To keep an eye on what's goin on simply ask the logs

```bash
journalctl -f | grep --line-buffered mostro | cut -d' ' -f 10,12-100
```

Press CTRL+C to stop it

### Inspect Database

**NOTE** : ⚠️ carefull with this one as you might damage the db

Try some sqlite commands like:

    cd /opt/mostro
    sqlite3 mostro.db
    ...
    sqlite> .databases
    sqlite> .tables
    sqlite> select * from orders;
    sqlite> select count(*) from orders;
    ...

## Connect your client

If you made it so far, congrats \!\!

Now is time to connect your client and see it at work \!\!

What about starting with [mostro-cli](https://github.com/MostroP2P/mostro-cli)

## Pitfalls

- Do not install rust and/or cargo using apt. Just use rustup.
