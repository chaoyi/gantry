"""Recovery behavior tests for converge algorithm.

Uses demo fixture (6 services):
  postgres, redis         — infrastructure
  crash-svc               — exits when redis dies (restart_on_fail: true)
  stuck-svc               — broken state when postgres dies (restart_on_fail: true)
  slow-heal (HEAL_DELAY=20) — reconnects slowly (restart_on_fail: true)
  fast-heal               — reconnects in ~2s (restart_on_fail: false)
"""

import subprocess
import time

import pytest

slow = pytest.mark.slow


def test_fresh_converge(demo):
    """All stopped → converge → all green, no restarts."""
    svcs, _, _, _ = demo.graph_full()
    for svc in svcs:
        if svcs[svc] != 'stopped':
            demo.stop(svc)
    demo.wait_for(lambda: all(v == 'stopped' for v in demo.graph_full()[0].values()))

    d = demo.converge()
    assert d['result'] == 'ok'
    assert len(d['actions'].get('restarted', [])) == 0
    assert len(d['actions'].get('started', [])) == 6
    assert d['duration_ms'] < 8000  # parallel startup

    svcs, _, tgts, _ = demo.graph_full()
    assert all(s == 'green' for s in svcs.values())
    assert all(t == 'green' for t in tgts.values())


@slow
def test_crash_recovery(demo):
    """crash-svc exits when redis dies → started by converge."""
    demo.ensure_green()
    demo.stop('redis')
    assert demo.wait_svc_state('crash-svc', 'stopped', timeout=15), \
        f"crash-svc didn't die: {demo.graph_full()[1].get('crash-svc')}"

    d = demo.converge()
    assert d['result'] == 'ok'
    assert 'crash-svc' in d['actions'].get('started', [])
    assert 'crash-svc' not in d['actions'].get('restarted', [])


@slow
def test_stuck_recovery(demo):
    """stuck-svc running but broken → restarted by converge."""
    demo.ensure_green()
    demo.stop('postgres')
    time.sleep(8)  # wait for monitoring loop to detect and enter broken state

    _, runtimes, _, _ = demo.graph_full()
    assert runtimes.get('stuck-svc') == 'running'

    d = demo.converge()
    assert d['result'] == 'ok'
    assert 'stuck-svc' in d['actions'].get('restarted', [])
    assert 5000 <= d['duration_ms'] <= 25000


@slow
def test_slow_heal_recovery(demo):
    """slow-heal reconnects in 20s, restart is faster."""
    demo.ensure_green()
    demo.stop('redis')
    demo.wait_probe_state('slow-heal.http', 'red', timeout=15)
    time.sleep(8)

    d = demo.converge()
    assert d['result'] == 'ok'
    assert 'slow-heal' in d['actions'].get('restarted', [])
    assert 5000 <= d['duration_ms'] <= 20000


@slow
def test_fast_heal_recovery(demo):
    """fast-heal reconnects in ~2s, no restart needed."""
    demo.ensure_green()
    demo.stop('postgres')
    demo.wait_probe_state('fast-heal.http', 'red', timeout=15)

    d = demo.converge()
    assert d['result'] == 'ok'
    assert 'fast-heal' not in d['actions'].get('restarted', [])

    svcs, _, _, _ = demo.graph_full()
    assert svcs.get('fast-heal') == 'green'


@slow
def test_both_infra_down(demo):
    """All 4 behaviors at once."""
    demo.ensure_green()
    demo.stop('postgres')
    demo.stop('redis')
    demo.wait_svc_state('crash-svc', 'stopped', timeout=15)
    demo.wait_probe_state('stuck-svc.http', 'red', timeout=15)
    time.sleep(8)

    d = demo.converge(timeout=120)
    assert d['result'] == 'ok'
    started = d['actions'].get('started', [])
    restarted = d['actions'].get('restarted', [])
    assert 'postgres' in started
    assert 'redis' in started
    assert 'fast-heal' not in restarted

    _, _, tgts, _ = demo.graph_full()
    assert all(t == 'green' for t in tgts.values())


@slow
def test_skip_restart(demo):
    """skip_restart=true: diagnose only."""
    demo.ensure_green()
    demo.stop('redis')
    demo.wait_probe_state('slow-heal.http', 'red', timeout=15)
    time.sleep(8)

    d = demo.converge(skip_restart=True)
    assert d['result'] == 'failed'
    assert len(d['actions'].get('restarted', [])) == 0
    assert d['duration_ms'] < 25000


@slow
def test_reprobe_self_healing(demo):
    """Reprobe: fast-heal self-heals, stuck-svc stays broken."""
    demo.ensure_green()
    demo.stop('postgres')
    demo.wait_probe_state('fast-heal.http', 'red', timeout=15)
    time.sleep(8)

    demo.start('postgres')
    demo.wait_probe_state('postgres.ready', 'green', timeout=30)
    time.sleep(3)

    d = demo.reprobe_all()
    assert d['result'] == 'ok'

    svcs, _, _, _ = demo.graph_full()
    assert svcs.get('fast-heal') == 'green'
    assert svcs.get('stuck-svc') != 'green'


@slow
def test_docker_watcher(demo):
    """External docker stop/start detected automatically."""
    demo.ensure_green()

    subprocess.run(['docker', 'stop', 'demo-redis'], capture_output=True, timeout=15)
    assert demo.wait_svc_state('redis', 'stopped', timeout=10)

    subprocess.run(['docker', 'start', 'demo-redis'], capture_output=True, timeout=15)
    assert demo.wait_svc_state('redis', 'running', timeout=10)


def test_op_lock(demo):
    """Op lock released after converge — next op works."""
    demo.ensure_green()
    demo.converge(timeout=5)
    time.sleep(1)
    g = demo.graph()
    assert 'services' in g
    d = demo.reprobe_all(timeout=5)
    assert d.get('result') is not None


def test_probe_results_have_logs(demo):
    """Log probes include matched lines in response."""
    demo.ensure_green()
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
