#!/bin/sh
set -e

# Check if the settings.toml file exists, if not, create a new one
if [ ! -f /config/settings.toml ]; then
  echo "settings.toml not found, creating a new one from template (default)."
  cp /mostro/settings.toml /config/settings.toml
fi

# Run application (Mostro creates mostro.db at startup if missing)
exec /usr/local/bin/mostrod -d /config
