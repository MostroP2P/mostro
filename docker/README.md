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

2. Ensure you have the `settings.toml` configuration file and the `mostro.db` SQLite database in a `config` directory (according to the `volumes` section in compose.yml file). The `volumes` section mounts `./config` (relative to the `docker/` directory) to `/config` in the container. If you don't have those files from a previous installation, then the first time they will be created as follows:

   ```sh
   cd docker
   mkdir -p config
   cp ../settings.tpl.toml config/settings.toml
   ```

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

   For more details about environment variables, see [ENV_VARIABLES.md](ENV_VARIABLES.md).

4. [Optional] Set the `MOSTRO_RELAY_LOCAL_PORT` environment variable to the port you want to use for the local relay (defaults to 7000 if not set). This can be set before running `make docker-up`:

   ```sh
   export MOSTRO_RELAY_LOCAL_PORT=7000
   make docker-up
   ```

   Or pass it inline:
   ```sh
   MOSTRO_RELAY_LOCAL_PORT=7000 make docker-up
   ```

5. Run the docker compose file (the make command automatically runs from the `docker/` directory):

   ```sh
   make docker-up
   ```

## Running the plain image from Docker Hub

You can run the plain Mostro image without building locally. Use a single **config directory** on the host and mount it at `/config` in the container. Paths in `settings.toml` are **inside the container**, so use `/config/...` for certs, macaroon, and database.

1. Create a config directory and get the settings template:

   **Option A — download the template** (from the [settings.tpl.toml](https://github.com/MostroP2P/mostro/blob/main/settings.tpl.toml) repo file):

   ```sh
   mkdir -p ~/mostro-config/lnd
   curl -sL https://raw.githubusercontent.com/MostroP2P/mostro/main/settings.tpl.toml -o ~/mostro-config/settings.toml
   ```

   **Option B — use the entrypoint default:** run the container once with an empty config dir; the entrypoint copies a default `settings.toml` (from the image, built from `settings.tpl.toml`) into `/config`. Stop the container, edit the file on the host (e.g. `~/mostro-config/settings.toml`), then start the container again.

2. Copy your LND TLS cert and macaroon into the config dir (so they appear at `/config/lnd/` in the container):

   ```sh
   cp /path/to/your/tls.cert ~/mostro-config/lnd/tls.cert
   cp /path/to/your/admin.macaroon ~/mostro-config/lnd/admin.macaroon
   ```

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

2. **Create a config directory** (e.g. `/opt/mostro` or `~/mostro-config`):

   ```sh
   mkdir -p /opt/mostro/lnd
   ```

3. **Get the settings template** into that directory as `settings.toml`:

   - Either run the container once with an empty config dir; the entrypoint will copy the default template to `/config/settings.toml`. Stop the container, then edit the file on the host.
   - Or download the template and copy it:

   ```sh
   curl -sL https://raw.githubusercontent.com/MostroP2P/mostro/main/settings.tpl.toml -o /opt/mostro/settings.toml
   ```

4. **Put LND files** in the config dir so they appear at `/config/lnd/` in the container:

   ```sh
   cp /path/to/lnd/tls.cert /opt/mostro/lnd/tls.cert
   cp /path/to/lnd/admin.macaroon /opt/mostro/lnd/admin.macaroon
   ```

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
