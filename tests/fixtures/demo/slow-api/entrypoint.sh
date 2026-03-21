#!/bin/sh
echo "api: starting initialization..."
sleep 1
echo "api: connecting to auth service..."
sleep 2
echo "api: auth connected"
sleep 1
echo "api: connecting to cache..."
sleep 1
echo "api: cache connected"
echo "api: ready to serve requests"
exec socat TCP-LISTEN:8080,fork,reuseaddr SYSTEM:'echo HTTP/1.1 200 OK\r\n\r\nok'
