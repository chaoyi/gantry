#!/bin/sh
# STUCK: stays running but enters broken state when dependency is lost
DEP_HOST=${DEP_HOST:-postgres}
DEP_PORT=${DEP_PORT:-5432}
trap 'kill $PID 2>/dev/null; exit 0' TERM

while ! nc -z "$DEP_HOST" "$DEP_PORT" 2>/dev/null; do
  echo "stuck-svc: waiting for $DEP_HOST:$DEP_PORT"
  sleep 1 & wait $!
done
echo "stuck-svc: dependency connected"

socat TCP-LISTEN:8080,fork,reuseaddr SYSTEM:'echo ok' &
PID=$!
while true; do
  if ! nc -z "$DEP_HOST" "$DEP_PORT" 2>/dev/null; then
    kill $PID 2>/dev/null
    wait $PID 2>/dev/null
    echo "stuck-svc: dependency check failed"
    # Broken forever — only restart fixes this
    while true; do
      echo "stuck-svc: dependency check failed"
      sleep 5 & wait $!
    done
  fi
  sleep 2 & wait $!
done
