//! Laufzeit-Plugins für CUSTOM-Layouts.
//!
//! In v0.14 persistiert ein CUSTOM-Layout nur seine Plugin-ID; die
//! Adressabbildung liefert ein zur Laufzeit registriertes Objekt mit den
//! optionalen Methoden `logical_address`, `describe`, `type_of`, `spacemap`
//! und `occupied_addresses`. Fehlt eine Methode, greift die Engine – wie
//! v0.14 mit `hasattr` – auf das dokumentierte Fallback-Verhalten zurück.
//!
//! Native Rust-Plugins implementieren dieses Trait direkt; die PyO3-Schicht
//! stellt eine Brücke für Python-Objekte bereit (isolierter Sonderweg).

use crate::address::Lane;
use crate::db::Db;
use crate::error::Result;
use crate::nat::Nat;
use crate::schema::LayoutDef;
use crate::val::Val;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginMethod {
    LogicalAddress,
    Describe,
    TypeOf,
    Spacemap,
    OccupiedAddresses,
}

pub trait LayoutPlugin: Send + Sync {
    /// Entspricht `hasattr(plugin, name)`.
    fn has(&self, method: PluginMethod) -> bool;

    /// Liefert `{"realm","u","v","lane"}` (dict), `[u, v]` oder `[r, u, v, lane]`.
    fn logical_address(&self, lay: &LayoutDef, object_id: &Nat, field: &Val, lane: Lane) -> Result<Val>;

    /// Interpretation einer Adresse (dict mit u. a. `record`, `field`, `anchor`).
    fn describe(&self, lay: &LayoutDef, address: &Val) -> Result<Val>;

    /// Typname (str) oder None.
    fn type_of(&self, lay: &LayoutDef, address: &Val) -> Result<Val>;

    /// Effektive SpaceMap (muss dict sein).
    fn spacemap(&self, lay: &LayoutDef) -> Result<Val>;

    /// Belegte Adressen eines Objekts (`"N…"`, dict oder `[u, v]`).
    fn occupied_addresses(&self, db: &Db, lay: &LayoutDef, object_id: &Nat, lane: Lane) -> Result<Vec<Val>>;
}
