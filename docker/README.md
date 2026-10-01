# Docker Guide for MostroP2P

This guide provides instructions for building and running the MostroP2P application using Docker and Docker Compose.

## Prerequisites

Ensure you have Docker and Docker Compose installed on your machine. You can download Docker from [here](https://www.docker.com/get-started) and Docker Compose from [here](https://docs.docker.com/compose/install/).

You need to have a LND node running locally. We recommend using [Polar](https://lightningpolar.com/) for this.

## Docker Compose Configuration

The `compose.yml` sets up the following services:

- `mostro`: the MostroP2P service (standard build using `docker/Dockerfile`)
- `nostr-relay`: the Nostr relay

StartOS users: install Mostro from the StartOS marketplace (one-click).

## Building and Running the Docker Container

To build and run the Docker container using Docker Compose, follow these steps:

### Steps for running the MostroP2P service and Nostr relay

1. Clone the repository:

   ```sh
   git clone https://github.com/MostroP2P/mostro.git
   ```

2. Ensure you have `settings.toml` (and, after running `make docker-build`, the LND cert and macaroon in `config/lnd/`) in a `config` directory. The `compose.yml` `volumes` section mounts `./config` (relative to the `docker/` directory) to `/config` in the container. On first run you only need `settings.toml`; Mostro creates `mostro.db` automatically. Create the config dir and copy the template as follows:

   ```sh
   cd docker
   install -d -m 700 config
   install -m 600 ../settings.tpl.toml config/settings.toml
   ```

   Mode `0700` on `config` and `0600` on `settings.toml` because that directory ends up holding every secret this deployment has: `nsec_privkey` in `settings.toml`, the LND credentials in `config/lnd/`, and the `mostro.db` the daemon writes. `install -d -m 700` also tightens a `config` directory an earlier `mkdir -p` left at `0755` — this command is you deciding the mode. Neither `make docker-build` nor `make docker-up` will decide it again: both only create `config` when it is missing, so a directory you deliberately opened up to a group later on keeps that mode. (`config/lnd` is the exception: `make docker-build` sets it to `0700` on every run, since nothing but the LND credentials it installs lives there.)

   _Don't forget to edit `lnd_grpc_host`, `nsec_privkey` and `relays` fields in the `config/settings.toml` file. Note that paths in `settings.toml` refer to paths **inside the container**, so use `/config/lnd/tls.cert` and `/config/lnd/admin.macaroon` for the LND certificate and macaroon files (these will be copied there by `make docker-build`)._

3. Build the docker image. You need to provide the `LND_CERT_FILE` and `LND_MACAROON_FILE` environment variables with the paths to your LND TLS certificate and macaroon files. These files will be copied to the `docker/config/lnd` directory by the `make docker-build` command. The build process will validate that these variables are set and that the files exist before proceeding.

   **Linux/macOS:**
   ```sh
   LND_CERT_FILE=~/.polar/networks/1/volumes/lnd/alice/tls.cert \
   LND_MACAROON_FILE=~/.polar/networks/1/volumes/lnd/alice/data/chain/bitcoin/regtest/admin.macaroon \
   make docker-build
   ```

   **Windows PowerShell:**
   ```powershell
   $env:LND_CERT_FILE="C:\Users\YourUser\.polar\networks\1\volumes\lnd\alice\tls.cert"
   $env:LND_MACAROON_FILE="C:\Users\YourUser\.polar\networks\1\volumes\lnd\alice\data\chain\bitcoin\regtest\admin.macaroon"
   make docker-build
   ```

   **Alternative:** You can export the variables for your session:
   ```sh
   export LND_CERT_FILE=~/.polar/networks/1/volumes/lnd/alice/tls.cert
   export LND_MACAROON_FILE=~/.polar/networks/1/volumes/lnd/alice/data/chain/bitcoin/regtest/admin.macaroon
   make docker-build
   ```

   The admin macaroon grants full control of your LND node, so `make docker-build` writes it to `config/lnd/admin.macaroon` with mode `0600` (owner only) inside a `config/lnd` directory with mode `0700`. The `config` root is set to `0700` too when this command has to create it, and left alone when it is already there (step 2). The directories and files this command creates belong to the user who ran it; `mostro.db` is created later by the container and belongs to whoever the container runs as.

   Under `sudo` there is one extra step, taken for you: `install` recreates its destination as the invoking user, so a plain `sudo make docker-build` would leave a `root:root` macaroon inside a `config` you had already handed to uid 1000, and the container would fail at the LND connection with nothing pointing at ownership. When it runs as root over a `config` that is not root-owned, the command derives `-o`/`-g` from that directory and installs `config/lnd` and both credentials as its owner. A `config` that is root-owned itself is left as it is: `make docker-up` refuses that case outright rather than run the daemon as root.

   `make docker-up` runs the container as the owner of `docker/config` and prints the uid/gid it picked. That is the account that can actually read the `0600` macaroon and write `mostro.db` there, whether the directory belongs to you (the usual case after `make docker-build`) or was handed to uid/gid 1000. To pick a different one, export the variable `compose.yml` reads in the same shell:

   ```sh
   export MOSTRO_CONTAINER_USER=$(id -u):$(id -g)
   ```

   A bare `docker compose up` does not derive anything and falls back to `1000:1000`, the image's `mostrouser`.

   `make docker-up` refuses to start when that uid comes out as root — both the numeric `0` and the name `root`, the two spellings compose accepts — which is what a `sudo make docker-build` on a fresh tree leaves behind: the container would run as root, dropping the one privilege boundary the image has. Either run both targets as the account that owns `docker/config`, or hand the directory over with `sudo chown -R 1000:1000 config` (`docker compose up` then matches, since the image's `mostrouser` is pinned to uid/gid 1000).

4. [Optional] Set the `MOSTRO_RELAY_LOCAL_PORT` environment variable to the port you want to use for the local relay (defaults to 7000 if not set). This can be set before running `make docker-up`:

   ```sh
   export MOSTRO_RELAY_LOCAL_PORT=7000
   make docker-up
   ```

   Or set the variable on one line before `make docker-up`:
   ```sh
   MOSTRO_RELAY_LOCAL_PORT=7000 make docker-up
   ```

5. Run the docker compose file:

   ```sh
   make docker-up
   ```

## Running the plain image from Docker Hub

You can run the plain Mostro image without building locally. Use a single **config directory** on the host and mount it at `/config` in the container. Paths in `settings.toml` are **inside the container**, so use `/config/...` for certs, macaroon, and database.

1. Create a config directory and get the settings template:

   **Option A — download the template** (from the [settings.tpl.toml](https://github.com/MostroP2P/mostro/blob/main/settings.tpl.toml) repo file):

   ```sh
   install -d -m 700 ~/mostro-config ~/mostro-config/lnd
   (umask 077 && curl -fsSL https://raw.githubusercontent.com/MostroP2P/mostro/main/settings.tpl.toml -o ~/mostro-config/settings.toml)
   ```

   The config root is `0700` and `settings.toml` is `0600` because both `nsec_privkey` and, later, `mostro.db` live there. `curl` creates the file under the umask in force, typically `0644`; setting the umask in a subshell around it means the file is never world-readable, not even for the moment a subsequent `chmod` would take.

   **Option B — use the entrypoint default:** create the config dir with `install -d -m 700 ~/mostro-config ~/mostro-config/lnd`, then run the container once against it; the entrypoint installs a default `settings.toml` (from the image, built from `settings.tpl.toml`) into `/config` with mode `0600`. Stop the container, edit the file on the host (e.g. `~/mostro-config/settings.toml`), then start the container again.

2. Copy your LND TLS cert and macaroon into the config dir (so they appear at `/config/lnd/` in the container). Use `install` rather than `cp`: `cp` keeps whatever mode the source file (or an already existing destination file) happens to have, while `install -m` sets the mode explicitly. The admin macaroon grants full control of your LND node, so it must not be readable by other users on the host:

   ```sh
   install -m 644 /path/to/your/tls.cert ~/mostro-config/lnd/tls.cert
   install -m 600 /path/to/your/admin.macaroon ~/mostro-config/lnd/admin.macaroon
   ```

   Mode `0600` on the macaroon inside a `0700` directory means only their owner can reach the file, and the container runs as uid/gid 1000 by default (the image's `mostrouser`, pinned to those ids). If your user is not uid 1000, run the container as yourself by adding `--user $(id -u):$(id -g)` to the `docker run` command in step 4 — that also lets it write `mostro.db` into your config directory.

3. Edit `~/mostro-config/settings.toml`: set `nsec_privkey`, `relays`, and for Docker set `lnd_cert_file` / `lnd_macaroon_file` to `/config/lnd/...`, `lnd_grpc_host` (e.g. `https://host.docker.internal:10009`), and `[database]` `url = "sqlite:///config/mostro.db"`.

4. Run the container. On Linux, add `--add-host=host.docker.internal:host-gateway` so the container can reach LND on the host:

   ```sh
   docker run -d --name mostro \
     --add-host=host.docker.internal:host-gateway \
     -v ~/mostro-config:/config \
     mostrop2p/mostro:latest
   ```

   If you used Option B (empty config dir), edit the copied `settings.toml` and restart. Mostro creates `mostro.db` at startup when missing.

5. Check logs: `docker logs -f mostro`.

## Running plain Mostro on a VPS

Steps to run the plain Mostro image on a VPS (no repo clone; image from Docker Hub).

1. **Install Docker** on the VPS (e.g. [Docker Engine](https://docs.docker.com/engine/install/)).

2. **Create a config directory** at `/opt/mostro`:

   ```sh
   install -d -m 700 -o 1000 -g 1000 /opt/mostro
   install -d -m 700 -o 1000 -g 1000 /opt/mostro/lnd
   ```

   These steps run as root, while the container runs as uid/gid 1000, so both directories are handed to the container's user: it needs to write `mostro.db` into the config directory. Both are owner-only because of what goes in them — `nsec_privkey` in `settings.toml` and the database in the config root, the LND credentials in `lnd` (step 4).

3. **Get the settings template** into that directory as `settings.toml`:

   - Either run the container once with an empty config dir; the entrypoint installs the default template at `/config/settings.toml` with mode `0600`. Stop the container, then edit the file on the host.
   - Or download the template and copy it:

   ```sh
   (umask 077 && curl -fsSL https://raw.githubusercontent.com/MostroP2P/mostro/main/settings.tpl.toml -o /opt/mostro/settings.toml)
   chown 1000:1000 /opt/mostro/settings.toml
   ```

   `curl` writes the file under root's umask, typically `0644` and root-owned. The umask in the subshell settles the mode as the file is created, so it is never world-readable; the owner still has to be handed over afterwards, because the file receives `nsec_privkey` in step 5 and the container reads it as uid/gid 1000.

4. **Put LND files** in the config dir so they appear at `/config/lnd/` in the container. Use `install -m` rather than `cp`, which would keep whatever mode the source file (or an already existing destination file) happens to have:

   ```sh
   install -m 644 /path/to/lnd/tls.cert /opt/mostro/lnd/tls.cert
   install -m 600 -o 1000 -g 1000 /path/to/lnd/admin.macaroon /opt/mostro/lnd/admin.macaroon
   ```

   The admin macaroon grants full control of your LND node — anyone who reads it can move the funds escrowed in Mostro's hold invoices — so it is installed owner-readable only, and `-o 1000 -g 1000` hands it to the container's user (as the `0700` directory from step 2 already was). Without that ownership, mode `0600` would leave mostrod unable to read the macaroon. (`-o`/`-g` require root; as a non-root user, drop them, run the steps as the account that owns the config dir, and add `--user $(id -u):$(id -g)` to the `docker run` commands in step 6, so the container runs as that account rather than as the image default `1000:1000` — which could not read the `0600` macaroon you just installed.)

   (If LND is on another host, you only need the cert and macaroon copied here; point `lnd_grpc_host` at that host in step 5.)

5. **Edit `/opt/mostro/settings.toml`**:

   - `[lightning]`: `lnd_cert_file = '/config/lnd/tls.cert'`, `lnd_macaroon_file = '/config/lnd/admin.macaroon'`, `lnd_grpc_host` = your LND gRPC URL (e.g. `https://host.docker.internal:10009` if LND is on the same VPS, or `https://your-lnd-host:10009` if remote).
   - `[database]`: `url = "sqlite:///config/mostro.db"`.
   - `[nostr]`: set `nsec_privkey` and `relays` (e.g. public relays).

6. **Run the container**:

   - If LND is on the **same VPS** (e.g. another container or process), so the container must reach the host:

   ```sh
   docker run -d --name mostro \
     --restart unless-stopped \
     --add-host=host.docker.internal:host-gateway \
     -v /opt/mostro:/config \
     mostrop2p/mostro:latest
   ```

   - If LND is on a **different machine**, omit `--add-host` and use that machine’s hostname or IP in `lnd_grpc_host`:

   ```sh
   docker run -d --name mostro \
     --restart unless-stopped \
     -v /opt/mostro:/config \
     mostrop2p/mostro:latest
   ```

7. **Check logs**: `docker logs -f mostro`. Mostro will create `mostro.db` in the config dir on first run.

8. **Optional**: Pin the image to a version, e.g. `mostrop2p/mostro:v0.16.2` instead of `:latest`.

## Stopping the Docker Container

To stop the Docker container, run:

```sh
make docker-down
```

## Available Make Commands

- `make docker-build` - Build the standard mostro service (requires `LND_CERT_FILE` and `LND_MACAROON_FILE` environment variables)
- `make docker-up` - Start all services (mostro + nostr-relay)
- `make docker-down` - Stop all services
- `make docker-relay-up` - Start only the Nostr relay
- `make docker-build-startos` - Build the StartOS variant of mostro service

See [ENV_VARIABLES.md](ENV_VARIABLES.md) for details about required environment variables.

## Steps for running just the Nostr relay

1. Run the following command to start the Nostr relay:

   ```sh
   make docker-relay-up
   ```

2. Stop the Nostr relay:

   ```sh
   make docker-down
   ```
