"""Recovery behavior tests for converge algorithm.

Uses demo fixture (8 services, 2 isolated groups):
  Group 1 (targets: infra, app):
    postgres, redis       — infrastructure
    crash-svc             — exits when redis dies (restart_on_fail: true)
    stuck-svc             — broken state when postgres dies (restart_on_fail: true)
    slow-heal             — reconnects in ~20s (restart_on_fail: true)
    fast-heal             — reconnects in ~2s (restart_on_fail: false)
  Group 2 (targets: flaky-api-target, unstable):
    flaky-api             — crashes randomly (restart_on_fail: true)
    doomed                — stops responding (restart_on_fail: false)
"""

import os
import subprocess
import time

import pytest

slow = pytest.mark.slow


def test_fresh_converge(demo, green):
    """All stopped → converge → all green, no restarts."""
    svcs, _, _, _ = demo.graph_full()
    for svc in svcs:
        if svcs[svc] != 'stopped':
            demo.stop(svc)
    demo.wait_for(lambda: all(v == 'stopped' for v in demo.graph_full()[0].values()))

    d = demo.converge()
    assert d['result'] == 'ok'
    assert len(d['actions'].get('restarted', [])) == 0
    assert len(d['actions'].get('started', [])) == 6  # group 1 only (app target)
    assert d['duration_ms'] < 10000  # ~5.4s typical (slow-heal 5s init is bottleneck)

    svcs, _, tgts, _ = demo.graph_full()
    # Group 1 services green, group 2 still stopped
    for name in ['postgres', 'redis', 'crash-svc', 'stuck-svc', 'slow-heal', 'fast-heal']:
        assert svcs[name] == 'green', f"{name}={svcs[name]}"
    assert tgts['infra'] == 'green'
    assert tgts['app'] == 'green'


@slow
def test_crash_recovery(demo, green):
    """crash-svc exits when redis dies → converge recovers."""
    demo.stop('redis')
    assert demo.wait_for(
        lambda: demo.graph_full()[1].get('crash-svc') in ('stopped', 'crashed'),
        timeout=15,
    ), f"crash-svc didn't die: {demo.graph_full()[1].get('crash-svc')}"

    d = demo.converge()
    assert d['result'] == 'ok', \
        f"converge failed: {d.get('error')}, not_green={d.get('not_green')}"
    svcs, _, _, _ = demo.graph_full()
    assert svcs.get('crash-svc') == 'green'


@slow
def test_stuck_recovery(demo, green):
    """stuck-svc running but broken → restarted by converge."""
    demo.stop('postgres')
    time.sleep(8)  # wait for monitoring loop to detect and enter broken state

    _, runtimes, _, _ = demo.graph_full()
    assert runtimes.get('stuck-svc') == 'running'

    d = demo.converge()
    assert d['result'] == 'ok'
    assert 'stuck-svc' in d['actions'].get('restarted', [])
    assert d['duration_ms'] < 5000  # ~1.9s typical (start postgres + restart stuck-svc)


@slow
def test_slow_heal_recovery(demo, green):
    """slow-heal reconnects in 20s, restart is faster."""
    demo.stop('redis')
    demo.wait_probe_state('slow-heal.http', 'red', timeout=15)
    time.sleep(8)

    d = demo.converge()
    assert d['result'] == 'ok'
    assert 'slow-heal' in d['actions'].get('restarted', [])
    assert d['duration_ms'] < 12000  # ~5.6s typical (start redis + restart slow-heal with 5s init)


def test_log_probe_recovers_after_dep_stop(demo, green):
    """Log probe on a still-running service can match existing logs after dep recovery.

    stop redis → slow-heal.dep goes Red(DepRed) → converge starts redis →
    slow-heal.dep reprobed → matches existing 'dependency connected' → green.
    No restart needed because the service was never stopped, log output is still valid.
    """
    demo.stop('redis')
    _, _, _, probes = demo.graph_full()
    assert probes.get('slow-heal.dep') == 'red', \
        f"slow-heal.dep should be red after redis stop, got {probes.get('slow-heal.dep')}"

    d = demo.converge()
    assert d['result'] == 'ok'

    # slow-heal should NOT need restart — its log probe matches existing output
    # because log_since is only advanced on service restart, not dep propagation
    _, svcs_state, _, probes = demo.graph_full()
    assert svcs_state.get('slow-heal') == 'running'
    assert probes.get('slow-heal.dep') == 'green'


@slow
def test_fast_heal_recovery(demo, green):
    """fast-heal reconnects in ~2s, no restart needed."""
    demo.stop('postgres')
    demo.wait_probe_state('fast-heal.http', 'red', timeout=15)

    d = demo.converge()
    assert d['result'] == 'ok'
    assert 'fast-heal' not in d['actions'].get('restarted', [])

    svcs, _, _, _ = demo.graph_full()
    assert svcs.get('fast-heal') == 'green'


@slow
def test_both_infra_down(demo, green):
    """All 4 behaviors at once."""
    demo.stop('postgres')
    demo.stop('redis')
    demo.wait_svc_state('crash-svc', 'stopped', timeout=15)
    demo.wait_probe_state('stuck-svc.http', 'red', timeout=15)
    time.sleep(8)

    d = demo.converge(timeout=120)
    assert d['result'] == 'ok'
    assert 'fast-heal' not in d['actions'].get('restarted', [])

    svcs, _, tgts, _ = demo.graph_full()
    for name in ['postgres', 'redis', 'crash-svc', 'stuck-svc', 'slow-heal', 'fast-heal']:
        assert svcs[name] == 'green', f"{name}={svcs[name]}"
    assert tgts['app'] == 'green'


@slow
def test_skip_restart(demo, green):
    """skip_restart=true: diagnose only."""
    demo.stop('redis')
    demo.wait_probe_state('slow-heal.http', 'red', timeout=15)
    time.sleep(8)

    d = demo.converge(skip_restart=True)
    assert d['result'] == 'failed'
    assert len(d['actions'].get('restarted', [])) == 0
    assert d['duration_ms'] < 5000  # ~0.3s typical (terminal failure detected immediately)


@slow
def test_reprobe_self_healing(demo, green):
    """Reprobe: fast-heal self-heals, stuck-svc stays broken."""
    demo.stop('postgres')
    # Wait for propagation + actual container detection.
    # Propagation makes probes Red instantly, but stuck-svc's monitoring
    # loop (every 2s) must also detect and log "dependency check failed"
    # for the reprobe log scan to find it.
    demo.wait_probe_state('fast-heal.http', 'red', timeout=15)
    time.sleep(10)

    demo.start('postgres', timeout=30)
    demo.wait_probe_state('postgres.ready', 'green', timeout=30)
    time.sleep(3)

    d = demo.reprobe_all()
    assert d['result'] == 'ok'

    svcs, _, _, _ = demo.graph_full()
    assert svcs.get('fast-heal') == 'green'
    assert svcs.get('stuck-svc') != 'green'


@slow
def test_docker_watcher(demo, green):
    """External docker stop/start detected automatically."""

    subprocess.run(['docker', 'stop', 'demo-redis'], capture_output=True, timeout=15)
    # External docker stop → watcher detects die event → Crashed (not Stopped)
    assert demo.wait_svc_state('redis', 'crashed', timeout=10)

    subprocess.run(['docker', 'start', 'demo-redis'], capture_output=True, timeout=15)
    assert demo.wait_svc_state('redis', 'running', timeout=10)


def test_op_lock(demo, green):
    """Op lock released after converge — next op works."""
    demo.converge(timeout=5)
    time.sleep(1)
    g = demo.graph()
    assert 'services' in g
    d = demo.reprobe_all(timeout=5)
    assert d.get('result') is not None


def test_probe_results_have_logs(demo, green):
    """Log probes include matched lines in response."""
    svcs, _, _, _ = demo.graph_full()
    for svc in svcs:
        if svcs[svc] != 'stopped':
            demo.stop(svc)
    demo.wait_for(lambda: all(v == 'stopped' for v in demo.graph_full()[0].values()))

    d = demo.converge()
    assert d['result'] == 'ok'
    log_probes = [k for k, v in d.get('probes', {}).items() if v.get('logs')]
    assert len(log_probes) > 0, "No log probes with matched lines"
    tcp_probes = [k for k, v in d.get('probes', {}).items() if v.get('probe_ms') is not None]
    assert len(tcp_probes) > 0, "No probes with timing"


@slow
def test_probe_propagation(demo, green):
    """Stop redis → probes that depend on redis go red immediately."""
    demo.stop('redis')

    # Dependency chain:
    #   redis.ready → crash-svc.dep → crash-svc.http
    #   redis.ready → slow-heal.dep → slow-heal.http
    # All should be red now, without waiting for probes to time out.
    _, _, _, probes = demo.graph_full()
    assert probes['crash-svc.dep'] == 'red'
    assert probes['crash-svc.http'] == 'red'
    assert probes['slow-heal.dep'] == 'red'
    assert probes['slow-heal.http'] == 'red'


@slow
def test_probe_propagation_external_kill(demo, green):
    """Docker kill redis externally → same propagation via watcher."""
    subprocess.run(['docker', 'kill', 'demo-redis'], capture_output=True, timeout=5)

    # Watcher detects the kill and propagates within seconds (not probe timeout).
    assert demo.wait_probe_state('crash-svc.dep', 'red', timeout=5), \
        f"crash-svc.dep not red: {demo.graph_full()[3]}"

    _, _, _, probes = demo.graph_full()
    assert probes['slow-heal.dep'] in ('red', 'stopped')


@slow
def test_converge_reports_start_failure(demo, green):
    """Container removed → converge reports which service failed and why."""

    # Stop all, then remove redis container so docker can't start it
    for svc in ('crash-svc', 'slow-heal', 'redis'):
        demo.stop(svc)
    demo.wait_svc_state('redis', 'stopped', timeout=10)
    subprocess.run(
        ['docker', 'rm', '-f', 'demo-redis'], capture_output=True, timeout=10)

    try:
        d = demo.converge(timeout=30)
        assert d['result'] == 'failed'

        # Should finish fast (not burn the full 30s timeout)
        assert d['duration_ms'] < 3000, \
            f"converge took {d['duration_ms']}ms — should bail early on start failure"

        # Error should name the failing service
        assert 'redis' in d.get('error', ''), \
            f"error doesn't mention redis: {d.get('error')}"
        assert 'start_errors' in d.get('actions', {}), \
            f"no start_errors in actions: {d.get('actions')}"
        assert 'redis' in d['actions']['start_errors']
    finally:
        # Recreate the container so other tests still work
        subprocess.run(
            ['docker', 'compose', 'up', '--no-start', 'redis'],
            capture_output=True, timeout=30,
            cwd=os.path.join(os.path.dirname(__file__), 'fixtures', 'demo'))


def test_ws_probe_consistency_after_stop(demo, green):
    """WS snapshot probe states must match API after stop.

    Regression: suppressed probe events on stop caused WS snapshot to show
    stale green probes on stopped services, leading to green edges in UI.
    """

    # All probes should be green in both API and WS
    _, _, _, probes = demo.graph_full()
    for pk, ps in probes.items():
        svc = pk.split('.')[0]
        if svc in ('flaky-api', 'doomed'):
            continue
        assert ps == 'green', f"pre-stop: {pk}={ps}"

    # Stop redis
    demo.stop('redis')
    time.sleep(0.5)

    # API should show redis probes as stopped
    _, _, _, probes = demo.graph_full()
    for pk, ps in probes.items():
        if pk.startswith('redis.'):
            assert ps == 'stopped', f"api: {pk}={ps} (expected stopped)"

    # WS snapshot must also show redis probes as stopped (not green!)
    ws = demo.ws_snapshot()
    if ws:
        ws_redis = ws.get('services', {}).get('redis', {}).get('probes', {})
        for pname, pdata in ws_redis.items():
            ws_ps = pdata.get('state')
            assert ws_ps != 'green', \
                f"ws: redis.{pname}={ws_ps} (should not be green after stop)"

    # Services depending on redis should be red, not green
    svcs, _, _, _ = demo.graph_full()
    assert svcs['crash-svc'] == 'red', f"crash-svc={svcs['crash-svc']}"
    assert svcs['slow-heal'] == 'red', f"slow-heal={svcs['slow-heal']}"


def test_converge_does_not_deadlock(demo, green):
    """Converge must complete within timeout — no deadlocks.

    Regression: acquiring a second write lock on services inside
    apply_probe_result (for last_log_match) caused deadlock.
    """
    # Stop everything, then converge from scratch
    svcs, _, _, _ = demo.graph_full()
    for svc in svcs:
        if svcs[svc] != 'stopped':
            demo.stop(svc)
    demo.wait_for(lambda: all(v == 'stopped' for v in demo.graph_full()[0].values()))

    # Converge must complete within 30s (deadlock would timeout at 90s)
    d = demo.converge(timeout=30)
    assert d['result'] == 'ok', f"converge failed: {d.get('error')}"
    assert d['duration_ms'] < 10000, f"too slow ({d['duration_ms']}ms) — possible contention"
