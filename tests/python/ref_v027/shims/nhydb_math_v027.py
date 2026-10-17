"""Kompatibilitätsmodul: ``nhydb_math_v027`` → native Rust-Engine.

Kernel (nhydb.kernel) + MATH_MAP-Sprache (nhydb.math). ``split_global_v``
folgt der v0.27-Fassung (MathDomainError).
"""
from nhydb.kernel import *  # noqa: F401,F403
from nhydb.math import *  # noqa: F401,F403
from nhydb import kernel as _k, math as _m

__all__ = sorted(set(_k.__all__) | set(_m.__all__))
