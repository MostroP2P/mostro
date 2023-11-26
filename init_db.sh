#!/bin/sh
echo "Clean project"
if ls Cargo.lock 1> /dev/null 2>&1; then
    echo "Deleting Cargo.lock"
    rm -rf ./Cargo.lock
fi
if ls sqlx-data.json 1> /dev/null 2>&1; then
    echo "Deleting old sqlx-data.json"
    rm -rf ./sqlx-data.json
fi

cargo clean
echo "Reading database URL from settings.toml..."
DATABASE_URL=$(awk -F'"' '/url *= */ {print $2}' settings.tpl.toml)
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
cargo sqlx prepare --check


