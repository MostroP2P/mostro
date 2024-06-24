#!/bin/sh

# Check if the settings.toml file exists, if not, create a new one
if [ ! -f /config/settings.toml ]; then
  echo "settings.toml not found, creating a new one from settings.docker (default)."
  cp /mostro/settings.docker.toml /config/settings.toml
fi

# Check if the database file exists, if not, create a new one
if [ ! -f /config/mostro.db ]; then
  echo "Database file not found, creating a new one."
  cp /mostro/empty.mostro.db /config/mostro.db
fi

# Run application
/usr/local/bin/mostrod -d /config