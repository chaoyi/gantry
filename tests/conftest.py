"""Shared pytest fixtures for gantry integration tests."""

import os
import subprocess
import time

import pytest
import requests

DEMO_DIR = os.path.join(os.path.dirname(__file__), 'fixtures', 'demo')
TIMEOUT_DIR = os.path.join(os.path.dirname(__file__), 'fixtures', 'timeout')


class GantryClient:
    """HTTP client for a gantry instance."""

    def __init__(self, base_url):
        self.base = base_url

    def api(self, method, path, timeout=120, body=None):
        kwargs = {'timeout': timeout}
        if body:
            kwargs['json'] = body
        r = getattr(requests, method)(f'{self.base}/api{path}', **kwargs)
        return r.json()

    def msg(self, text):
        self.api('post', '/message', body={'text': text})

    def converge(self, target='app', timeout=90, skip_restart=False):
        params = f'?timeout={timeout}'
        if skip_restart:
            params += '&skip_restart=true'
        return self.api('post', f'/converge/target/{target}{params}')

    def stop(self, svc):
        return self.api('post', f'/stop/service/{svc}')

    def start(self, svc, timeout=30):
        return self.api('post', f'/start/service/{svc}?timeout={timeout}')

    def restart(self, svc, timeout=60):
        return self.api('post', f'/restart/service/{svc}?timeout={timeout}')

    def reprobe_all(self, timeout=60):
        return self.api('post', f'/reprobe/all?timeout={timeout}')

    def graph(self):
        return self.api('get', '/graph')

    def status(self):
        return self.api('get', '/status')

    def ws_snapshot(self):
        """Get WS snapshot — connect, read first message, disconnect."""
        try:
            import websocket
            ws_url = self.base.replace('http', 'ws') + '/api/ws'
            ws = websocket.create_connection(ws_url, timeout=5)
            import json
            data = json.loads(ws.recv())
            ws.close()
            return data
        except Exception:
            return None

    def graph_full(self):
        g = self.graph()
        svcs, runtimes, tgts, probes = {}, {}, {}, {}
        for s in g['services']:
            svcs[s['name']] = s['state']
            runtimes[s['name']] = s['runtime']
            for p in s['probes']:
                probes[f"{s['name']}.{p['name']}"] = p['state']
        for t in g['targets']:
            tgts[t['name']] = t['state']
        return svcs, runtimes, tgts, probes

    def wait_ready(self, timeout=30):
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                r = requests.get(f'{self.base}/api/graph', timeout=5)
                if r.ok:
                    return True
            except Exception:
                pass
            time.sleep(1)
        return False

    def wait_for(self, predicate, timeout=15):
        deadline = time.time() + timeout
        while time.time() < deadline:
            if predicate():
                return True
            time.sleep(0.5)
        return False

    def wait_svc_state(self, svc_name, expected_runtime, timeout=15):
        return self.wait_for(
            lambda: self.graph_full()[1].get(svc_name) == expected_runtime, timeout
        )

    def wait_probe_state(self, probe_key, expected, timeout=15):
        return self.wait_for(
            lambda: self.graph_full()[3].get(probe_key) == expected, timeout
        )

def _fresh_start(compose_dir, port):
    subprocess.run(['docker', 'compose', 'down', '--remove-orphans', '--timeout', '5'],
                   capture_output=True, timeout=30, cwd=compose_dir)
    subprocess.run(['docker', 'compose', 'up', '--no-start'],
                   capture_output=True, timeout=30, cwd=compose_dir)
    subprocess.run(['docker', 'compose', 'start', 'gantry'],
                   capture_output=True, timeout=15, cwd=compose_dir)
    client = GantryClient(f'http://localhost:{port}')
    assert client.wait_ready(), f"Gantry not responding on port {port}"
    # All services should be stopped (only gantry running).
    # Wait until gantry is idle and all services show stopped.
    client.wait_for(lambda: all(
        v == 'stopped' for v in client.graph_full()[0].values()
    ), timeout=15)
    return client


@pytest.fixture(scope='session')
def demo():
    """Demo fixture (8 services, 2 isolated groups, port 9090). Reuses running instance or starts fresh."""
    client = GantryClient('http://localhost:9090')
    if not client.wait_ready(timeout=5):
        _fresh_start(DEMO_DIR, 9090)
        client = GantryClient('http://localhost:9090')
    yield client


@pytest.fixture(scope='session')
def timeout_fixture():
    """Timeout fixture (1 slow-boot service, port 9092). Reuses running instance or starts fresh."""
    client = GantryClient('http://localhost:9092')
    if not client.wait_ready(timeout=5):
        _fresh_start(TIMEOUT_DIR, 9092)
        client = GantryClient('http://localhost:9092')
    yield client


@pytest.fixture
def green(demo):
    """Full docker compose restart + converge for clean isolation."""
    _fresh_start(DEMO_DIR, 9090)
    r = None
    for _ in range(3):
        r = demo.converge(timeout=120)
        if r.get('result') == 'ok':
            break
        time.sleep(1)
    assert r and r.get('result') == 'ok', f"Failed to converge after 3 attempts: {r}"
    yield demo


@pytest.fixture(autouse=True)
def _emit_test_name(request):
    """Emit test name as MSG event to gantry for UI visibility."""
    # Try demo port first, then timeout port
    test_name = request.node.name
    for port in (9090, 9092):
        try:
            requests.post(
                f'http://localhost:{port}/api/message',
                json={'text': f'▶ {test_name}'},
                timeout=2,
            )
        except Exception:
            pass
    yield
    for port in (9090, 9092):
        try:
            requests.post(
                f'http://localhost:{port}/api/message',
                json={'text': f'✓ {test_name} done'},
                timeout=2,
            )
        except Exception:
            pass
