#!/bin/sh
set -e

# Verify config file exists (tests templated filename + bind mount)
CONFIG_FILE="${CONFIG_FILE:-config.toml}"
if [ -f "$CONFIG_FILE" ]; then
    echo "webapp: loaded config from $CONFIG_FILE"
else
    echo "webapp: WARNING: config file $CONFIG_FILE not found"
fi

if [ "$MODE" = "worker" ]; then
    echo "webapp: consuming from queue"
fi

echo "webapp: listening on port ${PORT:-8080}"
socat TCP-LISTEN:${PORT:-8080},fork,reuseaddr SYSTEM:'echo ok' &
wait
