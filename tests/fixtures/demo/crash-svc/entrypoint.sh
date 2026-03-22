#!/bin/sh
# CRASH: exits when dependency is lost
DEP_HOST=${DEP_HOST:-redis}
DEP_PORT=${DEP_PORT:-6379}

while ! nc -z "$DEP_HOST" "$DEP_PORT" 2>/dev/null; do
  echo "crash-svc: waiting for $DEP_HOST:$DEP_PORT"
  sleep 1
done
echo "crash-svc: dependency connected"

socat TCP-LISTEN:8080,fork,reuseaddr SYSTEM:'echo ok' &
PID=$!
while kill -0 $PID 2>/dev/null; do
  if ! nc -z "$DEP_HOST" "$DEP_PORT" 2>/dev/null; then
    echo "crash-svc: dependency lost"
    exit 1
  fi
  sleep 2
done
