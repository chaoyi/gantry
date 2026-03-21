#!/bin/sh
# Flaky service: TCP port randomly goes up/down
# Simulates a service that crashes and recovers intermittently
echo "flaky-svc: starting"
echo "flaky-svc: initialized"

while true; do
  # Up for 5-15 seconds
  UP=$((RANDOM % 11 + 5))
  echo "flaky-svc: listening on :9999 (up for ${UP}s)"
  socat TCP-LISTEN:9999,fork,reuseaddr SYSTEM:'echo ok' &
  PID=$!
  sleep $UP
  kill $PID 2>/dev/null
  wait $PID 2>/dev/null

  # Down for 2-5 seconds
  DOWN=$((RANDOM % 4 + 2))
  echo "flaky-svc: port down (down for ${DOWN}s)"
  sleep $DOWN
done
