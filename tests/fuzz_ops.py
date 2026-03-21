#!/usr/bin/env python3
"""Randomized operation fuzzer for Gantry.
Tests graph invariants, propagation correctness, and operation contracts."""

import requests
import random
import sys
import time
import os

BASE = os.environ.get('GANTRY_URL', 'http://localhost:9090')
N_ROUNDS = 50

def api(method, path):
    try:
        r = getattr(requests, method)(f'{BASE}/api{path}', timeout=60)
        return r.json()
    except Exception as e:
        return {'error': str(e)}

def graph_state():
    g = api('get', '/graph')
    probes, svcs = {}, {}
    for s in g['services']:
        svcs[s['name']] = s['state']
        for p in s['probes']:
            probes[f"{s['name']}.{p['name']}"] = {
                'state': p['state'], 'depends_on': p.get('depends_on', [])}
    tgts = {t['name']: t['state'] for t in g['targets']}
    return svcs, probes, tgts

def reset():
    for _ in range(3):
        api('post', '/converge/target/full?timeout=60')
        api('post', '/reprobe/all?timeout=60')
        svcs, probes, _ = graph_state()
        if all(v['state'] == 'green' for k, v in probes.items()
               if svcs.get(k.split('.')[0]) != 'stopped'):
            return True
    return False

class Model:
    def __init__(self):
        self.services, self.deps, self.rdeps = {}, {}, {}
        self.meta, self.tgt_probes, self.tgt_deps = set(), {}, {}

    def load(self):
        g = requests.get(f'{BASE}/api/graph').json()
        for s in g['services']:
            n = s['name']
            probes = {}
            for p in s['probes']:
                k = f"{n}.{p['name']}"
                probes[p['name']] = {'depends_on': p.get('depends_on', []), 'key': k}
                self.deps[k] = p.get('depends_on', [])
                for d in p.get('depends_on', []):
                    self.rdeps.setdefault(d, []).append(k)
                if p.get('probe_type') == 'meta':
                    self.meta.add(k)
            self.services[n] = {'probes': probes}
        for t in g['targets']:
            self.tgt_probes[t['name']] = t.get('probes', [])
            self.tgt_deps[t['name']] = t.get('depends_on', [])

    def _trans_rev(self, pk):
        visited, stack = set(), [pk]
        while stack:
            for r in self.rdeps.get(stack.pop(), []):
                if r not in visited:
                    visited.add(r); stack.append(r)
        return visited

    # ── 1. Graph consistency (checked after every operation) ──

    def check_graph(self, probes, tgts, svcs):
        v = []

        # 1a. Green probe → all deps green
        for pk, pi in probes.items():
            if pi['state'] == 'green':
                for d in pi.get('depends_on', []):
                    if probes.get(d, {}).get('state') != 'green':
                        v.append(f"graph.false_green: {pk} green but dep {d}={probes.get(d,{}).get('state')}")

        # 1b. Target state matches probes AND dependent targets
        for tn, ts in tgts.items():
            ps = [probes.get(p, {}).get('state', '?') for p in self.tgt_probes.get(tn, [])]
            svs = [svcs.get(p.split('.')[0]) for p in self.tgt_probes.get(tn, [])]
            dep_tgts = [tgts.get(dt) for dt in self.tgt_deps.get(tn, [])]
            all_g = all(s == 'green' for s in ps) and all(s == 'green' for s in dep_tgts)
            any_bad = (any(s == 'red' for s in ps) or any(s == 'stopped' for s in svs))
            any_stale = any(s == 'stale' for s in ps)
            
            if ts == 'green' and not all_g:
                v.append(f"graph.target_false_green: {tn} green but probes={set(ps)} dep_tgts={dep_tgts}")
            if all_g and ts != 'green':
                v.append(f"graph.target_not_green: {tn}={ts} but all probes+deps green")
            if any_bad and ts != 'red':
                v.append(f"graph.target_not_red: {tn}={ts} but any_bad=True (probes={set(ps)} svcs={set(svs)})")
            if any_stale and not any_bad and ts != 'stale':
                v.append(f"graph.target_not_stale: {tn}={ts} but has stale and not bad")

        # 1c. Meta probe ↔ deps
        for mk in self.meta:
            ms = probes.get(mk, {}).get('state')
            svc = mk.split('.')[0]
            if svcs.get(svc) == 'stopped': continue
            ds = self.deps.get(mk, [])
            ag = all(probes.get(d, {}).get('state') == 'green' for d in ds)
            if ag and ms != 'green':
                v.append(f"graph.meta_not_green: {mk}={ms}")
            if not ag and ms == 'green':
                v.append(f"graph.meta_false_green: {mk}")

        # 1d. Stale with red dep → should be red
        for pk, pi in probes.items():
            if svcs.get(pk.split('.')[0]) == 'stopped': continue
            if pi['state'] == 'stale':
                for d in pi.get('depends_on', []):
                    if probes.get(d, {}).get('state') == 'red' and svcs.get(d.split('.')[0]) != 'stopped':
                        v.append(f"graph.stale_with_red_dep: {pk} stale but {d} red")

        return v

    # ── 2. Propagation correctness (before→after comparison) ──

    def check_propagation(self, before, after, svcs):
        v = []
        for pk, pi in after.items():
            prev, now = before.get(pk, {}).get('state'), pi['state']

            # 2a. Probe went red → green dependents must go red (not stale)
            if prev != 'red' and now == 'red':
                for r in self.rdeps.get(pk, []):
                    if svcs.get(r.split('.')[0]) == 'stopped': continue
                    rp, rn = before.get(r, {}).get('state'), after.get(r, {}).get('state')
                    if rp == 'green' and rn == 'green':
                        v.append(f"propagation.red_not_cascaded: {pk}→red but {r} still green")
                    if rp == 'green' and rn == 'stale':
                        v.append(f"propagation.red_as_stale: {pk}→red but {r} went stale not red")

            # 2b. Probe recovered → red dependents (no other red deps) should go stale
            if prev == 'red' and now == 'green':
                for r in self._trans_rev(pk):
                    if svcs.get(r.split('.')[0]) == 'stopped': continue
                    rp, rn = before.get(r, {}).get('state'), after.get(r, {}).get('state')
                    if rp != 'red': continue
                    other_red = any(
                        after.get(d, {}).get('state') == 'red' and svcs.get(d.split('.')[0]) != 'stopped'
                        for d in self.deps.get(r, []) if d != pk)
                    if not other_red and rn == 'red':
                        v.append(f"propagation.recovery_stuck: {pk} recovered but {r} still red")

        return v

    # ── 3. State stability (no spontaneous changes) ──

    def check_stability(self, op, before, after, svcs):
        v = []
        op_type, op_arg = op

        for pk in before:
            svc = pk.split('.')[0]
            if svcs.get(svc) == 'stopped': continue
            prev, now = before[pk].get('state'), after.get(pk, {}).get('state')
            if prev == now: continue
            deps_changed = any(
                before.get(d, {}).get('state') != after.get(d, {}).get('state')
                for d in self.deps.get(pk, []))
            svc_targeted = op_arg == svc

            # 3a. Green → non-green only if deps changed, service targeted, or probes re-run
            probes_rerun = op_type in ('reprobe_svc', 'reprobe_all', 'converge', 'start', 'restart')
            if prev == 'green' and now != 'green' and not deps_changed and not svc_targeted and not probes_rerun:
                v.append(f"stability.green_lost: {pk} green→{now} (no deps changed, not targeted)")

            # 3b. Red → non-red only if reprobe/start/converge or deps changed
            if prev == 'red' and now != 'red':
                if op_type in ('stop',) and not deps_changed and not svc_targeted:
                    v.append(f"stability.red_flipped: {pk} red→{now} during {op_type}")

        # 3c. Upstream deps unchanged by service operations
        if op_type in ('stop', 'start', 'restart', 'replace', 'reprobe_svc') and op_arg:
            upstream = set()
            for pn in self.services.get(op_arg, {}).get('probes', {}):
                for d in self.deps.get(f"{op_arg}.{pn}", []):
                    if not d.startswith(op_arg + '.'): upstream.add(d)
            for pk in upstream:
                prev = before.get(pk, {}).get('state')
                now = after.get(pk, {}).get('state')
                if prev and now and prev != now:
                    v.append(f"stability.upstream_changed: {op_type} {op_arg} changed {pk} {prev}→{now}")

        return v

    # ── 4. Operation contracts ──

    def check_contract(self, op, result, probes, tgts, svcs):
        v = []
        op_type, op_arg = op

        if op_type == 'converge' and result.get('result') == 'ok':
            if tgts.get(op_arg) != 'green':
                v.append(f"contract.converge_ok_not_green: {op_arg}={tgts.get(op_arg)}")

        if op_type == 'converge' and result.get('result') == 'failed':
            rt = result.get('targets', {}).get(op_arg, {})
            if rt.get('state') == 'green':
                v.append(f"contract.converge_failed_but_green: {op_arg}")

        # Reprobe resolves: after reprobe_svc, probes with all deps green should not be stale
        if op_type == 'reprobe_svc' and result.get('result') == 'ok' and op_arg:
            for pk, pi in probes.items():
                if not pk.startswith(op_arg + '.'): continue
                if svcs.get(op_arg) == 'stopped': continue
                if pi['state'] != 'stale': continue
                all_deps_green = all(
                    probes.get(d, {}).get('state') == 'green'
                    for d in self.deps.get(pk, []))
                if all_deps_green and pk not in self.meta:
                    v.append(f"contract.reprobe_unresolved: {pk} stale after reprobe with all deps green")

        if op_type in ('start', 'restart', 'replace') and result.get('result') == 'ok':
            if svcs.get(op_arg) == 'stopped':
                v.append(f"contract.{op_type}_ok_but_stopped: {op_arg}")

        if op_type == 'stop' and result.get('result') == 'ok':
            if svcs.get(op_arg) != 'stopped':
                v.append(f"contract.stop_ok_but_not_stopped: {op_arg}={svcs.get(op_arg)}")
            for pk in probes:
                if pk.startswith(op_arg + '.'):
                    if probes[pk]['state'] in ('green', 'red'):
                        v.append(f"contract.stopped_probe_active: {pk}={probes[pk]['state']}")
                    for r in self.rdeps.get(pk, []):
                        if r.startswith(op_arg + '.'): continue
                        if probes.get(r, {}).get('state') == 'green':
                            v.append(f"contract.stop_green_dep: {r} green after stopping {op_arg}")

        return v

    # ── Run all checks ──

    def check_all(self, op, result, before, after, tgts, svcs):
        return (self.check_graph(after, tgts, svcs) +
                self.check_propagation(before, after, svcs) +
                self.check_stability(op, before, after, svcs) +
                self.check_contract(op, result, after, tgts, svcs))

# ── Fuzzer ──────────────────────────────────────────────────────────────

def run_op(op):
    t, a = op
    paths = {'stop': f'/stop/service/{a}', 'start': f'/start/service/{a}?timeout=30',
             'restart': f'/restart/service/{a}?timeout=60',
             'replace': f'/replace/service/{a}?timeout=120',
             'converge': f'/converge/target/{a}?timeout=60',
             'reprobe_all': '/reprobe/all?timeout=60',
             'reprobe_svc': f'/reprobe/service/{a}?timeout=30',
             'reprobe_tgt': f'/reprobe/target/{a}?timeout=60',
             'reload': '/reload'}
    return api('post', paths[t])

def op_name(op):
    return f'{op[0]} {op[1]}' if op[1] else op[0]

def random_op(services, targets):
    t = random.choice([
        'stop', 'start', 'restart',
        'converge', 'reprobe_all', 'reprobe_svc', 'reprobe_tgt',
    ])
    if t in ('stop', 'start', 'restart', 'reprobe_svc'): return (t, random.choice(services))
    if t in ('converge', 'reprobe_tgt'): return (t, random.choice(targets))
    return (t, None)

def minimize(model, ops):
    reset()
    fail_idx = None
    for i, op in enumerate(ops):
        _, bp, _ = graph_state()
        result = run_op(op)
        s, p, t = graph_state()
        if model.check_all(op, result, bp, p, t, s):
            fail_idx = i; break
    if fail_idx is None: return ops, []
    prefix = ops[:fail_idx + 1]
    print(f"\n  Minimizing: {len(prefix)} steps → ", end='', flush=True)
    i = 0
    while i < len(prefix) - 1:
        reset()
        candidate = prefix[:i] + prefix[i+1:]
        found = False
        for op in candidate:
            _, bp, _ = graph_state()
            result = run_op(op)
            s, p, t = graph_state()
            if model.check_all(op, result, bp, p, t, s):
                prefix = candidate; found = True
                print(f"{len(prefix)} ", end='', flush=True); break
        if not found: i += 1
    print()
    reset()
    final_v = []
    for op in prefix:
        _, bp, _ = graph_state()
        result = run_op(op)
        s, p, t = graph_state()
        v = model.check_all(op, result, bp, p, t, s)
        if v: final_v = v; break
    return prefix, final_v

def main():
    seed = int(sys.argv[1]) if len(sys.argv) > 1 else int(time.time())
    n_rounds = int(sys.argv[2]) if len(sys.argv) > 2 else N_ROUNDS
    random.seed(seed)
    print(f"Seed: {seed}, rounds: {n_rounds}")

    model = Model()
    model.load()
    services = list(model.services.keys())
    targets = list(model.tgt_probes.keys())
    print(f"Services: {len(services)}, Targets: {len(targets)}, "
          f"Probes: {len(model.deps)}, Meta: {len(model.meta)}")

    print("Warmup: converge full...")
    if not reset():
        print("WARNING: could not reach all-green state")

    all_ops, violations_total = [], []
    for i in range(n_rounds):
        _, before, _ = graph_state()
        op = random_op(services, targets)
        all_ops.append(op)
        result = run_op(op)
        r = result.get('result', result.get('error', '?'))
        svcs, probes, tgts = graph_state()
        violations = model.check_all(op, result, before, probes, tgts, svcs)

        status = 'OK' if not violations else f'FAIL({len(violations)})'
        print(f"  [{i+1:3d}] {op_name(op):25s} -> {r:8s} {status}")
        for v in violations:
            print(f"        {v}")
            violations_total.append((i+1, op_name(op), v))

    print(f"\n{'='*60}")
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
                print(f"  {j+1}. {op_name(op)}{m}")
            print(f"Violation: {min_v[0]}")
        else:
            print("Could not reproduce on replay (flaky)")
    else:
        print(f"PASSED: {n_rounds} rounds, 0 violations (seed={seed})")
    return 1 if violations_total else 0

if __name__ == '__main__':
    sys.exit(main())
