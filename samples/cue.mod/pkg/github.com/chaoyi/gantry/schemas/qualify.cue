package schemas

import "strings"

// #Qualify transforms raw service definitions into fully-qualified output:
//   1. Prefixes unqualified refs with "svc." (e.g. "port" → "db.port")
//   2. Auto-generates a "ready" meta probe aggregating all other probes
//
// Usage:
//   _qualified: (schemas.#Qualify & {input: _raw}).output
//   services: _qualified
//
// NOTE: The field pass-through below must stay in sync with #Service fields.
// CUE has no spread operator, so each optional field needs an explicit guard.
#Qualify: {
	input: {[string]: #Service}
	output: {
		for svcName, svc in input {
			"\(svcName)": {
				if svc.config != _|_ {config: svc.config}
				image: svc.image
				if svc.env != _|_ {env: svc.env}
				if svc.ports != _|_ {ports: svc.ports}
				if svc.files != _|_ {files: svc.files}
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
					// Auto-generate ready if not explicit
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

// #QualifyRef adds "name." prefix to bare refs (no dot).
#QualifyRef: {
	name: string
	ref:  string
	out:  string
	if strings.Contains(ref, ".") {out: ref}
	if !strings.Contains(ref, ".") {out: "\(name).\(ref)"}
}

// #QualifyRefs qualifies a list of refs.
#QualifyRefs: {
	name: string
	refs: [...string]
	out: [for r in refs {(#QualifyRef & {"name": name, "ref": r}).out}]
}
