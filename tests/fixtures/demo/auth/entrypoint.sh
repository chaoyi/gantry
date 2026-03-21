#!/bin/sh
echo "auth: starting..."
sleep 1
echo "auth: connecting to database..."
sleep 2
echo "auth: database connected"
sleep 1
echo "auth: connecting to redis..."
sleep 1
echo "auth: session store connected"
echo "auth: ready"
exec socat TCP-LISTEN:8082,fork,reuseaddr SYSTEM:'echo ok'
