#!/bin/sh
echo "Reading database URL from settings.toml..."
DATABASE_URL=$(awk -F'"' '/url *= */ {print $2}' settings.toml)
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
echo "Done!"
