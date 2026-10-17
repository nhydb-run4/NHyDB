"""Compatibility shim: ``nhydb_v027`` → native Rust engine.

Only for the unmodified test scripts of the external reference implementation
(not included). Besides the public API it provides the private helpers those
tests use.
"""
from pathlib import Path as _Path

from nhydb import *  # noqa: F401,F403
from nhydb import NHyDB, NHyDBError, ENGINE_VERSION, FILE_FORMAT_VERSION  # noqa: F401
from nhydb import _native as _n
from nhydb.cli import _parse_structural_get_target, main  # noqa: F401
from nhydb_math_v027 import *  # noqa: F401,F403  (identical kernel objects)

StaleIndexError = _n.StaleIndexError
from nhydb.math import MathMap, MathMapLayoutSpec  # noqa: F401,E402
from nhydb.mathlayout import RealmBudgetExceeded  # noqa: F401,E402


def _layout_paths(path):
    return _Path(str(path) + ".layout.enc"), _Path(str(path) + ".layout.key")


_layout_key_load = _n._layout_key_load
_layout_seal_entry = _n._layout_seal_entry
_layout_envelope = _n._layout_envelope
_layout_read_envelope = _n._layout_read_envelope
_layout_open_entry = _n._layout_open_entry
_atomic_layout_replace = _n._atomic_layout_replace

if __name__ == "__main__":
    main()
