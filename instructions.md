# Mostro Instructions

## Getting Started

Welcome to Mostro on StartOS! Mostro is a peer-to-peer lightning exchange over Nostr.

### Configuration

1.  **Nostr Settings**:
    *   `nsec_privkey`: Enter your Mostro Nostr private key (starting with `nsec...`). This is required for your mostro instance to sign events.
    *   `relays`: A list of Nostr relays your mostro instance will connect to. You can use the default list or add your own.

2.  **Lightning Network Settings**:
    *   `lnd_cert_file`: The full path to your LND node's `tls.cert` file within the StartOS filesystem.
    *   `lnd_macaroon_file`: The full path to your LND node's `admin.macaroon` file.
    *   `lnd_grpc_host`: The gRPC host and port for your LND node (e.g., `your-lnd-node.local:10009`).

### Usage

Once configured, Mostro will start and connect to the Nostr relays and your LND node. You can then interact with your Mostro instance using a compatible Nostr client.

For more detailed information about Mostro and how to use it, please refer to the official [Mostro documentation](https://github.com/MostroP2P/mostro). 