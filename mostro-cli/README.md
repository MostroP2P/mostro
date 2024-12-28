# Mostro CLI 🧌

![Mostro-logo](static/logo.png)

Very simple command line interface that show all new replaceable events from [Mostro](https://github.com/MostroP2P/mostro)

## Requirements:

0. You need Rust version 1.64 or higher to compile.
1. You will need a lightning network node

## Install dependencies:

To compile on Ubuntu/Pop!\_OS, please install [cargo](https://www.rust-lang.org/tools/install), then run the following commands:

```
$ sudo apt update
$ sudo apt install -y cmake build-essential pkg-config
```

## Install

To install you need to fill the env vars (`.env`) on the with your own private key and add a Mostro pubkey.

```
$ git clone https://github.com/MostroP2P/mostro-cli.git
$ cd mostro-cli
$ cp .env-sample .env
$ cargo run
```

# Usage

```
Commands:
  listorders       Requests open orders from Mostro pubkey
  neworder         Create a new buy/sell order on Mostro
  takesell         Take a sell order from a Mostro pubkey
  takebuy          Take a buy order from a Mostro pubkey
  addinvoice       Buyer add a new invoice to receive the payment
  getdm            Get the latest direct messages from Mostro
  fiatsent         Send fiat sent message to confirm payment to other user
  release          Settle the hold invoice and pay to buyer
  cancel           Cancel a pending order
  rate             Rate counterpart after a successful trade
  dispute          Start a dispute
  admcancel        Cancel an order (only admin)
  admsettle        Settle a seller's hold invoice (only admin)
  admlistdisputes  Requests open disputes from Mostro pubkey
  admaddsolver     Add a new dispute's solver (only admin)
  admtakedispute   Admin or solver take a Pending dispute (only admin)
  help             Print this message or the help of the given subcommand(s)

Options:
  -v, --verbose
  -n, --nsec <NSEC>
  -m, --mostropubkey <MOSTROPUBKEY>
  -r, --relays <RELAYS>
  -p, --pow <POW>
  -h, --help                         Print help
  -V, --version                      Print version
```

# Examples

```
$ mostro-cli -m npub1ykvsmrmw2hk7jgxgy64zr8tfkx4nnjhq9eyfxdlg3caha3ph0skq6jr3z0 -n nsec1...5ssky7pw -r 'wss://nos.lol,wss://relay.damus.io,wss://nostr-pub.wellorder.net,wss://nostr.mutinywallet.com,wss://relay.nostr.band,wss://nostr.cizmar.net,wss://140.f7z.io,wss://nostrrelay.com,wss://relay.nostrr.de' listorders

# You can set the env vars to avoid the -m, -n and -r flags
$ export MOSTROPUBKEY=npub1ykvsmrmw2hk7jgxgy64zr8tfkx4nnjhq9eyfxdlg3caha3ph0skq6jr3z0
$ export NSEC=nsec1...5ssky7pw
$ export RELAYS='wss://nos.lol,wss://relay.damus.io,wss://nostr-pub.wellorder.net,wss://nostr.mutinywallet.com,wss://relay.nostr.band,wss://nostr.cizmar.net,wss://140.f7z.io,wss://nostrrelay.com,wss://relay.nostrr.de'
$ mostro-cli listorders

# Create a new buy order
$ mostro-cli neworder -k buy -c ves -f 1000 -m "face to face"

# Cancel a pending order
$ mostro-cli cancel -o eb5740f6-e584-46c5-953a-29bc3eb818f0

# Create a new sell range order with Proof or work difficulty of 10
$ mostro-cli neworder -p 10 -k sell -c ars -f 1000-10000 -m "face to face"
```

## Progress Overview
- [x] Displays order list
- [x] Take orders (Buy & Sell)
- [x] Posts Orders (Buy & Sell)
- [x] Sell flow
- [x] Buy flow
- [x] Maker cancel pending order
- [x] Cooperative cancellation
- [x] Buyer: add new invoice if payment fails
- [x] Rate users
- [x] Dispute flow (users)
- [x] Dispute management (for admins)
- [x] Create buy orders with LN address
- [x] Direct message with peers (use nip-17)
- [x] Conversation key management
- [x] Add a new dispute's solver (for admins)
- [ ] Identity management (Nip-06 support)
- [ ] List own orders
