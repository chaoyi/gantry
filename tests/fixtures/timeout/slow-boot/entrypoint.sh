#!/bin/sh
# Configurable boot delay via BOOT_DELAY env var (default 10s)
DELAY=${BOOT_DELAY:-10}
echo "slow-boot: starting (delay=${DELAY}s)"
sleep "$DELAY"
echo "slow-boot: ready"
socat TCP-LISTEN:8080,fork,reuseaddr SYSTEM:'echo ok' &
wait
