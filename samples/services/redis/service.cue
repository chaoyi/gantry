package redis

import "github.com/chaoyi/gantry/schemas"

#Redis: schemas.#Service & {
	config: {
		port: int | *6379
	}

	image: "redis:7-alpine"

	ports: ["\(config.port)"]

	probes: {
		port: {
			probe: {type: "tcp", port: config.port}
		}
		ready: {
			probe: {type: "log", success: "Ready to accept connections"}
			depends_on: ["port"]
		}
	}
}
