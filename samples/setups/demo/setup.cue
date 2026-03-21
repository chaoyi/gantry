package demo

import (
	"github.com/chaoyi/gantry/schemas"
	"gantry.dev/samples/services/postgres"
	"gantry.dev/samples/services/redis"
	"gantry.dev/samples/services/webapp"
)

_raw: {
	db: postgres.#Postgres & {
		config: password: "devpass"
	}

	cache: redis.#Redis

	app: webapp.#WebApp & {
		config: {
			database_url: "postgres://db:5432/app"
			redis_url:    "redis://cache:6379"
		}
		start_after: ["db.ready", "cache.ready"]
		probes: http: depends_on: ["db.ready", "cache.ready"]
	}

	worker: webapp.#WebApp & {
		config: {
			database_url: "postgres://db:5432/app"
			redis_url:    "redis://cache:6379"
		}
		env: MODE: "worker"
		start_after: ["db.ready", "cache.ready"]
		probes: {
			queue: {
				probe: {type: "log", success: "consuming from queue"}
				depends_on: ["cache.ready"]
			}
			ready: {
				probe: type: "meta"
				depends_on: ["queue"]
			}
		}
	}
}

_qualified: (schemas.#Qualify & {input: _raw}).output

services: _qualified

targets: {
	"db-ready": {
		probes: ["db.ready"]
	}
	integration: {
		probes: ["app.ready", "worker.ready"]
		depends_on: ["db-ready"]
	}
}

defaults: {
	tcp_probe_timeout: "10s"
	log_probe_timeout: "30s"
	probe_backoff: {
		initial:    "100ms"
		max:        "5s"
		multiplier: 2
	}
}
