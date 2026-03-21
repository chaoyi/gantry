package schemas

// Service definition — a container with probes, files, and dependencies.
#Service: {
	config?:      _
	image:        #Image
	env?:         {[string]: string}
	ports?:       [...string]
	files?:       [...#File]
	start_after?: [...string]
	probes: {[string]: #ProbeEntry}
}

// Image: pre-built reference or build definition.
#Image: string | {build: #BuildDef}

#BuildDef: {
	context:    string
	dockerfile: string
}

// ProbeEntry: a health check with optional dependencies on other probes.
#ProbeEntry: {
	probe:       #Probe
	depends_on?: [...string]
}

// Probe types — must match what gantry supervisor accepts.
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

// File: copy or template-render into the service's output directory.
#File: {
	src:       string
	dst:       string
	template?: bool
	context?: {[string]: string | number | bool}
}
