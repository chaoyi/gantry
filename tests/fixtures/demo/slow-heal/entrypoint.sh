#!/bin/sh
# SLOW SELF-HEAL: reconnects but takes ~20s
DEP_HOST=${DEP_HOST:-redis}
DEP_PORT=${DEP_PORT:-6379}
HEAL_DELAY=${HEAL_DELAY:-20}

while ! nc -z "$DEP_HOST" "$DEP_PORT" 2>/dev/null; do
  echo "slow-heal: waiting for $DEP_HOST:$DEP_PORT"
  sleep 1
done
echo "slow-heal: dependency connected"

socat TCP-LISTEN:8080,fork,reuseaddr SYSTEM:'echo ok' &
PID=$!
while true; do
  if ! nc -z "$DEP_HOST" "$DEP_PORT" 2>/dev/null; then
    echo "slow-heal: dependency lost"
    kill $PID 2>/dev/null
    wait $PID 2>/dev/null
    # Slow recovery
    WAITED=0
    while [ $WAITED -lt $HEAL_DELAY ]; do
      sleep 1
      WAITED=$((WAITED + 1))
    done
    if nc -z "$DEP_HOST" "$DEP_PORT" 2>/dev/null; then
      echo "slow-heal: dependency connected"
      socat TCP-LISTEN:8080,fork,reuseaddr SYSTEM:'echo ok' &
      PID=$!
    fi
  fi
  sleep 2
done
