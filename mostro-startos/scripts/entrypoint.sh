#!/bin/sh

set -e

CONFIG_FILE="/config/config.json"
SETTINGS_FILE="/config/settings.toml"

# Function to safely get a value from the config file
get_config() {
    jq -r "$1" "$CONFIG_FILE"
}

# Generate settings.toml from config.json
if [ -f "$CONFIG_FILE" ]; then
    echo "Generating settings.toml from config.json"

    # Convert relays from JSON array to TOML array of strings
    RELAYS=$(get_config '.nostr.relays' | jq -c 'if type == "array" then . else [] end')

    cat > "$SETTINGS_FILE" <<EOF
[database]
url = "sqlite:///data/mostro.db"

[lightning]
lnd_cert_file = "$(get_config '.lightning.lnd_cert_file')"
lnd_macaroon_file = "$(get_config '.lightning.lnd_macaroon_file')"
lnd_grpc_host = "$(get_config '.lightning.lnd_grpc_host')"
invoice_expiration_window = 3600
hold_invoice_cltv_delta = 144
hold_invoice_expiration_window = 300
payment_attempts = 3
payment_retries_interval = 60

[nostr]
nsec_privkey = "$(get_config '.nostr.nsec_privkey')"
relays = ${RELAYS}

[mostro]
fee = $(get_config '.mostro.fee')
max_routing_fee = 0.001
max_order_amount = $(get_config '.mostro.max_order_amount')
min_payment_amount = $(get_config '.mostro.min_payment_amount')
expiration_hours = $(get_config '.mostro.expiration_hours')
max_expiration_days = 15
expiration_seconds = 900
user_rates_sent_interval_seconds = 3600
publish_relays_interval = 60
pow = 0
publish_mostro_info_interval = 300
bitcoin_price_api_url = "https://api.yadio.io"

[rpc]
enabled = $(get_config '.rpc.enabled')
listen_address = "0.0.0.0"
port = $(get_config '.rpc.port')

EOF
else
    echo "WARNING: config.json not found. Mostro might not work correctly."
fi

# Check if the database file exists in data volume, if not, create a new one
if [ ! -f /data/mostro.db ]; then
  echo "Database file not found in data volume, creating a new one."
  cp /home/mostrouser/empty.mostro.db /data/mostro.db
fi

# Run application with config directory
/usr/local/bin/mostrod -d /config
