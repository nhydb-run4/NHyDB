//! Python exception hierarchy of NHyDB.

use pyo3::create_exception;
use pyo3::exceptions::{PyException, PyValueError};

create_exception!(nhydb, NHyDBError, PyException, "Base class of all NHyDB errors.");
create_exception!(nhydb, CorruptionError, NHyDBError, "Corrupted or inconsistent data.");
create_exception!(nhydb, SchemaError, NHyDBError, "Schema/layout error.");
create_exception!(nhydb, StaleIndexError, NHyDBError, "Index is missing or older than the data.");
create_exception!(nhydb, RealmLimitError, NHyDBError, "Local v limit of a realm violated.");
create_exception!(nhydb, AddressError, NHyDBError, "Not a valid NHyDB storage address (GZ/N0).");
create_exception!(nhydb, DatabaseLockedError, NHyDBError, "Could not acquire the database write lock (held by another process).");
create_exception!(nhydb, DatabaseSizeLimitError, NHyDBError, "Physical database size limit exceeded.");
create_exception!(nhydb, IndexConfigError, PyValueError, "Invalid index family/options.");
create_exception!(nhydb, MathLayoutError, NHyDBError, "Error in a MATH layout.");
create_exception!(nhydb, MathLayoutNotFound, MathLayoutError, "MATH layout does not exist.");
create_exception!(nhydb, MappingFrozenError, MathLayoutError, "MATH definition is frozen after the first write.");
create_exception!(nhydb, MathCellConflictError, MathLayoutError, "MATH target cell occupied or duplicated.");
create_exception!(nhydb, MathError, PyException, "Base class of the MATH_MAP language errors.");
create_exception!(nhydb, MathSyntaxError, MathError, "Expression outside the safe MATH_MAP language.");
create_exception!(nhydb, MathDomainError, MathError, "Expression yields an invalid address value.");
create_exception!(nhydb, LayoutError, PyException, "Base class of the MATH realm planner errors.");
create_exception!(nhydb, LayoutStateError, LayoutError, "Realm state inconsistent.");
create_exception!(nhydb, RealmBudgetExceeded, LayoutError, "Automatic realm budget exceeded.");
create_exception!(nhydb, StalePlanError, LayoutError, "Plan is based on stale state.");
create_exception!(nhydb, AllocationConflictError, LayoutError, "Proposed realm is no longer free.");
create_exception!(nhydb, QueryError, PyException, "Base class of the query layer errors.");
create_exception!(nhydb, QueryPlanError, QueryError, "Invalid query plan/operator.");
create_exception!(nhydb, QueryDomainError, QueryError, "Domain error (record/cell).");
create_exception!(nhydb, SQLSyntaxError, QueryError, "Syntax/semantic error in NHySQL.");
