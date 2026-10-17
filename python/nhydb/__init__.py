"""NHyDB – native Rust-Engine mit Python-Anbindung.

Die Klasse :class:`NHyDB` bietet die vollständige Datenbank-API
(Dateikennung NHYDB016 mit Systemraum R0,u1 und verschlüsseltem externem
Layoutplan) sowie

* eine einfache Einzel-API (``put_cell``, ``put_record``, ``get_value``,
  ``get_record``, ``delete_cell``, ``delete_record``; jeweils ``commit=``) und
* eine NumPy-Batch-API (``put_numpy_batch``, ``put_cells_numpy``,
  ``put_objects_numpy``, ``read_numpy``), deren Schleifen vollständig in Rust
  laufen.

Die gesamte Datenbanklogik liegt im nativen Modul ``nhydb._native``.
"""
from ._native import (  # noqa: F401
    NHyDB,
    LayoutDef,
    IndexDef,
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


def open(path, **kwargs):  # noqa: A001 - bewusst wie sqlite3.connect
    """Öffnet eine bestehende Datenbank (Kurzform für ``NHyDB(path)``)."""
    return NHyDB(path, **kwargs)


def create(path, **kwargs):
    """Legt eine neue Datenbank an (Kurzform für ``NHyDB.create(path)``)."""
    return NHyDB.create(path, **kwargs)


__all__ = [n for n in dir() if not n.startswith("_")]
