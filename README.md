# Gantry

Fast docker compose startup for testing. Replaces `depends_on` and `healthcheck` with dependency-aware probes that know when services are actually ready — not just running.

```
code change → gantry replace <service> → gantry converge <target> → ready → run tests
```

## Quick Start

```bash
git clone https://github.com/chaoyi/gantry && cd gantry/tests/fixtures/demo
docker compose up --no-start && docker compose start gantry
curl -X POST localhost:9090/api/converge/target/integration
# http://localhost:9090 — live graph UI
```

## gantry.yaml

Add gantry as a service in your `docker-compose.yml`, then write `gantry.yaml` to describe how your services become ready:

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
```

Each service has **probes** — health checks that tell gantry when the service is actually usable:

- `tcp` — port is open and accepting connections
- `log` — a pattern appeared in the container's log output (e.g. "ready to accept connections")

Probes can depend on other probes via `depends_on`. If a dependency goes red, its dependents go red too. When a dependency recovers, dependents are reprobed in order. A `ready` probe is auto-generated per service that aggregates all its other probes.

`start_after` controls startup order — `app` won't start until `db.ready` is green.

**Targets** define what "ready for testing" means. `converge` brings a target to green: starts stopped services in dependency order, reprobes anything stale, restarts services that are still failing (once), then reprobes again.

## API

`GET /api` returns the full interface. All POST ops accept `?timeout=N` (default 60s). One operation at a time (409 if busy).

```
GET  /api/graph                       Dependency graph + live state
GET  /api/status/service/:name        Probe-level detail

POST /api/converge/target/:name       Bring target to ready
POST /api/start/service/:name         Start + probe
POST /api/stop/service/:name          Stop (red cascades to dependents)
POST /api/restart/service/:name       Stop + start + reprobe
POST /api/replace/service/:name       Rebuild container + start + reprobe
POST /api/reprobe/service/:name       Re-check without restarting
POST /api/reload                      Reload gantry.yaml

WS   /api/ws                          Live event stream
```

## CUE Codegen (optional)

For larger setups, `gantry-cue` generates both `docker-compose.yml` and `gantry.yaml` from CUE definitions. This gives you:

- **Composable services** — define postgres/redis/your-app as reusable templates, mix into different setups
- **Config templating** — render config files with [Tera](https://keats.github.io/tera/) (`{{ var }}` syntax)
- **Validated output** — CUE type-checks your config, generator verifies all referenced files exist
- **Single source of truth** — one CUE setup produces both compose and gantry config, guaranteed consistent

See `samples/` for a working example.

```bash
gantry-cue setups/integration
cd output/integration && docker compose up --no-start && docker compose start gantry
```

## License

MIT. Uses [ELK.js](https://github.com/kieler/elkjs) (EPL-2.0) for graph layout.
