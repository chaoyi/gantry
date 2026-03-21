#!/bin/sh
echo "worker: starting..."
sleep 1
echo "worker: connecting to auth..."
sleep 1
echo "worker: auth connected"
sleep 1
echo "worker: consuming from queue"
echo "worker: ready"
exec socat TCP-LISTEN:8081,fork,reuseaddr SYSTEM:'echo ok'
