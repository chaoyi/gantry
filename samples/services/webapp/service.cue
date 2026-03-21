package webapp

import "github.com/chaoyi/gantry/schemas"

#WebApp: schemas.#Service & {
	config: {
		port:         int | *8080
		log_level:    string | *"info"
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
		if config.redis_url != _|_ {
			REDIS_URL: config.redis_url
		}
	}

	ports: ["\(config.port)"]

	files: [
		{
			src:      "services/webapp/config.toml.tmpl"
			dst:      "config.toml"
			template: true
			context: {
				port:      config.port
				log_level: config.log_level
				if config.redis_url != _|_ {
					redis_url: config.redis_url
				}
			}
		},
	]

	probes: {
		http: {
			probe: {type: "tcp", port: config.port}
		}
	}
}
