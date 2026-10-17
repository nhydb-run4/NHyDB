"""MATH_MAP language.

``MathMap`` deterministically maps input tuples to ``(u, V_global, side)``;
``V_global`` is split into a logical realm and a local ``v`` using
``REALM_SPAN = 10**100``. Parser and evaluator run natively (Rust) with
Python integer semantics and a safety limit for intermediate results
(``math_max_intermediate_bits``, default 2**20 bits).
"""
from ._native import (  # noqa: F401
    MathError,
    MathSyntaxError,
    MathDomainError,
    MathExpression,
    MathMap,
    MathMapTarget,
    MathMapLayoutSpec,
    MathDryRunReport,
    dry_run,
    split_global_v,
    join_global_v,
    normalize_side,
    set_math_max_intermediate_bits,
    get_math_max_intermediate_bits,
    REALM_SPAN,
)

DEFAULT_REALM_SPAN = REALM_SPAN
DEFAULT_REALM_AUTO_LIMIT = 4096
DEFAULT_DB_MAX_SIZE_BYTES = 50 * (1024 ** 3)
NL = 0
NR = 1

__all__ = [n for n in dir() if not n.startswith("_")]
