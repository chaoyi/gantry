"""Test for log_since bug: log probes must not match old success lines from before disruption."""

import time

import pytest


@pytest.mark.slow
def test_log_probe_after_disruption(demo):
    """After disruption, log probe finds failure (not old success from boot)."""
    demo.ensure_green()
    demo.stop('postgres')
    time.sleep(10)  # wait for stuck-svc to enter broken state

    d = demo.converge(skip_restart=True)
    dep = d.get('probes', {}).get('stuck-svc.dep', {})
    assert dep.get('state') == 'red', \
        f"stuck-svc.dep should be red but is {dep.get('state')}, logs={dep.get('logs')}"
