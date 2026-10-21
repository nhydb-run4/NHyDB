//! Error types of the native NHyDB.
//!
//! The variants mirror the Python exception hierarchy
//! (`NHyDBError`, `CorruptionError`, `SchemaError`, `StaleIndexError`,
//! `RealmLimitError`, `AddressError`) plus the Python builtins used alongside
//! it (`ValueError`, `KeyError`, ...). The PyO3 layer maps each variant to the
//! corresponding Python exception.

use std::any::Any;
use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// General `NHyDBError` (e.g. database closed).
    NHyDB(String),
    Corruption(String),
    Schema(String),
    StaleIndex(String),
    RealmLimit(String),
    Address(String),
    /// Python `ValueError`.
    Value(String),
    /// Python `KeyError`.
    Key(String),
    /// Python `IndexError`.
    Index(String),
    /// Python `TypeError`.
    Type(String),
    /// Python `FileNotFoundError`.
    NotFound(String),
    /// Python `FileExistsError`.
    Exists(String),
    /// Database write lock could not be acquired (`DatabaseLockedError`).
    Locked(String),
    /// Database size limit exceeded (`DatabaseSizeLimitError`).
    SizeLimit(String),
    /// MATH expression is syntactically invalid (`MathSyntaxError`).
    MathSyntax(String),
    /// MATH expression yields an invalid value (`MathDomainError`).
    MathDomain(String),
    /// Realm planner: inconsistent state (`LayoutStateError`).
    LayoutState(String),
    /// Realm planner: budget exceeded (`RealmBudgetExceeded`).
    RealmBudget(String),
    /// Realm planner: stale plan (`StalePlanError`).
    StalePlan(String),
    /// Realm planner: realm no longer free (`AllocationConflictError`).
    AllocationConflict(String),
    /// MATH definition frozen after the first write (`MappingFrozenError`).
    MappingFrozen(String),
    /// MATH target cell occupied/duplicated (`MathCellConflictError`).
    MathCellConflict(String),
    /// MATH layout does not exist (`MathLayoutNotFound`).
    MathLayoutNotFound(String),
    /// Invalid index configuration (`IndexConfigError`, subclass of `ValueError`).
    IndexConfig(String),
    /// Query: general error (`QueryError`).
    Query(String),
    /// Query: invalid plan/operator (`QueryPlanError`).
    QueryPlan(String),
    /// Query: record/cell domain error (`QueryDomainError`).
    QueryDomain(String),
    /// NHySQL: syntax/semantic error (`SQLSyntaxError`).
    SqlSyntax(String),
    /// Resource exhaustion (allocation would fail or exceed the physical
    /// memory); Python `MemoryError`.
    Memory(String),
    Io(std::io::Error),
    /// OS error on a specific side file (layout plan, key file) – carries the
    /// affected path so Python gets the right `OSError.filename` (F048).
    IoPath(std::io::Error, std::path::PathBuf),
    /// Error raised by a runtime plugin (e.g. a Python exception from a
    /// CUSTOM layout plugin). The binding layer can unwrap the object and
    /// re-raise it unchanged.
    Foreign(Box<dyn Any + Send + Sync>, String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NHyDB(m)
            | Error::Corruption(m)
            | Error::Schema(m)
            | Error::StaleIndex(m)
            | Error::RealmLimit(m)
            | Error::Address(m)
            | Error::Value(m)
            | Error::Key(m)
            | Error::Index(m)
            | Error::Type(m)
            | Error::NotFound(m)
            | Error::Exists(m)
            | Error::Locked(m)
            | Error::SizeLimit(m)
            | Error::IndexConfig(m)
            | Error::MathLayoutNotFound(m)
            | Error::MathSyntax(m)
            | Error::MathDomain(m)
            | Error::LayoutState(m)
            | Error::RealmBudget(m)
            | Error::StalePlan(m)
            | Error::AllocationConflict(m)
            | Error::MappingFrozen(m)
            | Error::MathCellConflict(m)
            | Error::Query(m)
            | Error::QueryPlan(m)
            | Error::QueryDomain(m)
            | Error::SqlSyntax(m)
            | Error::Memory(m) => f.write_str(m),
            Error::Io(e) => write!(f, "{e}"),
            Error::IoPath(e, p) => write!(f, "{e}: '{}'", p.display()),
            Error::Foreign(_, m) => f.write_str(m),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl Error {
    /// Attaches `path` to an OS error (`Io`/`IoPath` → `IoPath(_, path)`);
    /// every other error is returned unchanged (F048).
    pub fn at_path(self, path: &std::path::Path) -> Error {
        match self {
            Error::Io(e) | Error::IoPath(e, _) => Error::IoPath(e, path.to_path_buf()),
            other => other,
        }
    }

    pub fn kind_name(&self) -> &'static str {
        match self {
            Error::NHyDB(_) => "NHyDBError",
            Error::Corruption(_) => "CorruptionError",
            Error::Schema(_) => "SchemaError",
            Error::StaleIndex(_) => "StaleIndexError",
            Error::RealmLimit(_) => "RealmLimitError",
            Error::Address(_) => "AddressError",
            Error::Value(_) => "ValueError",
            Error::Key(_) => "KeyError",
            Error::Index(_) => "IndexError",
            Error::Type(_) => "TypeError",
            Error::NotFound(_) => "FileNotFoundError",
            Error::Exists(_) => "FileExistsError",
            Error::Locked(_) => "DatabaseLockedError",
            Error::SizeLimit(_) => "DatabaseSizeLimitError",
            Error::IndexConfig(_) => "IndexConfigError",
            Error::MathLayoutNotFound(_) => "MathLayoutNotFound",
            Error::MathSyntax(_) => "MathSyntaxError",
            Error::MathDomain(_) => "MathDomainError",
            Error::LayoutState(_) => "LayoutStateError",
            Error::RealmBudget(_) => "RealmBudgetExceeded",
            Error::StalePlan(_) => "StalePlanError",
            Error::AllocationConflict(_) => "AllocationConflictError",
            Error::MappingFrozen(_) => "MappingFrozenError",
            Error::MathCellConflict(_) => "MathCellConflictError",
            Error::Query(_) => "QueryError",
            Error::QueryPlan(_) => "QueryPlanError",
            Error::QueryDomain(_) => "QueryDomainError",
            Error::SqlSyntax(_) => "SQLSyntaxError",
            Error::Memory(_) => "MemoryError",
            Error::Io(_) | Error::IoPath(..) => "OSError",
            Error::Foreign(..) => "ForeignError",
        }
    }
}

