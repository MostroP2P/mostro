# Environment Variables

This document describes the environment variables used by the Docker setup.

## Required Variables (for `make docker-build`)

- `LND_CERT_FILE`: Path to the LND TLS certificate file on your host system
  - Example: `~/.polar/networks/1/volumes/lnd/alice/tls.cert`

- `LND_MACAROON_FILE`: Path to the LND admin macaroon file on your host system
  - Example: `~/.polar/networks/1/volumes/lnd/alice/data/chain/bitcoin/regtest/admin.macaroon`

These files are copied to `docker/config/lnd/` during the build process: the cert with mode `0644`, the admin macaroon with mode `0600`, both inside a directory with mode `0700`. The macaroon grants full control of your LND node, so it is never left readable by other users on the host. The `docker/config` root is set to mode `0700` too when the build has to create it, since `settings.toml` (which carries `nsec_privkey`) and `mostro.db` live beside the credentials; an existing `docker/config` keeps the mode you gave it.

The copies belong to the user that ran the command, and `make docker-up` runs the container as the owner of `docker/config` — that user — so a host account that is not uid 1000 needs no extra step. Under `sudo`, the build installs `config/lnd` and the credentials as the owner of `docker/config` instead of as root, so a directory handed over with `chown -R 1000:1000` stays readable by the container across rebuilds. Set the optional variable below to override the container user.

## Optional Variables

- `MOSTRO_RELAY_LOCAL_PORT`: Port number for the local Nostr relay (defaults to 7000)
  - Used in `compose.yml` for port mapping
  - Example: `export MOSTRO_RELAY_LOCAL_PORT=7000`

- `MOSTRO_CONTAINER_USER`: uid/gid the `mostro` container runs as
  - `make docker-up` defaults it to the owner of `docker/config`, the account that can read the `0600` macaroon and write `mostro.db` there, and prints what it picked. A bare `docker compose up` falls back to `1000:1000`, the image's `mostrouser`.
  - root is refused, whether derived or set explicitly, and in both spellings compose accepts for it: the numeric `0` and the name `root`. `make docker-up` stops rather than run the daemon as root, which is what a root-owned `docker/config` (left by a `sudo make docker-build` on a fresh tree) would otherwise mean. Hand the directory to an unprivileged account, or set this variable to a non-zero uid/gid that can read it. Any other name is resolved inside the image, where `mostrouser` (uid/gid 1000) is the only account besides root.
  - Set it to run as someone else — for instance uid/gid 1000 on a config directory you handed over with `chown -R 1000:1000`
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
