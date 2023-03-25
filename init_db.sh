#!/bin/sh
rm mostro.db*
sqlx database create
sqlx migrate run