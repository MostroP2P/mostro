# Mostro Instructions

## Getting Started

Welcome to Mostro on StartOS! Mostro is a peer-to-peer lightning exchange over Nostr.

## Storage

Mostro uses two separate storage volumes for data persistence:

- **Database Volume**: Stores `mostro.db` (SQLite database with orders, trades, and user data)
- **Configuration Volume**: Stores `settings.toml` (Mostro configuration file)

These volumes persist your data across container restarts and updates. The files are automatically created with default values on first run.

## Configuration

1.  **Nostr Settings**:
    *   `nsec_privkey`: Enter your Mostro Nostr private key (starting with `nsec...`). This is required for your mostro instance to sign events.
    *   `relays`: A list of Nostr relays your mostro instance will connect to. You can use the default list or add your own.

2.  **Lightning Network Settings**:
    *   `lnd_cert_file`: The full path to your LND node's `tls.cert` file within the StartOS filesystem.
    *   `lnd_macaroon_file`: The full path to your LND node's `admin.macaroon` file.
    *   `lnd_grpc_host`: The gRPC host and port for your LND node (e.g., `localhost:10009`).

## Dependencies

Mostro requires the following services to be installed and running:

- **LND** (version 0.18.0 or higher): Lightning Network Daemon for Lightning Network functionality
- **Bitcoin Core** (version 25.0 or higher): Bitcoin node for blockchain access

## Usage

Once installed and configured, Mostro will be available at the configured RPC port (default: 8000). You can interact with it through the StartOS interface or directly via the RPC API.

## Backup

To backup your Mostro data, you can export the storage volumes:
- Database volume contains your `mostro.db` file
- Configuration volume contains your `settings.toml` file

## Support

- [Mostro GitHub Repository](https://github.com/MostroP2P/mostro)
- [Mostro Documentation](https://mostro.network/)
- [StartOS Documentation](https://docs.start9.com/) 