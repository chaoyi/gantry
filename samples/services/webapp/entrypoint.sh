#!/bin/sh
if [ "$MODE" = "worker" ]; then
    echo "consuming from queue"
fi
echo "listening on port ${PORT:-8080}"
python3 -c "import http.server; http.server.HTTPServer(('', int('${PORT:-8080}')), http.server.SimpleHTTPRequestHandler).serve_forever()"
