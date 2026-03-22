"""Timeout and cancellation tests.

Uses timeout fixture (1 slow-boot service, 10s boot delay, port 9092).
"""

import threading
import time

import pytest

slow = pytest.mark.slow


def _stop_all(client):
    g = client.graph()
    for s in g.get('services', []):
        if s['runtime'] != 'stopped':
            client.stop(s['name'])
    time.sleep(1)


def test_converge_respects_timeout(timeout_fixture):
    """2s timeout on 10s service → returns failed."""
    client = timeout_fixture
    _stop_all(client)
    time.sleep(1)
    d = client.api('post', '/converge/target/all?timeout=2', timeout=10)
    assert d.get('result') == 'failed'
    assert d.get('duration_ms', 0) < 5000


def test_op_lock_released(timeout_fixture):
    """After timeout, next operation works."""
    client = timeout_fixture
    _stop_all(client)
    client.api('post', '/converge/target/all?timeout=2', timeout=10)
    time.sleep(1)
    _, op = client.graph_full()[0], client.graph().get('current_op')
    assert op is None
    d = client.reprobe_all(timeout=5)
    assert 'error' not in d or 'Conflict' not in str(d.get('error', ''))


def test_no_orphaned_changes(timeout_fixture):
    """State stable after timeout."""
    client = timeout_fixture
    _stop_all(client)
    client.api('post', '/converge/target/all?timeout=2', timeout=10)
    time.sleep(2)
    s1 = client.graph_full()[0]
    time.sleep(5)
    s2 = client.graph_full()[0]
    changed = {k: (s1.get(k), s2.get(k)) for k in s1 if s1.get(k) != s2.get(k)}
    assert len(changed) == 0, f"State changed: {changed}"


def test_start_respects_timeout(timeout_fixture):
    """Start with 3s timeout returns within 5s."""
    client = timeout_fixture
    _stop_all(client)
    t0 = time.time()
    client.api('post', '/start/service/slow-boot?timeout=3', timeout=10)
    assert time.time() - t0 < 5


@slow
def test_converge_sufficient_timeout(timeout_fixture):
    """30s timeout on 10s service → succeeds."""
    client = timeout_fixture
    _stop_all(client)
    d = client.api('post', '/converge/target/all?timeout=30', timeout=35)
    assert d.get('result') == 'ok'
    assert 10000 <= d.get('duration_ms', 0) <= 20000


def test_timeout_error_message(timeout_fixture):
    """Timeout response has clear error info."""
    client = timeout_fixture
    _stop_all(client)
    time.sleep(1)
    d = client.api('post', '/converge/target/all?timeout=2', timeout=10)
    assert d.get('result') == 'failed'
    assert 'timeout' in d.get('error', '').lower()
    assert 'probes' in d
    assert 'targets' in d


@slow
def test_restart_timeout(timeout_fixture):
    """Restart completes within reasonable time."""
    client = timeout_fixture
    client.api('post', '/converge/target/all?timeout=30', timeout=35)
    t0 = time.time()
    client.api('post', '/restart/service/slow-boot?timeout=30', timeout=35)
    assert time.time() - t0 < 25


def test_reprobe_fast(timeout_fixture):
    """Reprobe is single-attempt scan, returns fast."""
    client = timeout_fixture
    client.api('post', '/converge/target/all?timeout=30', timeout=35)
    t0 = time.time()
    d = client.reprobe_all(timeout=10)
    assert d.get('result') == 'ok'
    assert time.time() - t0 < 3


@slow
def test_concurrent_op_rejected(timeout_fixture):
    """Second operation during active converge gets 409."""
    client = timeout_fixture
    _stop_all(client)
    result_holder = [None]

    def slow_converge():
        result_holder[0] = client.api('post', '/converge/target/all?timeout=20', timeout=25)

    t = threading.Thread(target=slow_converge)
    t.start()
    time.sleep(1)
    d = client.reprobe_all(timeout=5)
    has_conflict = 'error' in d and ('Conflict' in str(d.get('error', '')) or 'in progress' in str(d.get('error', '')))
    assert has_conflict, f"Expected 409, got: {d.get('error')}"
    t.join(timeout=25)
