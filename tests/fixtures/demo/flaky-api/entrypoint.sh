#!/bin/sh
# flaky-api: works but crashes randomly after 60-120s.
# restart_on_fail=true — converge restarts it automatically.
trap 'kill $HTTP_PID 2>/dev/null; exit 0' TERM

echo "flaky-api: starting"
echo "flaky-api: dependency connected"

LIFETIME=$((60 + $(od -An -N1 -tu1 /dev/urandom | tr -d ' ') % 60))
echo "flaky-api: listening on :8080 (will crash in ${LIFETIME}s)"
socat TCP-LISTEN:8080,fork,reuseaddr SYSTEM:'echo HTTP/1.1 200 OK; echo; echo ok' &
HTTP_PID=$!
sleep "$LIFETIME" & wait $!
echo "flaky-api: crashing!"
kill $HTTP_PID 2>/dev/null
exit 1
