# Mostro StartOS Wrapper

This is the StartOS wrapper for [Mostro](https://github.com/MostroP2P/mostro), a peer-to-peer lightning exchange over Nostr.

## What is Mostro?

Mostro is a decentralized peer-to-peer exchange that allows users to trade Bitcoin for fiat currencies using the Lightning Network and Nostr protocol. It provides a secure, non-custodial way to exchange Bitcoin without relying on centralized exchanges.

## Features

- **Non-custodial**: You maintain control of your funds at all times
- **Peer-to-peer**: Direct trades between users without intermediaries
- **Lightning Network**: Fast, low-fee Bitcoin transactions
- **Nostr integration**: Decentralized communication protocol
- **Privacy-focused**: No KYC requirements

## Installation

1. Install the `.s9pk` package on your StartOS device
2. Configure your Nostr private key and Lightning Network settings
3. Start the service

## Configuration

### Nostr Settings
- **nsec_privkey**: Your Mostro Nostr private key (required)
- **relays**: List of Nostr relays to connect to (defaults provided)

### Lightning Network Settings
- **lnd_cert_file**: Path to your LND node's TLS certificate
- **lnd_macaroon_file**: Path to your LND node's admin macaroon
- **lnd_grpc_host**: Your LND node's gRPC host and port

## Usage

Once installed and configured, Mostro will be available at the configured RPC port (default: 8000). You can interact with it through the StartOS interface or directly via the RPC API.

## Support

- [Mostro GitHub Repository](https://github.com/MostroP2P/mostro)
- [Mostro Documentation](https://mostro.network/)
- [StartOS Documentation](https://docs.start9.com/)

## License

MIT License - see the LICENSE file for details. 