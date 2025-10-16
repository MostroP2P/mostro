#!/bin/sh
echo "Clean project"
if ls sqlx-data.json 1> /dev/null 2>&1; then
    echo "Deleting old sqlx-data.json"
    rm ./sqlx-data.json
fi

echo "Reading database URL from settings.toml..."
DATABASE_URL=$(grep -A 10 '^\[database\]' settings.tpl.toml | grep '^url =' | cut -d'"' -f2)
export DATABASE_URL
echo "Database URL is: $DATABASE_URL"
if ls mostro.db* 1> /dev/null 2>&1; then
    echo "Deleting old database files..."
    rm mostro.db*
else
    echo "No old database files found."
fi
echo "Creating new database..."
sqlx database create
echo "Running migrations..."
sqlx migrate run
echo "Preparing offline file for CI on github!"
cargo sqlx prepare
echo "Check json db file is ok!"
if cargo sqlx prepare --check
then
  echo "Success: sqlx-json is correct"
  exit 0
else
  echo "Failure: sqlx-json has issues" >&2
  exit 1
fi

