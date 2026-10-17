//! Runtime plugins for CUSTOM layouts.
//!
//! A CUSTOM layout persists only its plugin ID; the address mapping is
//! provided by an object registered at runtime with the optional methods
//! `logical_address`, `describe`, `type_of`, `spacemap` and
//! `occupied_addresses`. If a method is missing, the engine falls back to
//! the documented default behaviour (checked like Python's `hasattr`).
//!
//! Native Rust plugins implement this trait directly; the PyO3 layer
//! provides a bridge for Python objects (an isolated special path).

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
    /// Equivalent to `hasattr(plugin, name)`.
    fn has(&self, method: PluginMethod) -> bool;

    /// Returns `{"realm","u","v","lane"}` (dict), `[u, v]` or `[r, u, v, lane]`.
    fn logical_address(&self, lay: &LayoutDef, object_id: &Nat, field: &Val, lane: Lane) -> Result<Val>;

    /// Interpretation of an address (dict with e.g. `record`, `field`, `anchor`).
    fn describe(&self, lay: &LayoutDef, address: &Val) -> Result<Val>;

    /// Type name (str) or None.
    fn type_of(&self, lay: &LayoutDef, address: &Val) -> Result<Val>;

    /// Effective SpaceMap (must be a dict).
    fn spacemap(&self, lay: &LayoutDef) -> Result<Val>;

    /// Occupied addresses of an object (`"N…"`, dict or `[u, v]`).
    fn occupied_addresses(&self, db: &Db, lay: &LayoutDef, object_id: &Nat, lane: Lane) -> Result<Vec<Val>>;
}
