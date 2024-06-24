# Docker Guide for MostroP2P

This guide provides instructions for building and running the MostroP2P application using Docker and Docker Compose.

## Prerequisites

Ensure you have Docker and Docker Compose installed on your machine. You can download Docker from [here](https://www.docker.com/get-started) and Docker Compose from [here](https://docs.docker.com/compose/install/).

## Docker Compose Configuration

The `compose.yml` file is configured as follows:

```yaml
services:
  mostro:
    build: .
    volumes:
      - ~/mostro:/config # settings.toml and mostro.db
      - ~/.polar/networks/1/volumes/lnd:/lnd # LND data
```

## Building and Running the Docker Container

To build and run the Docker container using Docker Compose, follow these steps:

1. Clone the repository:

   ```sh
   git clone https://github.com/MostroP2P/mostro.git
   cd mostro/docker
   ```

2. Ensure you have the `settings.toml` configuration file and the `mostro.db` SQLite database in a `config` directory (acording to the `volumes` section). If you don't have those files from a previous installation, then the first time they will be created as follows:

   - `settings.toml` from the settings.docker.toml template
   - `mostro.db` from (empty) database mostro.empty.db

3. Run Docker Compose:

   ```sh
   docker compose up --build -d
   ```

This command will build the Docker image and run the container in detached mode.
