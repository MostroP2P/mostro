# Environment Variables

This document describes the environment variables used by the Docker setup.

## Required Variables (for `make docker-build`)

- `LND_CERT_FILE`: Path to the LND TLS certificate file on your host system
  - Example: `~/.polar/networks/1/volumes/lnd/alice/tls.cert`

- `LND_MACAROON_FILE`: Path to the LND admin macaroon file on your host system
  - Example: `~/.polar/networks/1/volumes/lnd/alice/data/chain/bitcoin/regtest/admin.macaroon`

These files are copied to `docker/config/lnd/` during the build process: the cert with mode `0644`, the admin macaroon with mode `0600`, both inside a directory with mode `0700`. The macaroon grants full control of your LND node, so it is never left readable by other users on the host.

The copies belong to the user that ran the command, and the container runs as uid/gid 1000 by default. If your user is not uid 1000, run the container as yourself with the optional variable below rather than handing the config directory over to uid 1000.

## Optional Variables

- `MOSTRO_RELAY_LOCAL_PORT`: Port number for the local Nostr relay (defaults to 7000)
  - Used in `compose.yml` for port mapping
  - Example: `export MOSTRO_RELAY_LOCAL_PORT=7000`

- `MOSTRO_CONTAINER_USER`: uid/gid the `mostro` container runs as (defaults to `1000:1000`, the image's `mostrouser`)
  - Set it when your host user is not uid 1000, so the container can read the `0600` macaroon and write `mostro.db` in the mounted config directory
  - Example: `export MOSTRO_CONTAINER_USER=$(id -u):$(id -g)`

- `MOSTRO_DB_PASSWORD`: Not used (database encryption was removed). Kept in `compose.yml` for backward compatibility; can be omitted or left empty.

## Usage Examples

### Linux/macOS
```sh
LND_CERT_FILE=~/.polar/networks/1/volumes/lnd/alice/tls.cert \
LND_MACAROON_FILE=~/.polar/networks/1/volumes/lnd/alice/data/chain/bitcoin/regtest/admin.macaroon \
make docker-build
```

### Windows PowerShell
```powershell
$env:LND_CERT_FILE="C:\Users\YourUser\.polar\networks\1\volumes\lnd\alice\tls.cert"
$env:LND_MACAROON_FILE="C:\Users\YourUser\.polar\networks\1\volumes\lnd\alice\data\chain\bitcoin\regtest\admin.macaroon"
make docker-build
```

### Setting variables for the session
```sh
export LND_CERT_FILE=~/.polar/networks/1/volumes/lnd/alice/tls.cert
export LND_MACAROON_FILE=~/.polar/networks/1/volumes/lnd/alice/data/chain/bitcoin/regtest/admin.macaroon
export MOSTRO_RELAY_LOCAL_PORT=7000
make docker-build
make docker-up
```
