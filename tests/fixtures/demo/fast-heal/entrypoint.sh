#!/bin/sh
# FAST SELF-HEAL: reconnects within ~2s
DEP_HOST=${DEP_HOST:-postgres}
DEP_PORT=${DEP_PORT:-5432}

while ! nc -z "$DEP_HOST" "$DEP_PORT" 2>/dev/null; do
  echo "fast-heal: waiting for $DEP_HOST:$DEP_PORT"
  sleep 1
done
echo "fast-heal: dependency connected"

socat TCP-LISTEN:8080,fork,reuseaddr SYSTEM:'echo ok' &
PID=$!
while true; do
  if ! nc -z "$DEP_HOST" "$DEP_PORT" 2>/dev/null; then
    echo "fast-heal: dependency lost"
    kill $PID 2>/dev/null
    wait $PID 2>/dev/null
    while ! nc -z "$DEP_HOST" "$DEP_PORT" 2>/dev/null; do
      sleep 1
    done
    echo "fast-heal: dependency connected"
    socat TCP-LISTEN:8080,fork,reuseaddr SYSTEM:'echo ok' &
    PID=$!
  fi
  sleep 2
done
