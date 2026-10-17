"""Compatibility shim: ``nhydb_math_v027`` → native Rust engine.

Kernel (nhydb.kernel) + MATH_MAP language (nhydb.math). ``split_global_v``
raises MathDomainError.
"""
from nhydb.kernel import *  # noqa: F401,F403
from nhydb.math import *  # noqa: F401,F403
from nhydb import kernel as _k, math as _m

__all__ = sorted(set(_k.__all__) | set(_m.__all__))
