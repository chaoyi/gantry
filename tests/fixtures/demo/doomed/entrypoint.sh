#!/bin/sh
# doomed: starts healthy, then stops responding after ~10s.
# restart_on_fail=false — converge cannot fix this automatically.
trap 'kill $HTTP_PID 2>/dev/null; exit 0' TERM

echo "doomed: starting"
while ! nc -z flaky-api 8080 2>/dev/null; do
    echo "doomed: waiting for flaky-api..."
    sleep 1 & wait $!
done
echo "doomed: dependency connected"

ALIVE=$((8 + $(od -An -N1 -tu1 /dev/urandom | tr -d ' ') % 5))
echo "doomed: listening on :8080 (will die in ${ALIVE}s)"
socat TCP-LISTEN:8080,fork,reuseaddr SYSTEM:'echo HTTP/1.1 200 OK; echo; echo ok' &
HTTP_PID=$!
sleep "$ALIVE" & wait $!
kill $HTTP_PID 2>/dev/null
echo "doomed: stopped listening (broken)"

while true; do echo "doomed: still broken"; sleep 10 & wait $!; done
