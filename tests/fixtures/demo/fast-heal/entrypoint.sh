#!/bin/sh
# FAST SELF-HEAL: reconnects within ~2s. 1s init delay shows brief probing.
DEP_HOST=${DEP_HOST:-postgres}
DEP_PORT=${DEP_PORT:-5432}
trap 'kill $PID 2>/dev/null; exit 0' TERM

while ! nc -z "$DEP_HOST" "$DEP_PORT" 2>/dev/null; do
  echo "fast-heal: waiting for $DEP_HOST:$DEP_PORT"
  sleep 1 & wait $!
done
echo "fast-heal: initializing..."
sleep 1 & wait $!
echo "fast-heal: dependency connected"

socat TCP-LISTEN:8080,fork,reuseaddr SYSTEM:'echo ok' &
PID=$!
while true; do
  if ! nc -z "$DEP_HOST" "$DEP_PORT" 2>/dev/null; then
    echo "fast-heal: dependency lost"
    kill $PID 2>/dev/null
    wait $PID 2>/dev/null
    while ! nc -z "$DEP_HOST" "$DEP_PORT" 2>/dev/null; do
      sleep 1 & wait $!
    done
    echo "fast-heal: initializing..."
    sleep 1 & wait $!
    echo "fast-heal: dependency connected"
    socat TCP-LISTEN:8080,fork,reuseaddr SYSTEM:'echo ok' &
    PID=$!
  fi
  sleep 2 & wait $!
done
