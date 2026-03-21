package schemas

// Setup: top-level structure exported by `cue export`.
// Use this in your setup.cue: `schemas.#Setup & { services: ... }`
#Setup: {
	services: {[string]: #Service}
	targets?: {[string]: #Target}
	defaults?: #Defaults
}

// Target: a named goal (e.g. "integration") defined by which probes must be green.
#Target: {
	probes:      [...string]
	depends_on?: [...string]
}

// Defaults: global probe timeouts and backoff strategy.
#Defaults: {
	tcp_probe_timeout?: string
	log_probe_timeout?: string
	probe_backoff?:     #Backoff
}

#Backoff: {
	initial?:    string
	max?:        string
	multiplier?: number & >0
}
