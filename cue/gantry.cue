package gantry

import "strings"

// ── Types ──

#Service: {
	config?:          _
	container_name?:  string
	image:            #Image
	env?:             {[string]: string}
	ports?:           [...string]
	volumes?:         [...string]
	command?:         string | [...string]
	files?:           [...#File]
	start_after?:     [...string]
	restart_on_fail?: bool
	probes: {[string]: #ProbeEntry}
}

#Image: string | {build: #BuildDef}

#BuildDef: {
	context:    string
	dockerfile: string
}

#ProbeEntry: {
	probe:       #Probe
	depends_on?: [...string]
}

#Probe: #TcpProbe | #LogProbe | #MetaProbe

#TcpProbe: {
	type:     "tcp"
	port:     int & >0 & <=65535
	timeout?: string
}

#LogProbe: {
	type:     "log"
	success:  string
	failure?: string
	timeout?: string
}

#MetaProbe: {
	type: "meta"
}

#File: {
	src:       string
	dst:       string
	template?: bool
	context?: _   // any JSON-serializable value (strings, numbers, bools, arrays, objects)
}

#Target: {
	probes:      [...string]
	depends_on?: [...string]
}

#Defaults: {
	tcp_probe_timeout?: string
	log_probe_timeout?: string
	restart_on_fail?:   bool
	probe_backoff?:     #Backoff
}

#Backoff: {
	initial?:    string
	max?:        string
	multiplier?: number & >0
}

// ── Setup ──
//
// Declare services with local refs ("port" instead of "db.port").
// Qualification (ref prefixing + auto-ready) is automatic.
//
// Usage:
//   gantry.#Setup & {
//       input: { db: ..., app: ... }
//       targets: { integration: probes: ["app.ready"] }
//   }

#Setup: {
	name:   string
	input: {[string]: #Service}
	services: (#Qualify & {_name_: name, "input": input}).output
	targets?: {[string]: #Target}
	defaults?: #Defaults
	// Shared files rendered to output/shared/, not belonging to any service.
	// Use for configs that reference multiple services' values.
	files?: [...#File]
}

// ── Qualify (internal) ──

#Qualify: {
	_name_: string
	input: {[string]: #Service}
	output: {
		for svcName, svc in input {
			"\(svcName)": {
				if svc.container_name == _|_ {container_name: "\(_name_)-\(svcName)"}
				if svc.container_name != _|_ {container_name: svc.container_name}
				if svc.config != _|_ {config: svc.config}
				image: svc.image
				if svc.env != _|_ {env: svc.env}
				if svc.ports != _|_ {ports: svc.ports}
				if svc.volumes != _|_ {volumes: svc.volumes}
				if svc.command != _|_ {command: svc.command}
				if svc.files != _|_ {files: svc.files}
				if svc.restart_on_fail != _|_ {restart_on_fail: svc.restart_on_fail}
				if svc.start_after != _|_ {
					start_after: (#QualifyRefs & {name: svcName, refs: svc.start_after}).out
				}
				probes: {
					for pn, pe in svc.probes {
						"\(pn)": {
							probe: pe.probe
							if pe.depends_on != _|_ {
								depends_on: (#QualifyRefs & {name: svcName, refs: pe.depends_on}).out
							}
						}
					}
					if svc.probes.ready == _|_ {
						let _all = [for pn, _ in svc.probes {"\(svcName).\(pn)"}]
						if len(_all) > 0 {
							ready: {
								probe: type: "meta"
								depends_on: _all
							}
						}
					}
				}
			}
		}
	}
}

#QualifyRef: {
	name: string
	ref:  string
	out:  string
	if strings.Contains(ref, ".") {out: ref}
	if !strings.Contains(ref, ".") {out: "\(name).\(ref)"}
}

#QualifyRefs: {
	name: string
	refs: [...string]
	out: [for r in refs {(#QualifyRef & {"name": name, "ref": r}).out}]
}
