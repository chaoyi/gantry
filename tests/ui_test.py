#!/usr/bin/env python3
"""API-level UI state test: verifies all display states the UI renders,
including WebSocket snapshot consistency."""

import requests
import time
import sys
import json

BASE = 'http://localhost:9090'
PASS = 0
FAIL = 0

def api(method, path, timeout=90):
    r = getattr(requests, method)(f'{BASE}/api{path}', timeout=timeout)
    return r.json()

def ws_snapshot():
    """Get WS snapshot — what the UI sees on connect."""
    import websocket
    ws = websocket.create_connection(BASE.replace('http', 'ws') + '/api/ws', timeout=5)
    data = json.loads(ws.recv())
    ws.close()
    return data

def graph():
    g = api('get', '/graph')
    svcs = {s['name']: s for s in g['services']}
    tgts = {t['name']: t for t in g['targets']}
    probes = {}
    for s in g['services']:
        for p in s['probes']:
            probes[f"{s['name']}.{p['name']}"] = p
    return svcs, tgts, probes

def check(name, condition, detail=""):
    global PASS, FAIL
    if condition:
        PASS += 1
        print(f"  PASS  {name}")
    else:
        FAIL += 1
        print(f"  FAIL  {name} — {detail}")

def svc_state(svcs, name):
    return svcs[name]['state'], svcs[name]['runtime']

def probe_state(probes, key):
    return probes[key]['state']

def check_ws_matches_api(label):
    """Verify WS snapshot matches API graph for all services, probes, targets."""
    ws = ws_snapshot()
    s2, t2, p2 = graph()
    print(f"\n  -- WS consistency: {label} --")
    for sname, sdata in s2.items():
        ws_svc = ws.get('services', {}).get(sname, {})
        ws_state = ws_svc.get('state')
        ws_runtime = ws_svc.get('runtime')
        check(f"ws svc {sname} state={sdata['state']}", ws_state == sdata['state'],
              f"ws={ws_state} api={sdata['state']}")
        check(f"ws svc {sname} runtime={sdata['runtime']}", ws_runtime == sdata['runtime'],
              f"ws={ws_runtime} api={sdata['runtime']}")
    for pk, pdata in p2.items():
        svc, probe = pk.split('.', 1)
        ws_ps = ws.get('services', {}).get(svc, {}).get('probes', {}).get(probe, {}).get('state')
        check(f"ws probe {pk}={pdata['state']}", ws_ps == pdata['state'],
              f"ws={ws_ps} api={pdata['state']}")
    for tname, tdata in t2.items():
        ws_ts = ws.get('targets', {}).get(tname, {}).get('state')
        check(f"ws tgt {tname}={tdata['state']}", ws_ts == tdata['state'],
              f"ws={ws_ts} api={tdata['state']}")

# ──────────────────────────────────────────────────────────────
print("=== Test 1: Initial state (fresh gantry, nothing converged) ===")
svcs, tgts, probes = graph()

# All services should be stopped (runtime) and gray (display=stopped, inactive)
for name, s in svcs.items():
    display, runtime = svc_state(svcs, name)
    check(f"{name} runtime=stopped", runtime == 'stopped', f"got {runtime}")
    check(f"{name} display=stopped (gray)", display == 'stopped', f"got {display}")

# All targets should be inactive (stopped=gray)
for name, t in tgts.items():
    check(f"target {name} inactive", t['state'] == 'stopped', f"got {t['state']}")

# All probes should be stopped/red (not green/stale)
for key, p in probes.items():
    check(f"{key} not green", p['state'] != 'green', f"got {p['state']}")

# ──────────────────────────────────────────────────────────────
print("\n=== Test 2: Converge infra ===")
r = api('post', '/converge/target/infra?timeout=60')
check("converge infra ok", r['result'] == 'ok', f"got {r['result']}: {r.get('error')}")

svcs, tgts, probes = graph()

# Infra services green
for name in ['postgres', 'redis']:
    display, runtime = svc_state(svcs, name)
    check(f"{name} running", runtime == 'running', f"got {runtime}")
    check(f"{name} display=green", display == 'green', f"got {display}")

# Infra target green
check("target infra green", tgts['infra']['state'] == 'green', f"got {tgts['infra']['state']}")

# App target still inactive (not converged)
check("target app inactive", tgts['app']['state'] == 'stopped', f"got {tgts['app']['state']}")

# App services still stopped and gray (not active — app target not activated)
for name in ['crash-svc', 'stuck-svc', 'slow-heal', 'fast-heal']:
    display, runtime = svc_state(svcs, name)
    check(f"{name} stopped", runtime == 'stopped', f"got {runtime}")
    check(f"{name} display=stopped (gray)", display == 'stopped', f"got {display}")

check_ws_matches_api("after converge infra")

# ──────────────────────────────────────────────────────────────
print("\n=== Test 3: Converge app ===")
r = api('post', '/converge/target/app?timeout=90')
check("converge app ok", r['result'] == 'ok', f"got {r['result']}: {r.get('error')}")

svcs, tgts, probes = graph()

# All group 1 services running and green
for name in ['postgres', 'redis', 'crash-svc', 'stuck-svc', 'slow-heal', 'fast-heal']:
    display, runtime = svc_state(svcs, name)
    check(f"{name} running", runtime == 'running', f"got {runtime}")
    check(f"{name} display=green", display == 'green', f"got {display}")

# Both group 1 targets green
check("target infra green", tgts['infra']['state'] == 'green', f"got {tgts['infra']['state']}")
check("target app green", tgts['app']['state'] == 'green', f"got {tgts['app']['state']}")

# Group 2 still inactive
check("target flaky-api-target inactive", tgts['flaky-api-target']['state'] == 'stopped',
      f"got {tgts['flaky-api-target']['state']}")
check("target unstable inactive", tgts['unstable']['state'] == 'stopped',
      f"got {tgts['unstable']['state']}")

# Group 2 services still gray
for name in ['flaky-api', 'doomed']:
    display, runtime = svc_state(svcs, name)
    check(f"{name} display=stopped (gray)", display == 'stopped', f"got {display}")

check_ws_matches_api("after converge app")

# ──────────────────────────────────────────────────────────────
print("\n=== Test 4: Stop postgres → active service shows RED ===")
r = api('post', '/stop/service/postgres')
check("stop postgres ok", r['result'] == 'ok', f"got {r['result']}")

svcs, tgts, probes = graph()

# Postgres: stopped runtime, RED display (active — in activated app target)
display, runtime = svc_state(svcs, 'postgres')
check("postgres runtime=stopped", runtime == 'stopped', f"got {runtime}")
check("postgres display=red (active+stopped)", display == 'red', f"got {display}")

# Postgres probes should show stopped
for key in probes:
    if key.startswith('postgres.'):
        check(f"{key} display=stopped", probes[key]['state'] == 'stopped', f"got {probes[key]['state']}")

# Infra target should be red
check("target infra red", tgts['infra']['state'] == 'red', f"got {tgts['infra']['state']}")

# App target should be red (depends on infra)
check("target app red", tgts['app']['state'] == 'red', f"got {tgts['app']['state']}")

# Dependent probes should be red/stale (not green)
for key in ['stuck-svc.dep', 'fast-heal.dep']:
    ps = probe_state(probes, key)
    check(f"{key} not green (dep cascade)", ps != 'green', f"got {ps}")

# Group 2 unaffected (still gray)
for name in ['flaky-api', 'doomed']:
    display, runtime = svc_state(svcs, name)
    check(f"{name} still gray", display == 'stopped', f"got {display}")

check_ws_matches_api("after stop postgres")

# ──────────────────────────────────────────────────────────────
print("\n=== Test 5: Converge app recovers ===")
# fast-heal has restart_on_fail=false and may need time to self-heal.
# First converge might fail if fast-heal probe catches it mid-heal.
# Second converge should succeed (fast-heal self-heals in ~2s).
r = api('post', '/converge/target/app?timeout=90')
if r['result'] == 'failed':
    time.sleep(3)  # wait for fast-heal to self-heal
    r = api('post', '/converge/target/app?timeout=90')
check("converge app ok", r['result'] == 'ok', f"got {r['result']}: {r.get('error')}")

svcs, tgts, probes = graph()
check("postgres back green", svcs['postgres']['state'] == 'green', f"got {svcs['postgres']['state']}")
check("target app green", tgts['app']['state'] == 'green', f"got {tgts['app']['state']}")

# ──────────────────────────────────────────────────────────────
print("\n=== Test 6: Converge unstable → fails (doomed) ===")
r = api('post', '/converge/target/unstable?timeout=30')
# First converge may succeed (doomed alive briefly)
if r['result'] == 'ok':
    print("  (doomed still alive, waiting for it to break...)")
    time.sleep(12)
    api('post', '/reprobe/target/unstable?timeout=10')
    r = api('post', '/converge/target/unstable?timeout=15')

check("converge unstable failed", r['result'] == 'failed', f"got {r['result']}")
check("error mentions restart_on_fail", 'restart_on_fail' in r.get('error', ''), f"got {r.get('error')}")
check("not_green includes doomed", 'doomed' in r.get('not_green', []), f"got {r.get('not_green')}")

svcs, tgts, probes = graph()

# Unstable target should be red (activated, doomed failing)
check("target unstable red", tgts['unstable']['state'] == 'red', f"got {tgts['unstable']['state']}")

# flaky-api-target should be activated (dependency of unstable)
ts = tgts['flaky-api-target']['state']
check("target flaky-api-target activated (not stopped)", ts != 'stopped', f"got {ts}")

# ──────────────────────────────────────────────────────────────
print("\n=== Test 7: Group isolation ===")
svcs, tgts, probes = graph()

# Group 1 should still be green (unaffected by group 2)
check("target app still green", tgts['app']['state'] == 'green', f"got {tgts['app']['state']}")
for name in ['postgres', 'redis', 'crash-svc', 'stuck-svc']:
    check(f"{name} still green", svcs[name]['state'] == 'green', f"got {svcs[name]['state']}")

# ──────────────────────────────────────────────────────────────
print(f"\n{'='*60}")
print(f"Results: {PASS} passed, {FAIL} failed")
sys.exit(1 if FAIL else 0)
