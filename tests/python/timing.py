"""T-10: wall-clock limits of tests, scalable for slow or loaded machines.

``NHYDB_TEST_TIME_FACTOR`` (default 1) multiplies every absolute limit, e.g.
``NHYDB_TEST_TIME_FACTOR=3`` on a VM running other jobs. Relative limits (a
factor between two measurements of the same test) need no scaling.
"""
import os


def time_factor() -> float:
    try:
        f = float(os.environ.get("NHYDB_TEST_TIME_FACTOR", "1"))
    except ValueError:
        return 1.0
    return f if f > 0 else 1.0


def limit(seconds: float) -> float:
    """``seconds`` scaled by ``NHYDB_TEST_TIME_FACTOR``."""
    return seconds * time_factor()
