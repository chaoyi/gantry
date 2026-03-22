# Gantry

Dependency-aware startup and health probing for docker compose. Starts services in order, probes readiness, and restarts failures — so your tests begin when everything is actually up.

## Quick Start

```bash
git clone https://github.com/chaoyi/gantry
cd gantry/tests/fixtures/demo
docker compose build
docker compose up --no-start && docker compose start gantry
curl -X POST localhost:9090/api/converge/target/app?timeout=120
```

Open http://localhost:9090 for the live dependency graph.

## How It Works

Add gantry as a service in your `docker-compose.yml`:

```yaml
services:
  gantry:
    image: ghcr.io/chaoyi/gantry:latest
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock
      - ./gantry.yaml:/etc/gantry/config.yaml:ro
    ports:
      - "9090:9090"
  # ... your services (no depends_on or healthcheck needed)
```

Write `gantry.yaml` to describe how your services become ready:

```yaml
services:
  db:
    container: myapp-db-1
    probes:
      port:
        probe: { type: tcp, port: 5432, timeout: 15s }
      ready:
        probe: { type: log, success: "ready to accept connections", timeout: 30s }
        depends_on: [db.port]
  app:
    container: myapp-app-1
    start_after: [db.ready]
    probes:
      http:
        probe: { type: tcp, port: 8080 }
        depends_on: [db.ready]

targets:
  integration:
    probes: [app.ready]

defaults:
  restart_on_fail: true
```

### Probes

Health checks that tell gantry when a service is usable:

- **tcp** — port is accepting connections. Retries with backoff during start; single check during reprobe.
- **log** — pattern matched in container logs. Requires `success`; optional `failure` for fast detection. Scans existing logs first (last match wins), then streams new output.

Probe `timeout` controls how long to wait during start. A `ready` meta-probe is auto-generated per service (aggregates all its probes), or you can override it.

### Startup Order and Health Propagation

**`start_after`** controls when a container starts — it won't start until the listed probes are green.

**`depends_on`** controls health propagation — if a dependency goes red, dependents go red too, even if their own check would pass.

Both typically reference the same upstream probes but serve different purposes: `start_after` prevents premature startup, `depends_on` tracks ongoing health.

### Converge

The main operation. Brings a target to green by looping:

1. **Start** stopped services — each waits for its `start_after` deps, then starts and probes with retry. All in parallel.
2. **Long-probe** stale and red probes with retry and backoff — the self-healing window. When a probe fails and the service has `restart_on_fail: true`, it's stopped immediately without waiting for slow probes.
3. If all green → done. Otherwise, stopped services loop back to step 1. Each service restarts at most once.

Options:
- `?timeout=N` (default 60s) — total time cap. On timeout, returns immediately; in-flight probes are cancelled.
- `?skip_restart=true` — run steps 1-2 only (diagnose without restarting).

### Recovery Behaviors

| Behavior | `restart_on_fail` | What happens |
|----------|-------------------|-------------|
| **Crash** (container exits) | `true` (default) | started fresh in step 1 |
| **Stuck** (running but broken) | `true` (default) | `failure` pattern detected, restarted |
| **Slow self-heal** | `true` (default) | probe times out, restart is faster |
| **Fast self-heal** | `false` | recovers during step 2, no restart |

Services with `restart_on_fail: false` that remain red are left as-is — converge returns `failed` with per-probe errors.

### Docker Event Watching

Gantry watches Docker events via the socket. If a container dies or starts externally, state updates automatically — no polling needed.

## API

`GET /api` returns the full interface. One operation at a time (409 if busy). All POST operations accept `?timeout=N` in seconds (default 60).

```
GET  /api/graph                       Dependency graph + live state

POST /api/converge/target/:name       Bring a target to green
POST /api/start/service/:name         Start and probe a service
POST /api/stop/service/:name          Stop (red cascades to dependents)
POST /api/restart/service/:name       Stop → start → probe
POST /api/reprobe/service/:name       Re-check probes
POST /api/reprobe/target/:name        Re-check target probes
POST /api/reprobe/all                 Re-check all probes
POST /api/message                     Post to event stream

WS   /api/ws                          Live events (snapshot on connect)
```

To pick up code changes: `docker compose build <svc> && docker compose up --no-start <svc>`, then `POST /api/restart/service/<svc>`.

## CUE Codegen (optional)

For larger setups, `gantry-cue` generates both `docker-compose.yml` and `gantry.yaml` from [CUE](https://cuelang.org/) definitions. See [`samples/`](samples/) for a working example with shared config, templated filenames, volumes, and reusable service templates.

```bash
go install cuelang.org/go/cmd/cue@latest
gh release download latest --repo chaoyi/gantry --pattern gantry-cue && chmod +x gantry-cue

cd samples
cue mod tidy
./gantry-cue setups/demo
cd output/demo && docker compose build && docker compose up --no-start && docker compose start gantry
```

## License

MIT. Uses [ELK.js](https://github.com/kieler/elkjs) (EPL-2.0) for graph layout.
