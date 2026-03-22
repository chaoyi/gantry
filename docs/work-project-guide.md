# AI-Assisted Development with Gantry

A guide for setting up an AI-driven development loop in a monorepo: from source code to testable services, with observability, persistence, and team collaboration.

## Overview

```
source code → build → docker image → gantry converge → green/red → fix → repeat
```

Gantry is the test oracle — it tells AI (and humans) whether changes work. Everything else is infrastructure to make this loop fast and reliable.

## Workspace Layout

A master repo that references everything. AI works in one directory tree.

```
workspace/
  monorepo/                        # work code (submodule or clone)
    services/
      api/src/
      worker/src/
      libs/shared/
    docker/                        # build overlay (commit on top of existing code)
      manifest.yaml                # service → source → build mapping
      build.sh                     # shared build script
      api/Dockerfile
      worker/Dockerfile
    gantry/                        # test infrastructure
      cue.mod/module.cue
      services/                    # reusable CUE service templates
        api/service.cue
        worker/service.cue
        postgres/service.cue
      setups/                      # environment definitions
        minimal/setup.cue          # simplest (1 service + db)
        integration/setup.cue     # full system
        isolated-api/setup.cue    # single service + deps
      justfile
    tests/                         # test scripts
      smoke.py
      regression.py
    QA.md                          # AI-maintained QA log
  tools/                           # external tool repos (submodules)
    data-loader/                   # each has its own CLAUDE.md
    api-client/
    db-migrator/
  results/                         # test history (separate repo)
  persist.py                       # save test results
  CLAUDE.md                        # AI reads this first
```

## Build System

### manifest.yaml

The single source of truth for what exists and how to build it:

```yaml
services:
  api:
    source: services/api/
    description: "HTTP API server, serves /v1/* endpoints"
  worker:
    source: services/worker/
    description: "Background job processor, consumes from RabbitMQ"
  migrator:
    source: services/migrator/
    description: "Database migration runner, exits after completion"
```

AI reads this to understand: what services exist, where their code lives, what they do.

### build.sh

One script, takes a service name. Enforces commit-before-build so every image is traceable:

```bash
#!/bin/bash
set -euo pipefail
SERVICE=$1

if ! git diff --quiet HEAD; then
    echo "ERROR: uncommitted changes. Commit first."
    exit 1
fi

SHA=$(git rev-parse --short HEAD)
ROOT=$(git rev-parse --show-toplevel)

# Build binary using existing build system
cd "$ROOT" && make build-$SERVICE

# Package into Docker image tagged with commit SHA
cd "$ROOT/docker/$SERVICE"
docker build --build-arg COMMIT_SHA=$SHA -t $SERVICE:$SHA -t $SERVICE:latest .
echo "Built $SERVICE:$SHA"
```

### Per-service Dockerfile

Minimal — just packages a built binary:

```dockerfile
ARG COMMIT_SHA=unknown
LABEL org.opencontainers.image.revision=$COMMIT_SHA
FROM alpine:3.20
COPY bin/api /api
ENTRYPOINT ["/api"]
```

## CUE Service Templates

### Reusable service definitions

Each service gets a CUE template with typed config:

```cue
package api

import "github.com/chaoyi/gantry"

#API: gantry.#Service & {
    config: {
        port:    int | *8080
        db_url:  string
        log_level: string | *"info"
    }
    image: "api:latest"
    env: {
        PORT:      "\(config.port)"
        DB_URL:    config.db_url
        LOG_LEVEL: config.log_level
    }
    volumes: ["./services/api/config.yaml:/app/config.yaml:ro"]
    probes: {
        http: {
            probe: {type: "tcp", port: config.port}
        }
        ready: {
            probe: {type: "log", success: "server started", failure: "fatal error", timeout: "15s"}
            depends_on: ["http"]
        }
    }
}
```

### Setup files

Compose services with shared config, cross-references, and targets:

```cue
package integration

import (
    "github.com/chaoyi/gantry"
    "mycompany/gantry/services/api"
    "mycompany/gantry/services/worker"
    "mycompany/gantry/services/postgres"
)

let _db = postgres.#Postgres & {config: password: "devpass"}
let _dbUrl = "postgres://db:\(_db.config.port)/app"

gantry.#Setup & {
    input: {
        db: _db
        api: api.#API & {
            config: db_url: _dbUrl
            start_after: ["db.ready"]
            probes: http: depends_on: ["db.ready"]
        }
        worker: worker.#Worker & {
            config: db_url: _dbUrl
            start_after: ["db.ready", "api.ready"]
        }
    }
    targets: {
        infra: probes: ["db.ready"]
        backend: {
            probes: ["api.ready", "worker.ready"]
            depends_on: ["infra"]
        }
    }
    defaults: {
        restart_on_fail: true
        tcp_probe_timeout: "10s"
        log_probe_timeout: "15s"
    }
}
```

### Multiple setup examples

Provide 2-3 so AI learns the pattern and creates its own:

- **minimal/** — one service + database. Fastest iteration for focused work.
- **integration/** — all services. Full regression test before merge.
- **isolated-api/** — API + its direct deps, nothing else. Debug one service.

AI creates throwaway setups for specific scenarios by copying and adapting.

## AI Workflow

### The core loop

```
edit source → git commit → docker/build.sh <service> →
  POST /api/restart/service/<service> →
  POST /api/converge/target/<target> →
    green → persist results → done
    red → read GET /api/graph → understand error → fix → repeat
```

### Discovering what to do

AI reads in order:
1. `CLAUDE.md` — top-level instructions, links to everything
2. `docker/manifest.yaml` — what services exist, where code lives
3. `GET http://localhost:9090/api` — full API interface with descriptions
4. `GET http://localhost:9090/api/graph` — current state of all services
5. Tool repos' `CLAUDE.md` — how to use specific tools

### Debugging failures

When converge returns red:
1. Read `GET /api/graph` — which probes are red, what errors
2. Check container logs — `docker compose logs <service>`
3. Check observability — Grafana dashboards, Prometheus metrics
4. Read the probe error — `failure pattern matched` means the service logged an error; `tcp timed out` means the service isn't listening
5. Fix code, rebuild, restart, converge again

### Making gantry emit markers

Use `POST /api/message` to annotate the event stream:
```bash
curl -X POST localhost:9090/api/message -H 'Content-Type: application/json' \
  -d '{"text": "Starting API timeout fix"}'
```

Shows in gantry's UI as a MSG event — creates a readable timeline of what AI did and when.

## Observability

### Setup

Add as infrastructure services in CUE, in their own target:

```cue
input: {
    prometheus: #Prometheus & {
        config: scrape_targets: ["api:8080", "worker:8081"]
    }
    grafana: #Grafana & {
        config: prometheus_url: "http://prometheus:9090"
    }
}

targets: {
    backend: probes: ["api.ready", "worker.ready"]
    monitoring: {
        probes: ["prometheus.ready", "grafana.ready"]
        // separate target — tests don't wait for monitoring
    }
}
```

### Endpoints

- **Gantry UI**: http://localhost:9090 — live dependency graph, event stream
- **Grafana**: http://localhost:3000 — dashboards (admin/admin)
- **Prometheus**: http://localhost:9090 — metrics targets

### For AI

In CLAUDE.md: "When a service is red but the probe error isn't clear, check Grafana for latency spikes or error rate changes before modifying code."

## External Tools

Tools from other repos run after converge on the compose network:

```bash
# Sync test data
docker run --rm --network demo_default data-loader sync --tables users,orders

# Run API smoke tests
docker run --rm --network demo_default api-smoketest

# Run database migrations
docker run --rm --network demo_default db-migrator migrate
```

Each tool's repo has its own `CLAUDE.md` with usage instructions. The workspace `CLAUDE.md` lists them with short descriptions.

## Persisting Results

### persist.py

AI calls after each test session:

```python
import json, subprocess, requests, datetime, os

def save_run(notes="", run_dir=None):
    sha = subprocess.check_output(
        ["git", "rev-parse", "--short", "HEAD"]
    ).decode().strip()
    run_id = f"{sha}-{int(datetime.datetime.now().timestamp())}"
    run_dir = run_dir or f"results/{run_id}"
    os.makedirs(run_dir, exist_ok=True)

    # Gantry state
    graph = requests.get("http://localhost:9090/api/graph").json()
    json.dump(graph, open(f"{run_dir}/graph.json", "w"), indent=2)

    # Container logs
    subprocess.run(
        ["docker", "compose", "logs", "--no-color"],
        stdout=open(f"{run_dir}/logs.txt", "w"),
        stderr=subprocess.STDOUT,
    )

    # Setup used
    subprocess.run(["cp", "-r", "gantry/setups/", f"{run_dir}/setups/"])

    # AI notes
    if notes:
        open(f"{run_dir}/notes.md", "w").write(notes)

    # Summary
    status = graph.get("status", "unknown")
    services = {s["name"]: s["state"] for s in graph.get("services", [])}
    summary = {
        "run_id": run_id,
        "commit": sha,
        "timestamp": datetime.datetime.now().isoformat(),
        "status": status,
        "services": services,
        "notes": notes,
    }
    json.dump(summary, open(f"{run_dir}/summary.json", "w"), indent=2)

    print(f"Saved: {run_dir} ({status})")
    return run_dir

if __name__ == "__main__":
    import sys
    save_run(sys.argv[1] if len(sys.argv) > 1 else "")
```

### QA.md

AI appends after each session. Becomes a searchable history:

```markdown
# QA Log

## 2026-03-23 — API timeout fix
- **Commit**: abc123
- **Setup**: integration
- **Changed**: services/api/src/handler.rs — increased timeout from 5s to 30s
- **First attempt**: red — worker.queue failed (stale connection after api restart)
- **Fix**: added reconnect logic in worker/src/queue.rs
- **Final result**: green (all services)
- **Run**: results/abc123-1711151234/

## 2026-03-22 — Worker retry logic
- **Commit**: def456
- **Setup**: isolated-worker
- **Changed**: services/worker/src/retry.rs — exponential backoff
- **Result**: green on first attempt
- **Run**: results/def456-1711064834/
```

AI reads past entries to:
- Avoid repeating known fixes
- Understand which services are flaky
- Know which setups to use for which kind of change

## Team Sharing

### Golden setup

A tagged commit where all tests pass:

```bash
git tag golden-2026-03-23
```

Anyone reproduces:
```bash
git checkout golden-2026-03-23
docker/build.sh all
cd gantry/setups/integration && just up
# Open http://localhost:9090
```

### Remote demo

For team members without local setup:
1. Run golden setup on a shared VM
2. Expose ports via tunnel: gantry UI (:9090), Grafana (:3000), the services themselves
3. Share the URL — team sees live dependency graph, can run operations

### Code review integration

Before merging a PR:
```bash
git checkout feature-branch
docker/build.sh api
just -f gantry/justfile up integration
# If green: ready to merge
# If red: push results to PR comment
```

AI does this automatically. The results link in the PR shows exactly what was tested.

## Image → Source Traceability

Every image is labeled with its commit SHA:

```bash
# What commit built this image?
docker inspect api:latest --format '{{index .Config.Labels "org.opencontainers.image.revision"}}'
# → abc123

# What source code?
git show abc123:services/api/src/main.rs

# What was the test result for this build?
ls results/abc123-*/summary.json
```

The chain: image label → commit SHA → source code → test results. All linked, all in git.

## Bug Fix Workflow

When a bug is reported:

1. AI reads bug description
2. AI checks QA.md — has this been seen before?
3. AI creates an isolated setup for the affected service
4. AI reproduces the bug (converge → red, matching the reported error)
5. AI fixes the code
6. AI rebuilds and converges → green
7. AI runs the full integration setup to check for regressions
8. AI persists results and updates QA.md
9. AI creates PR with the fix + test results

## Product Development Workflow

When building a new feature:

1. AI reads the feature spec
2. AI creates a new setup with the services involved
3. AI modifies service code
4. Build → converge → iterate until green
5. AI adds a test script that validates the feature behavior (not just health)
6. AI runs integration setup — full regression
7. AI persists results, updates QA.md, creates PR

## Evolving the System

### Adding a new service

1. Add to `docker/manifest.yaml`
2. Create `docker/<service>/Dockerfile`
3. Create `gantry/services/<service>/service.cue`
4. Add to relevant setups
5. AI learns from existing templates — follows the same pattern

### Adding a new tool

1. Clone/submodule into `tools/`
2. Ensure the tool has a `CLAUDE.md`
3. Add reference in workspace `CLAUDE.md`
4. AI reads the tool's docs when it needs it

### Scaling to more environments

```
gantry/setups/
  dev/             # local development (build from source)
  staging/         # staging images (pinned SHAs from CI)
  perf-test/       # reduced services + load generator
  migration/       # old + new service versions side by side
```

Each setup is a CUE file. Cheap to create, easy to share.

### From test loop to CI/CD

The same gantry setup that AI uses locally can run in CI:

```yaml
# .gitlab-ci.yml
integration-test:
  script:
    - docker/build.sh all
    - cd gantry/setups/integration
    - gantry-cue .
    - cd output && docker compose up --no-start && docker compose start gantry
    - curl -X POST localhost:9090/api/converge/target/backend?timeout=120
    - python3 tests/regression.py
```

Same converge, same probes, same pass/fail criteria. Local and CI are identical.

## CLAUDE.md Template

```markdown
# CLAUDE.md

## Project
Monorepo with N services. Uses gantry for service orchestration and testing.

## Structure
- `docker/manifest.yaml` — service list, source paths, descriptions
- `gantry/setups/` — CUE test environments (start with `minimal/` for fast iteration)
- `tools/` — external tools (see each tool's CLAUDE.md)
- `QA.md` — test history and known issues
- `results/` — persisted test runs

## Build
1. Edit source code
2. `git commit -m "description"`
3. `docker/build.sh <service>`

## Test
```bash
POST http://localhost:9090/api/converge/target/<target>
```
- API docs: `GET http://localhost:9090/api`
- Live UI: http://localhost:9090

## Debug
- Read probe errors: `GET /api/graph`
- Container logs: `docker compose logs <service>`
- Grafana: http://localhost:3000
- Check QA.md for known issues before investigating

## After each session
1. Update QA.md with what you changed and why
2. `python3 persist.py "description"`

## Tools
- data-loader: `docker run --rm --network demo_default data-loader sync`
- api-smoketest: `docker run --rm --network demo_default api-smoketest`
See each tool's CLAUDE.md for details.
```
