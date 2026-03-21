package minimal

import (
	"github.com/chaoyi/gantry/schemas"
	"gantry.dev/samples/services/postgres"
)

_raw: {
	db: postgres.#Postgres & {
		config: password: "devpass"
	}
}

_qualified: (schemas.#Qualify & {input: _raw}).output

services: _qualified

targets: {
	"db-ready": {
		probes: ["db.ready"]
	}
}
