package webapp

import "github.com/chaoyi/gantry"

#WebApp: gantry.#Service & {
	config: {
		port:         int | *8080
		log_level:    string | *"info"
		env:          string | *"development"
		database_url: string
		redis_url?:   string
	}

	image: build: {
		context:    "services/webapp"
		dockerfile: "Dockerfile"
	}

	env: {
		PORT:         "\(config.port)"
		DATABASE_URL: config.database_url
		LOG_LEVEL:    config.log_level
		CONFIG_FILE:  "/app/config-\(config.env).toml"
		if config.redis_url != _|_ {
			REDIS_URL: config.redis_url
		}
	}

	// Default volume — rendered config mounted into container.
	// Setups can override with their own volumes list.
	volumes: [...string] | *["./services/app/config-\(config.env).toml:/app/config-\(config.env).toml:ro"]

	files: [{
		src:      "services/webapp/config.toml.tmpl"
		dst:      "config-{{ env }}.toml"
		template: true
		context: {
			port:      config.port
			log_level: config.log_level
			env:       config.env
			if config.redis_url != _|_ {
				redis_url: config.redis_url
			}
		}
	}]

	probes: http: probe: {type: "tcp", port: config.port}
}
