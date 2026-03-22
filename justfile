default:
    @just --list

test:
    cargo test --lib

check:
    cargo fmt --check
    cargo clippy -- -D warnings
    cue vet ./cue/...

# 6-service demo: 4 converge recovery behaviors
demo:
    #!/usr/bin/env bash
    set -euo pipefail
    cd tests/fixtures/demo
    docker compose down --timeout 5 2>/dev/null || true
    docker compose build
    docker compose up --no-start
    docker compose start gantry
    echo "  http://localhost:9090"
    docker compose logs -f gantry

demo-down:
    cd tests/fixtures/demo && docker compose down --timeout 5

fuzz seed='42' rounds='50':
    python3 tests/fuzz_ops.py {{seed}} {{rounds}}
