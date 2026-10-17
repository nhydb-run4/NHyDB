"""MATH_MAP-Sprache (Gegenstück zum MATH-Teil von ``nhydb_math_v027``).

``MathMap`` bildet Eingabetupel deterministisch auf ``(u, V_global, side)``
ab; ``V_global`` wird mit ``REALM_SPAN = 10**100`` in logischen Realm und
lokales ``v`` zerlegt. Parser und Auswerter laufen nativ (Rust) mit der
Python-Semantik der Referenz und einer Schutzgrenze für Zwischenergebnisse
(``math_max_intermediate_bits``, Standard 2**20 Bit).
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
