# Docker Guide for MostroP2P

This guide provides instructions for building and running the MostroP2P application using Docker and Docker Compose.

## Prerequisites

Ensure you have Docker and Docker Compose installed on your machine. You can download Docker from [here](https://www.docker.com/get-started) and Docker Compose from [here](https://docs.docker.com/compose/install/).

You need to have a LND node running locally. We recommend using [Polar](https://lightningpolar.com/) for this.

## Docker Compose Configuration

The `compose.yml` sets up the following services:

- `mostro`: the MostroP2P service
- `nostr-relay`: the Nostr relay

## Building and Running the Docker Container

To build and run the Docker container using Docker Compose, follow these steps:

### Steps for running the MostroP2P service and Nostr relay

1. Clone the repository:

   ```sh
   git clone https://github.com/MostroP2P/mostro.git
   ```

2. Ensure you have the `settings.toml` configuration file and the `mostro.db` SQLite database in a `config` directory (acording to the `volumes` section). If you don't have those files from a previous installation, then the first time they will be created as follows:

   - `docker/config/settings.toml` from the `docker/settings.docker.toml` template
   - `docker/config/mostro.db` from the `docker/empty.mostro.db` database

3. Set the `LND_CERT_FILE` and `LND_MACAROON_FILE` to the paths of the LND TLS certificate and macaroon files on the `docker/.env` file. These files will be copied to the `docker/config/lnd` directory. For example:

   ```sh
   LND_CERT_FILE=~/.polar/networks/1/volumes/lnd/alice/tls.cert
   LND_MACAROON_FILE=~/.polar/networks/1/volumes/lnd/alice/data/chain/bitcoin/regtest/admin.macaroon
   ```

4. [Optional] Set the `MOSTRO_RELAY_LOCAL_PORT` to the port you want to use for the local relay on the `docker/.env` file. For example:

   ```sh
   MOSTRO_RELAY_LOCAL_PORT=7000
   ```

5. Build the docker image:

   ```sh
   make docker-build
   ```

6. Run the docker compose file:

   ```sh
   make docker-up
   ```

## Stopping the Docker Container

To stop the Docker container, run:

```sh
make docker-down
```

## Steps for running just the Nostr relay

1. Run the following command to start the Nostr relay:

   ```sh
   make docker-relay-up
   ```

2. Stop the Nostr relay:

   ```sh
   make docker-down
   ```
