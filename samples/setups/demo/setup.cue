package demo

import (
	"github.com/chaoyi/gantry"
	"github.com/chaoyi/gantry/samples/services/postgres"
	"github.com/chaoyi/gantry/samples/services/redis"
	"github.com/chaoyi/gantry/samples/services/webapp"
)

// Shared infrastructure
let _db = postgres.#Postgres & {config: password: "devpass"}
let _cache = redis.#Redis
let _dbUrl = "postgres://db:\(_db.config.port)/app"
let _redisUrl = "redis://cache:\(_cache.config.port)"
let _env = "development"
let _appPort = 8080
let _workerPort = 8080
let _routesFile = "routes-\(_env).conf"

gantry.#Setup & {
	input: {
		db:    _db
		cache: _cache

		// Web app — rendered config + shared routes mounted
		app: webapp.#WebApp & {
			config: {
				env:          _env
				database_url: _dbUrl
				redis_url:    _redisUrl
			}
			volumes: ["./shared/\(_routesFile):/app/routes.conf:ro"]
			start_after: ["db.ready", "cache.ready"]
			probes: http: depends_on: ["db.ready", "cache.ready"]
		}

		// Worker — same template, shared routes mounted
		worker: webapp.#WebApp & {
			config: {
				database_url: _dbUrl
				redis_url:    _redisUrl
			}
			command: ["/entrypoint.sh"]
			env: MODE: "worker"
			volumes: ["./shared/\(_routesFile):/app/routes.conf:ro"]
			start_after: ["db.ready", "cache.ready"]
			probes: queue: {
				probe: {type: "log", success: "consuming from queue"}
				depends_on: ["cache.ready"]
			}
		}
	}

	targets: {
		infra: probes: ["db.ready", "cache.ready"]
		integration: {
			probes:     ["app.ready", "worker.ready"]
			depends_on: ["infra"]
		}
	}

	// Shared file — references values from multiple services
	files: [{
		src:      "shared/routes.conf.tmpl"
		dst:      "routes-{{ env }}.conf"
		template: true
		context: {
			env:         _env
			api_port:    _appPort
			worker_port: _workerPort
		}
	}]

	defaults: {
		tcp_probe_timeout:  "10s"
		log_probe_timeout:  "15s"
		restart_on_fail:    true
		probe_backoff: {
			initial:    "100ms"
			max:        "3s"
			multiplier: 2
		}
	}
}
