#!/bin/sh
# SLOW SELF-HEAL: reconnects but takes ~20s. 5s init delay shows visible probing state.
DEP_HOST=${DEP_HOST:-redis}
DEP_PORT=${DEP_PORT:-6379}
HEAL_DELAY=${HEAL_DELAY:-20}
INIT_DELAY=${INIT_DELAY:-5}
trap 'kill $PID 2>/dev/null; exit 0' TERM

while ! nc -z "$DEP_HOST" "$DEP_PORT" 2>/dev/null; do
  echo "slow-heal: waiting for $DEP_HOST:$DEP_PORT"
  sleep 1 & wait $!
done
echo "slow-heal: initializing (${INIT_DELAY}s)..."
sleep "$INIT_DELAY" & wait $!
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
      sleep 1 & wait $!
      WAITED=$((WAITED + 1))
    done
    if nc -z "$DEP_HOST" "$DEP_PORT" 2>/dev/null; then
      echo "slow-heal: initializing (${INIT_DELAY}s)..."
      sleep "$INIT_DELAY" & wait $!
      echo "slow-heal: dependency connected"
      socat TCP-LISTEN:8080,fork,reuseaddr SYSTEM:'echo ok' &
      PID=$!
    fi
  fi
  sleep 2 & wait $!
done
