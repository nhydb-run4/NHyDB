"""NHyDB – native Rust engine with Python bindings.

The class :class:`NHyDB` provides the complete database API (file format
0.28.2, signature ``NHYDB`` + version bytes, with system space R0,u1,
persistent block directory and an encrypted external layout plan) plus

* a simple single-item API (``put_cell``, ``put_record``, ``get_value``,
  ``get_record``, ``delete_cell``, ``delete_record``; each with ``commit=``) and
* a NumPy batch API (``put_numpy_batch``, ``put_cells_numpy``,
  ``put_objects_numpy``, ``read_numpy``) whose loops run entirely in Rust.

All database logic lives in the native module ``nhydb._native``.
"""
from ._native import (  # noqa: F401
    NHyDB,
    LayoutDef,
    IndexDef,
    ResultFile,
    ResultJob,
    cleanup_results,
    resource_status,
    cancel_job,
    NHyDBError,
    CorruptionError,
    SchemaError,
    StaleIndexError,
    RealmLimitError,
    AddressError,
    DatabaseLockedError,
    DatabaseSizeLimitError,
    IndexConfigError,
    MathLayoutError,
    MathLayoutNotFound,
    MappingFrozenError,
    MathCellConflictError,
    MathError,
    MathSyntaxError,
    MathDomainError,
    LayoutError,
    LayoutStateError,
    RealmBudgetExceeded,
    StalePlanError,
    AllocationConflictError,
    QueryError,
    QueryPlanError,
    QueryDomainError,
    QueryLimitError,
    ResourceLimitError,
    SQLSyntaxError,
    QueryEngine,
    MathLayoutState,
    REALM_SPAN,
    ENGINE_VERSION,
    FILE_FORMAT_VERSION,
    TYPE_CODES,
    normalize_lane,
    lane_name,
    validate_local_v,
    realm_v_bounds,
    global_v,
    split_global_v,
    dynadron_address,
    parse_n_reference,
    dynadron_n,
    n_to_dynadron_address,
    cell_address,
    enc_vu,
    dec_vu,
    enc_vi,
    dec_vi,
)


def open(path, **kwargs):  # noqa: A001 - intentionally like sqlite3.connect
    """Open an existing database (shorthand for ``NHyDB(path)``)."""
    return NHyDB(path, **kwargs)


def create(path, **kwargs):
    """Create a new database (shorthand for ``NHyDB.create(path)``)."""
    return NHyDB.create(path, **kwargs)


# Explicit export list (F052): ``open`` is deliberately not exported by
# ``from nhydb import *`` so that it cannot shadow the builtin ``open``;
# use ``nhydb.open(...)``.
__all__ = [
    "NHyDB", "LayoutDef", "IndexDef", "QueryEngine", "MathLayoutState",
    "ResultFile", "ResultJob", "cleanup_results", "resource_status", "cancel_job",
    "NHyDBError", "CorruptionError", "SchemaError", "StaleIndexError",
    "RealmLimitError", "AddressError", "DatabaseLockedError",
    "DatabaseSizeLimitError", "IndexConfigError", "MathLayoutError",
    "MathLayoutNotFound", "MappingFrozenError", "MathCellConflictError",
    "MathError", "MathSyntaxError", "MathDomainError", "LayoutError",
    "LayoutStateError", "RealmBudgetExceeded", "StalePlanError",
    "AllocationConflictError", "QueryError", "QueryPlanError",
    "QueryDomainError", "QueryLimitError", "ResourceLimitError", "SQLSyntaxError",
    "REALM_SPAN", "ENGINE_VERSION", "FILE_FORMAT_VERSION", "TYPE_CODES",
    "normalize_lane", "lane_name", "validate_local_v", "realm_v_bounds",
    "global_v", "split_global_v", "dynadron_address", "parse_n_reference",
    "dynadron_n", "n_to_dynadron_address", "cell_address",
    "enc_vu", "dec_vu", "enc_vi", "dec_vi",
    "create",
]
