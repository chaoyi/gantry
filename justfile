set dotenv-load

fixture := "tests/fixtures/demo"

default:
    @just --list

# Unit tests (no Docker)
test:
    cargo test --lib

# Run demo with UI (http://localhost:9090)
demo:
    #!/usr/bin/env bash
    set -euo pipefail
    cd {{fixture}}
    docker compose down --timeout 5 2>/dev/null || true
    docker compose build
    docker compose up --no-start
    docker compose start gantry
    echo ""
    echo "  Gantry UI: http://localhost:9090"
    echo "  10 services, flaky probe, complex deps"
    echo "  Stop: just demo-down"
    echo ""
    docker compose logs -f gantry

# Randomized operation fuzzer (requires running demo)
fuzz seed='42' rounds='50':
    python3 tests/fuzz_ops.py {{seed}} {{rounds}}

# Generate docker-compose.yml + gantry.yaml from a CUE setup
generate setup_dir:
    cargo run -p gantry-cue -- {{setup_dir}}

# Validate CUE schemas
cue-vet:
    cue vet ./cue/schemas/...
