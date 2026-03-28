#!/usr/bin/env python3
"""Randomized operation fuzzer for Gantry.
Tests graph invariants, propagation correctness, and operation contracts."""

import requests
import random
import sys
import time
import os
from dataclasses import dataclass, field

BASE = os.environ.get('GANTRY_URL', 'http://localhost:9090')
N_ROUNDS = 50


def api(method, path):
    try:
        r = getattr(requests, method)(f'{BASE}/api{path}', timeout=90)
        return r.json()
    except Exception as e:
        return {'error': str(e)}


def probe_svc(probe_key):
    """Extract service name from a probe key like 'db.port'."""
    return probe_key.split('.')[0]


@dataclass
class Snapshot:
    """Point-in-time state from /api/graph."""
    services: dict = field(default_factory=dict)   # name → display state
    runtimes: dict = field(default_factory=dict)    # name → runtime state (stopped/running/crashed)
    probes: dict = field(default_factory=dict)      # "svc.probe" → {state, depends_on}
    targets: dict = field(default_factory=dict)     # name → state

    def is_inactive(self, svc_name):
        """Service is stopped or crashed at the runtime level."""
        return self.runtimes.get(svc_name) in ('stopped', 'crashed')

    @staticmethod
    def capture():
        g = api('get', '/graph')
        snap = Snapshot()
        for s in g.get('services', []):
            name = s['name']
            snap.services[name] = s['state']
            snap.runtimes[name] = s.get('runtime', 'unknown')
            for p in s['probes']:
                snap.probes[f"{name}.{p['name']}"] = {
                    'state': p['state'],
                    'reason': p.get('reason', ''),
                    'depends_on': p.get('depends_on', []),
                }
        for t in g.get('targets', []):
            snap.targets[t['name']] = t['state']
        return snap


def reset():
    targets = [t['name'] for t in api('get', '/graph').get('targets', [])]
    if not targets:
        return False
    for _ in range(3):
        # Converge ALL targets so all groups are active
        for tgt in targets:
            api('post', f'/converge/target/{tgt}?timeout=90')
        api('post', '/reprobe/all?timeout=60')
        snap = Snapshot.capture()
        if all(v['state'] == 'green' for k, v in snap.probes.items()
               if not snap.is_inactive(probe_svc(k))):
            return True
    return False


# ── Model: static topology loaded once at start ─────────────────────────

class Model:
    def __init__(self):
        self.services = {}       # name → {probes, restart_on_fail}
        self.deps = {}           # "svc.probe" → ["svc.probe", ...]
        self.rdeps = {}          # "svc.probe" → ["svc.probe", ...] (reverse)
        self.meta = set()        # set of meta probe keys
        self.tgt_probes = {}     # target → [probe keys]
        self.tgt_deps = {}       # target → [dep target names]

    @classmethod
    def load_from_api(cls):
        m = cls()
        g = requests.get(f'{BASE}/api/graph').json()
        for s in g['services']:
            name = s['name']
            probes = {}
            for p in s['probes']:
                key = f"{name}.{p['name']}"
                probes[p['name']] = {'depends_on': p.get('depends_on', []), 'key': key}
                m.deps[key] = p.get('depends_on', [])
                for d in p.get('depends_on', []):
                    m.rdeps.setdefault(d, []).append(key)
                if p.get('probe_type') == 'meta':
                    m.meta.add(key)
            m.services[name] = {
                'probes': probes,
                'restart_on_fail': s.get('restart_on_fail', True),
            }
        for t in g['targets']:
            m.tgt_probes[t['name']] = t.get('probes', [])
            m.tgt_deps[t['name']] = t.get('depends_on', [])
        return m

    def trans_rev_deps(self, probe_key):
        """All transitive reverse dependents of a probe."""
        visited, stack = set(), [probe_key]
        while stack:
            for r in self.rdeps.get(stack.pop(), []):
                if r not in visited:
                    visited.add(r)
                    stack.append(r)
        return visited

    def target_services(self, tgt_name):
        """All services transitively needed by a target (BFS through target deps)."""
        svcs, visited, queue = set(), set(), [tgt_name]
        while queue:
            t = queue.pop(0)
            if t in visited:
                continue
            visited.add(t)
            for p in self.tgt_probes.get(t, []):
                svcs.add(probe_svc(p))
            queue.extend(self.tgt_deps.get(t, []))
        return svcs

    def op_scope(self, op_type, op_arg):
        """Set of service names affected by an operation."""
        if op_type in ('stop', 'start', 'restart', 'reprobe_svc'):
            return {op_arg} if op_arg else set()
        if op_type in ('converge', 'reprobe_tgt'):
            return self.target_services(op_arg) if op_arg else set()
        if op_type == 'reprobe_all':
            return set(self.services.keys())
        return set()

    # ════════════════════════════════════════════════════════════════════
    # 1. PROBE INVARIANTS — rules about individual probe states
    # ════════════════════════════════════════════════════════════════════

    def check_probes(self, snap):
        v = []
        for pk, pi in snap.probes.items():
            svc = probe_svc(pk)
            rt = snap.runtimes.get(svc)
            ps = pi['state']

            # 1a. Green probe → all deps must be green
            if ps == 'green':
                for d in pi['depends_on']:
                    ds = snap.probes.get(d, {}).get('state')
                    if ds != 'green':
                        v.append(f"probe.false_green: {pk} green but dep {d}={ds}")

            # 1b. Stopped/crashed service → probes must be stopped or red
            if rt in ('stopped', 'crashed') and ps in ('green', 'pending', 'probing'):
                v.append(f"probe.active_on_inactive_svc: {pk}={ps} but runtime={rt}")

            # 1c. Pending/probing probe with a red dep → should be red (on running services)
            if rt == 'running' and ps in ('pending', 'probing'):
                for d in pi['depends_on']:
                    if snap.is_inactive(probe_svc(d)):
                        continue
                    if snap.probes.get(d, {}).get('state') == 'red':
                        v.append(f"probe.pending_with_red_dep: {pk}={ps} but {d} red")

        # 1d. Meta probe consistency (only on running services)
        for mk in self.meta:
            if snap.is_inactive(probe_svc(mk)):
                continue
            ms = snap.probes.get(mk, {}).get('state')
            deps = self.deps.get(mk, [])
            all_deps_green = all(snap.probes.get(d, {}).get('state') == 'green' for d in deps)
            if all_deps_green and ms != 'green':
                v.append(f"probe.meta_not_green: {mk}={ms}")
            if not all_deps_green and ms == 'green':
                v.append(f"probe.meta_false_green: {mk}")

        return v

    # ════════════════════════════════════════════════════════════════════
    # 2. SERVICE INVARIANTS — rules about service display states
    # ════════════════════════════════════════════════════════════════════

    def check_services(self, snap):
        v = []
        for sname in snap.services:
            ss = snap.services[sname]
            rt = snap.runtimes.get(sname)

            # 2a. Green service → all its probes must be green
            if ss == 'green':
                for pk, pi in snap.probes.items():
                    if pk.startswith(sname + '.') and pi['state'] != 'green':
                        v.append(f"svc.false_green: {sname} green but {pk}={pi['state']}")

            # 2b. Crashed runtime → display must be red
            if rt == 'crashed' and ss != 'red':
                v.append(f"svc.crashed_not_red: {sname} runtime=crashed display={ss}")

            # 2c. Stopped display → runtime must be stopped (not running or crashed)
            if ss == 'stopped' and rt not in ('stopped',):
                v.append(f"svc.stopped_display_wrong_runtime: {sname} display=stopped runtime={rt}")

            # 2d. Red + running → must have a non-green probe
            if ss == 'red' and rt == 'running':
                all_probes_green = all(
                    pi['state'] == 'green'
                    for pk, pi in snap.probes.items()
                    if pk.startswith(sname + '.'))
                if all_probes_green:
                    v.append(f"svc.red_all_probes_green: {sname} red+running but all probes green")

        return v

    # ════════════════════════════════════════════════════════════════════
    # 3. TARGET INVARIANTS — rules about target states
    # ════════════════════════════════════════════════════════════════════

    def check_targets(self, snap):
        v = []
        for tn, ts in snap.targets.items():
            if ts in ('stopped', 'inactive'):
                continue

            probe_states = [snap.probes.get(p, {}).get('state', '?')
                            for p in self.tgt_probes.get(tn, [])]
            svc_runtimes = [snap.runtimes.get(probe_svc(p))
                            for p in self.tgt_probes.get(tn, [])]
            dep_tgt_states = [snap.targets.get(dt) for dt in self.tgt_deps.get(tn, [])]

            all_green = (all(s == 'green' for s in probe_states)
                         and all(s == 'green' for s in dep_tgt_states))
            any_bad = (any(s == 'red' for s in probe_states)
                       or any(rt in ('stopped', 'crashed') for rt in svc_runtimes))

            # 3a. Green target → all probes + dep targets green
            if ts == 'green' and not all_green:
                v.append(f"target.false_green: {tn} green but probes={set(probe_states)}")

            # 3b. All probes + dep targets green → target must be green
            if all_green and ts != 'green':
                v.append(f"target.not_green: {tn}={ts} but all probes+deps green")

            # 3c. Any red probe or stopped/crashed service → target must be red
            if any_bad and ts != 'red':
                v.append(f"target.not_red: {tn}={ts} (probes={set(probe_states)} runtimes={set(svc_runtimes)})")

            # 3d. Pending probes (no red) → target must be red
            any_pending = any(s in ('pending', 'probing') for s in probe_states)
            if any_pending and not any_bad and ts != 'red':
                v.append(f"target.not_red_pending: {tn}={ts} but has pending probes")

        return v

    # ════════════════════════════════════════════════════════════════════
    # 4. PROPAGATION — before/after state transition rules
    # ════════════════════════════════════════════════════════════════════

    def check_propagation(self, before, after):
        v = []
        for pk, pi in after.probes.items():
            prev = before.probes.get(pk, {}).get('state')
            now = pi['state']

            # 4a. Probe went red → green dependents on running services must go red
            if prev != 'red' and now == 'red':
                for r in self.rdeps.get(pk, []):
                    if after.is_inactive(probe_svc(r)):
                        continue
                    rp = before.probes.get(r, {}).get('state')
                    rn = after.probes.get(r, {}).get('state')
                    if rp == 'green' and rn == 'green':
                        v.append(f"propagation.red_not_cascaded: {pk}→red but {r} still green")
                    if rp == 'green' and rn in ('pending', 'probing'):
                        v.append(f"propagation.red_went_pending: {pk}→red but {r} went {rn} not red")

            # 4b. Probe recovered → red dependents (no other red deps) should go pending
            # Exception: ProbeFailed dependents stay red — own failure, not dep-related
            if prev == 'red' and now == 'green':
                for r in self.trans_rev_deps(pk):
                    if after.is_inactive(probe_svc(r)):
                        continue
                    rp = before.probes.get(r, {}).get('state')
                    rn = after.probes.get(r, {}).get('state')
                    if rp != 'red':
                        continue
                    # ProbeFailed stays red — recovery doesn't fix own failures
                    r_reason = after.probes.get(r, {}).get('reason', '')
                    if 'probe failed' in r_reason:
                        continue
                    other_red = any(
                        after.probes.get(d, {}).get('state') == 'red'
                        and not after.is_inactive(probe_svc(d))
                        for d in self.deps.get(r, []) if d != pk)
                    if not other_red and rn == 'red':
                        v.append(f"propagation.recovery_stuck: {pk} recovered but {r} still red")

            # 4c. ProbeFailed must not be overwritten by recovery propagation
            # If a probe ran and failed, it must stay red — dep recovery can't fix it
            if prev == 'red' and now in ('pending', 'probing'):
                prev_reason = before.probes.get(pk, {}).get('reason', '')
                if 'probe failed' in prev_reason:
                    v.append(f"propagation.failed_overwritten: {pk} was red(probe failed) but went {now}")

            # 4d. Probe stopped → cross-service running dependents must not stay green
            if prev != 'stopped' and now == 'stopped':
                svc = probe_svc(pk)
                for r in self.trans_rev_deps(pk):
                    rsvc = probe_svc(r)
                    if rsvc == svc:
                        continue
                    if after.is_inactive(rsvc):
                        continue
                    rn = after.probes.get(r, {}).get('state')
                    if rn == 'green':
                        v.append(f"propagation.green_after_dep_stop: {pk}→stopped but {r} still green")

        return v

    # ════════════════════════════════════════════════════════════════════
    # 5. STABILITY — no spontaneous state changes outside operation scope
    # ════════════════════════════════════════════════════════════════════

    def check_stability(self, op, before, after):
        v = []
        op_type, op_arg = op
        scope = self.op_scope(op_type, op_arg)

        for pk in before.probes:
            svc = probe_svc(pk)
            if after.is_inactive(svc):
                continue
            prev = before.probes[pk].get('state')
            now = after.probes.get(pk, {}).get('state')
            if prev == now:
                continue

            deps_changed = any(
                before.probes.get(d, {}).get('state') != after.probes.get(d, {}).get('state')
                for d in self.deps.get(pk, []))
            svc_in_scope = svc in scope
            probes_rerun = op_type in ('reprobe_svc', 'reprobe_all', 'reprobe_tgt',
                                       'converge', 'start', 'restart')

            # 5a. Green → non-green only if deps changed, in scope, or probes re-run
            if prev == 'green' and now != 'green':
                if not deps_changed and not svc_in_scope and not probes_rerun:
                    v.append(f"stability.green_lost: {pk} green→{now} (not in scope, no deps changed)")

            # 5b. Red → non-red only if reprobe/start/converge or deps changed
            if prev == 'red' and now != 'red':
                if op_type == 'stop' and not deps_changed and not svc_in_scope:
                    v.append(f"stability.red_flipped: {pk} red→{now} during {op_type}")

        # 5c. Upstream deps unchanged by stop/start/restart
        if op_type in ('stop', 'start', 'restart') and op_arg:
            upstream = set()
            for pn in self.services.get(op_arg, {}).get('probes', {}):
                for d in self.deps.get(f"{op_arg}.{pn}", []):
                    if not d.startswith(op_arg + '.'):
                        upstream.add(d)
            for pk in upstream:
                prev = before.probes.get(pk, {}).get('state')
                now = after.probes.get(pk, {}).get('state')
                if prev and now and prev != now:
                    v.append(f"stability.upstream_changed: {op_type} {op_arg} changed {pk} {prev}→{now}")

        return v

    # ════════════════════════════════════════════════════════════════════
    # 6. OPERATION CONTRACTS — post-conditions for each operation type
    # ════════════════════════════════════════════════════════════════════

    def check_contract(self, op, result, before, snap):
        v = []
        op_type, op_arg = op

        # ── Converge ──
        if op_type == 'converge' and result.get('result') == 'ok':
            if snap.targets.get(op_arg) != 'green':
                v.append(f"contract.converge_ok_not_green: {op_arg}={snap.targets.get(op_arg)}")
            for pk in self.tgt_probes.get(op_arg, []):
                ps = snap.probes.get(pk, {}).get('state')
                if ps != 'green':
                    v.append(f"contract.converge_ok_probe_not_green: {pk}={ps}")
            for svc_name in result.get('actions', {}).get('restarted', []):
                if snap.services.get(svc_name) != 'green':
                    v.append(f"contract.restarted_not_green: {svc_name}={snap.services.get(svc_name)}")
            for svc_name in self.target_services(op_arg):
                if snap.runtimes.get(svc_name) == 'stopped':
                    v.append(f"contract.converge_ok_svc_stopped: {svc_name} still stopped")

            # Stale probe data: red(ProbeFailed)→green with probe_ms=0 and no restart
            # is suspicious. But red(DepRed)→green with probe_ms=0 is fine — the
            # service was running, log output is still valid, dep just recovered.
            started = set(result.get('actions', {}).get('started', []))
            restarted = set(result.get('actions', {}).get('restarted', []))
            refreshed = started | restarted
            for pk, pi in snap.probes.items():
                svc = probe_svc(pk)
                if svc in refreshed or snap.is_inactive(svc) or pk in self.meta:
                    continue
                prev_state = before.probes.get(pk, {}).get('state')
                if prev_state != 'red' or pi['state'] != 'green':
                    continue
                # Only flag if the previous red was ProbeFailed (actual failure),
                # not DepRed (dep went red, service was still running)
                # Skip probes that were red due to deps or service state (not own failure)
                prev_reason = before.probes.get(pk, {}).get('reason', '')
                if any(x in prev_reason for x in ('dep red', 'dep recovered', 'stopped', 'container died')):
                    continue
                probe_info = result.get('probes', {}).get(pk, {})
                # probe_ms absent means probe wasn't run (propagation only)
                if 'probe_ms' not in probe_info:
                    v.append(f"contract.stale_probe_data: {pk} red→green without probe_ms (no restart)")

        if op_type == 'converge' and result.get('result') == 'failed':
            if result.get('targets', {}).get(op_arg, {}).get('state') == 'green':
                v.append(f"contract.converge_failed_but_green: {op_arg}")

        # restart_on_fail=false must NOT be restarted
        if op_type == 'converge':
            for svc_name in result.get('actions', {}).get('restarted', []):
                if not self.services.get(svc_name, {}).get('restart_on_fail', True):
                    v.append(f"contract.restart_on_fail_violated: {svc_name}")

        # ── Start / Restart ──
        if op_type in ('start', 'restart') and result.get('result') == 'ok':
            if snap.runtimes.get(op_arg) != 'running':
                v.append(f"contract.{op_type}_ok_not_running: {op_arg} runtime={snap.runtimes.get(op_arg)}")

        # ── Stop ──
        if op_type == 'stop' and result.get('result') == 'ok':
            if snap.runtimes.get(op_arg) != 'stopped':
                v.append(f"contract.stop_ok_not_stopped: {op_arg} runtime={snap.runtimes.get(op_arg)}")
            for pk in snap.probes:
                if not pk.startswith(op_arg + '.'):
                    continue
                if snap.probes[pk]['state'] in ('green', 'pending', 'probing'):
                    v.append(f"contract.stopped_probe_active: {pk}={snap.probes[pk]['state']}")
                for r in self.rdeps.get(pk, []):
                    if r.startswith(op_arg + '.'):
                        continue
                    if snap.probes.get(r, {}).get('state') == 'green':
                        v.append(f"contract.stop_green_dep: {r} green after stopping {op_arg}")

        # ── Reprobe ──
        # After reprobe, all probes in scope should have been re-checked.
        # A probe with all green deps should be green (or pending if log probe
        # matched failure pattern — that's a real failure, not a reprobe issue).
        if op_type in ('reprobe_svc', 'reprobe_tgt', 'reprobe_all') and result.get('result') == 'ok':
            scope = self.op_scope(op_type, op_arg)
            for pk, pi in snap.probes.items():
                svc = probe_svc(pk)
                if svc not in scope or snap.is_inactive(svc) or pk in self.meta:
                    continue
                if pi['state'] == 'green':
                    continue
                all_deps_green = all(
                    snap.probes.get(d, {}).get('state') == 'green'
                    for d in self.deps.get(pk, []))
                if all_deps_green and pi['state'] not in ('red',):
                    # Non-red, non-green probe with all deps green after reprobe = unresolved
                    v.append(f"contract.reprobe_unresolved: {pk}={pi['state']} after {op_type} with all deps green")

        return v

    # ════════════════════════════════════════════════════════════════════
    # 7. WS/UI CONSISTENCY — WebSocket snapshot must match API
    # ════════════════════════════════════════════════════════════════════

    def check_ws(self, snap):
        v = []
        # WS snapshot is eventually consistent — retry with fresh API+WS snapshots
        ws = None
        for _ in range(3):
            time.sleep(0.3)
            snap = Snapshot()  # re-fetch API to avoid stale comparison
            ws = ws_snapshot()
            if ws:
                # Quick check: any obvious mismatch?
                mismatch = False
                for sname in snap.services:
                    ws_st = ws.get('services', {}).get(sname, {}).get('state')
                    if ws_st and ws_st != snap.services[sname]:
                        mismatch = True
                        break
                if not mismatch:
                    break
        if not ws:
            return v

        # 7a. Service display state: WS must match API
        for sname in snap.services:
            ws_state = ws.get('services', {}).get(sname, {}).get('state')
            if ws_state and ws_state != snap.services[sname]:
                v.append(f"ws.svc_mismatch: {sname} api={snap.services[sname]} ws={ws_state}")

        # 7b. Probe display state: WS must match API
        for pk, pi in snap.probes.items():
            svc, probe = pk.split('.', 1)
            ws_ps = ws.get('services', {}).get(svc, {}).get('probes', {}).get(probe, {}).get('state')
            if ws_ps and ws_ps != pi['state']:
                v.append(f"ws.probe_mismatch: {pk} api={pi['state']} ws={ws_ps}")

        # 7c. Target display state: WS must match API
        for tname in snap.targets:
            ws_ts = ws.get('targets', {}).get(tname, {}).get('state')
            if ws_ts and ws_ts != snap.targets[tname]:
                v.append(f"ws.target_mismatch: {tname} api={snap.targets[tname]} ws={ws_ts}")

        # 7d. Stopped service → no green probes in WS (crashed/red can have mixed probes)
        for sname in snap.services:
            if snap.services[sname] != 'stopped':
                continue
            ws_probes = ws.get('services', {}).get(sname, {}).get('probes', {})
            for pname, wp in ws_probes.items():
                if wp.get('state') in ('green', 'pending', 'probing'):
                    v.append(f"ws.active_probe_on_stopped_svc: {sname}.{pname} ws={wp.get('state')} but svc=stopped")

        return v

    # ════════════════════════════════════════════════════════════════════

    def check_all(self, op, result, before, after):
        return (self.check_probes(after) +
                self.check_services(after) +
                self.check_targets(after) +
                self.check_propagation(before, after) +
                self.check_stability(op, before, after) +
                self.check_contract(op, result, before, after) +
                self.check_ws(after))


def ws_snapshot():
    """Get WS snapshot by connecting, reading the first message, and disconnecting."""
    try:
        import websocket
        ws_url = BASE.replace('http', 'ws') + '/api/ws'
        ws = websocket.create_connection(ws_url, timeout=5)
        data = ws.recv()
        ws.close()
        import json
        return json.loads(data)
    except Exception:
        return None


# ── Fuzzer ──────────────────────────────────────────────────────────────

OP_PATHS = {
    'stop':        lambda a: f'/stop/service/{a}',
    'start':       lambda a: f'/start/service/{a}?timeout=30',
    'restart':     lambda a: f'/restart/service/{a}?timeout=60',
    'converge':    lambda a: f'/converge/target/{a}?timeout=90',
    'reprobe_all': lambda _: '/reprobe/all?timeout=60',
    'reprobe_svc': lambda a: f'/reprobe/service/{a}?timeout=30',
    'reprobe_tgt': lambda a: f'/reprobe/target/{a}?timeout=60',
}

OP_WEIGHTS = {
    'stop': 2, 'start': 2, 'restart': 1,
    'converge': 3,  # highest — tests the most paths
    'reprobe_all': 1, 'reprobe_svc': 1, 'reprobe_tgt': 1,
}


def run_op(op):
    op_type, op_arg = op
    return api('post', OP_PATHS[op_type](op_arg))


def op_name(op):
    return f'{op[0]} {op[1]}' if op[1] else op[0]


def random_op(services, targets):
    types = list(OP_WEIGHTS.keys())
    weights = list(OP_WEIGHTS.values())
    t = random.choices(types, weights=weights)[0]
    if t in ('stop', 'start', 'restart', 'reprobe_svc'):
        return (t, random.choice(services))
    if t in ('converge', 'reprobe_tgt'):
        return (t, random.choice(targets))
    return (t, None)


def minimize(model, ops):
    """Delta-debugging minimizer: finds shortest prefix that reproduces."""
    reset()
    fail_idx = None
    for i, op in enumerate(ops):
        before = Snapshot.capture()
        result = run_op(op)
        after = Snapshot.capture()
        if model.check_all(op, result, before, after):
            fail_idx = i
            break
    if fail_idx is None:
        return ops, []

    prefix = ops[:fail_idx + 1]
    print(f"\n  Minimizing: {len(prefix)} steps → ", end='', flush=True)
    i = 0
    while i < len(prefix) - 1:
        reset()
        candidate = prefix[:i] + prefix[i + 1:]
        found = False
        for op in candidate:
            before = Snapshot.capture()
            result = run_op(op)
            after = Snapshot.capture()
            if model.check_all(op, result, before, after):
                prefix = candidate
                found = True
                print(f"{len(prefix)} ", end='', flush=True)
                break
        if not found:
            i += 1
    print()

    # Final replay to get the actual violation
    reset()
    final_v = []
    for op in prefix:
        before = Snapshot.capture()
        result = run_op(op)
        after = Snapshot.capture()
        v = model.check_all(op, result, before, after)
        if v:
            final_v = v
            break
    return prefix, final_v


def main():
    seed = int(sys.argv[1]) if len(sys.argv) > 1 else int(time.time())
    n_rounds = int(sys.argv[2]) if len(sys.argv) > 2 else N_ROUNDS
    random.seed(seed)
    print(f"Seed: {seed}, rounds: {n_rounds}")

    model = Model.load_from_api()
    services = list(model.services.keys())
    targets = list(model.tgt_probes.keys())
    print(f"Services: {len(services)}, Targets: {len(targets)}, "
          f"Probes: {len(model.deps)}, Meta: {len(model.meta)}")

    print("Warmup: converge...")
    if not reset():
        print("WARNING: could not reach all-green state")

    all_ops, violations_total = [], []
    for i in range(n_rounds):
        before = Snapshot.capture()
        op = random_op(services, targets)
        all_ops.append(op)
        result = run_op(op)
        r = result.get('result', result.get('error', '?'))
        after = Snapshot.capture()
        violations = model.check_all(op, result, before, after)

        status = 'OK' if not violations else f'FAIL({len(violations)})'
        print(f"  [{i + 1:3d}] {op_name(op):25s} -> {r:8s} {status}")
        for v in violations:
            print(f"        {v}")
            violations_total.append((i + 1, op_name(op), v))

    print(f"\n{'=' * 60}")
    if violations_total:
        print(f"FAILED: {len(violations_total)} violations in {n_rounds} rounds (seed={seed})")
        for step, opn, v in violations_total[:10]:
            print(f"  step {step} ({opn}): {v}")
        fail_step = violations_total[0][0]
        print(f"\nMinimizing first failure (step {fail_step})...")
        min_ops, min_v = minimize(model, all_ops[:fail_step])
        if min_v:
            print(f"\nMinimal reproduction ({len(min_ops)} steps):")
            for j, op in enumerate(min_ops):
                m = " <-- FAILS" if j == len(min_ops) - 1 else ""
                print(f"  {j + 1}. {op_name(op)}{m}")
            print(f"Violation: {min_v[0]}")
        else:
            print("Could not reproduce on replay (flaky)")
    else:
        print(f"PASSED: {n_rounds} rounds, 0 violations (seed={seed})")
    return 1 if violations_total else 0


if __name__ == '__main__':
    sys.exit(main())
