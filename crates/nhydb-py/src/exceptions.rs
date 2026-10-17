//! Ausnahmehierarchie wie v0.14.

use pyo3::create_exception;
use pyo3::exceptions::{PyException, PyValueError};

create_exception!(nhydb, NHyDBError, PyException, "Basisklasse aller NHyDB-Fehler.");
create_exception!(nhydb, CorruptionError, NHyDBError, "Beschädigte oder inkonsistente Daten.");
create_exception!(nhydb, SchemaError, NHyDBError, "Schema-/Layoutfehler.");
create_exception!(nhydb, StaleIndexError, NHyDBError, "Index fehlt oder ist älter als die Daten.");
create_exception!(nhydb, RealmLimitError, NHyDBError, "Lokale v-Grenze eines Realms verletzt.");
create_exception!(nhydb, AddressError, NHyDBError, "Keine gültige NHyDB-Speicheradresse (GZ/N0).");
create_exception!(nhydb, DatabaseLockedError, NHyDBError, "Schreibsperre der Datenbank nicht erhalten (anderer Prozess).");
create_exception!(nhydb, DatabaseSizeLimitError, NHyDBError, "Physische DB-Größengrenze überschritten.");
create_exception!(nhydb, IndexConfigError, PyValueError, "Ungültige Indexfamilie/-optionen.");
create_exception!(nhydb, MathLayoutError, NHyDBError, "Fehler eines MATH-Layouts.");
create_exception!(nhydb, MathLayoutNotFound, MathLayoutError, "MATH-Layout existiert nicht.");
create_exception!(nhydb, MappingFrozenError, MathLayoutError, "MATH-Definition nach erstem Schreiben eingefroren.");
create_exception!(nhydb, MathCellConflictError, MathLayoutError, "MATH-Zielzelle belegt oder doppelt.");
create_exception!(nhydb, MathError, PyException, "Basisklasse der MATH_MAP-Sprache.");
create_exception!(nhydb, MathSyntaxError, MathError, "Ausdruck außerhalb der sicheren MATH_MAP-Sprache.");
create_exception!(nhydb, MathDomainError, MathError, "Ausdruck liefert ungültigen Adresswert.");
create_exception!(nhydb, LayoutError, PyException, "Basisklasse des MATH-Realmplaners.");
create_exception!(nhydb, LayoutStateError, LayoutError, "Realm-Zustand inkonsistent.");
create_exception!(nhydb, RealmBudgetExceeded, LayoutError, "Automatisches Realm-Budget überschritten.");
create_exception!(nhydb, StalePlanError, LayoutError, "Plan basiert auf veraltetem Zustand.");
create_exception!(nhydb, AllocationConflictError, LayoutError, "Vorgeschlagener Realm ist nicht mehr frei.");
create_exception!(nhydb, QueryError, PyException, "Basisklasse der Abfrageschicht.");
create_exception!(nhydb, QueryPlanError, QueryError, "Ungültiger Abfrageplan/Operator.");
create_exception!(nhydb, QueryDomainError, QueryError, "Domänenfehler (record/cell).");
create_exception!(nhydb, SQLSyntaxError, QueryError, "Syntax-/Semantikfehler in NHySQL.");
