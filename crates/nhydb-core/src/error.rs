//! Fehlertypen der nativen NHyDB.
//!
//! Die Varianten bilden die Ausnahmehierarchie der Python-v0.14 ab
//! (`NHyDBError`, `CorruptionError`, `SchemaError`, `StaleIndexError`,
//! `RealmLimitError`, `AddressError`) sowie die dort verwendeten
//! Python-Builtins (`ValueError`, `KeyError`, ...). Die PyO3-Schicht bildet
//! jede Variante auf die entsprechende Python-Ausnahme ab.

use std::any::Any;
use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// Allgemeiner `NHyDBError` (z. B. geschlossene Datenbank).
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
    /// Schreibsperre der Datenbank nicht erhalten (`DatabaseLockedError`).
    Locked(String),
    /// DB-Größengrenze überschritten (`DatabaseSizeLimitError`).
    SizeLimit(String),
    /// MATH-Ausdruck syntaktisch unzulässig (`MathSyntaxError`).
    MathSyntax(String),
    /// MATH-Ausdruck liefert ungültigen Wert (`MathDomainError`).
    MathDomain(String),
    /// Realmplaner: inkonsistenter Zustand (`LayoutStateError`).
    LayoutState(String),
    /// Realmplaner: Budget überschritten (`RealmBudgetExceeded`).
    RealmBudget(String),
    /// Realmplaner: veralteter Plan (`StalePlanError`).
    StalePlan(String),
    /// Realmplaner: Realm nicht mehr frei (`AllocationConflictError`).
    AllocationConflict(String),
    /// MATH-Definition nach erstem Schreiben eingefroren (`MappingFrozenError`).
    MappingFrozen(String),
    /// MATH-Zielzelle belegt/doppelt (`MathCellConflictError`).
    MathCellConflict(String),
    /// MATH-Layout existiert nicht (`MathLayoutNotFound`).
    MathLayoutNotFound(String),
    /// Ungültige Index-Konfiguration (`IndexConfigError`, Unterklasse von `ValueError`).
    IndexConfig(String),
    /// Abfrage: allgemeiner Fehler (`QueryError`).
    Query(String),
    /// Abfrage: Plan/Operator ungültig (`QueryPlanError`).
    QueryPlan(String),
    /// Abfrage: Domänenfehler record/cell (`QueryDomainError`).
    QueryDomain(String),
    /// NHySQL: Syntax-/Semantikfehler (`SQLSyntaxError`).
    SqlSyntax(String),
    Io(std::io::Error),
    /// Fehler aus einem Laufzeit-Plugin (z. B. Python-Ausnahme aus einem
    /// CUSTOM-Layout-Plugin). Die Bindungsschicht kann das Objekt wieder
    /// auspacken und unverändert weiterreichen.
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
            | Error::SqlSyntax(m) => f.write_str(m),
            Error::Io(e) => write!(f, "{e}"),
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
            Error::Io(_) => "OSError",
            Error::Foreign(..) => "ForeignError",
        }
    }
}

