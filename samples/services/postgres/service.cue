package postgres

import "github.com/chaoyi/gantry/schemas"

#Postgres: schemas.#Service & {
	config: {
		port:     int | *5432
		password: string
	}

	image: "postgres:16"

	env: {
		POSTGRES_PASSWORD: config.password
		PGPORT:            "\(config.port)"
	}

	ports: ["\(config.port)"]

	probes: {
		port: {
			probe: {type: "tcp", port: config.port}
		}
		accepting: {
			probe: {type: "log", success: "ready to accept connections"}
			depends_on: ["port"]
		}
	}
}
