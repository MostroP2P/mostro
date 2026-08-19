#!/bin/sh
set -e

# Check if the settings.toml file exists, if not, create a new one.
# `install -m 600` rather than `cp`: the file receives nsec_privkey once edited,
# and `cp` would keep whatever mode the template in the image happens to have.
if [ ! -f /config/settings.toml ]; then
  echo "settings.toml not found, creating a new one from template (default)."
  install -m 600 /mostro/settings.toml /config/settings.toml
fi

# Run application (Mostro creates mostro.db at startup if missing)
exec /usr/local/bin/mostrod -d /config
