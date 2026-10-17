//! NHyDB – native Rust engine.
//!
//! The mathematical cell identity is `(R, u, v_local, lane)`; `N` is derived
//! purely arithmetically (see [`address`]).
//! Query layer: [`query`] (QueryEngine), [`sql`] (NHySQL);
//! MATH_MAP: [`mathexpr`], [`mathlayout`], `db::math`.

// Nested conditions are kept deliberately: they mirror the structure of the
// original algorithm description and keep the control flow easy to follow.
#![allow(clippy::collapsible_if, clippy::collapsible_match)]

pub mod address;
pub mod codec;
pub mod config;
pub mod crypto;
pub mod db;
pub mod error;
pub mod format;
pub mod mathexpr;
pub mod mathlayout;
pub mod nat;
pub mod platform;
pub mod plugin;
pub mod query;
pub mod schema;
pub mod setops;
pub mod sql;
pub mod storage;
pub mod textindex;
pub mod unicode;
#[rustfmt::skip]
mod unicode_tables;
pub mod val;
pub mod zlib;

pub use address::{Lane, NL, NR};
pub use codec::{FieldDef, TypeCode};
pub use config::Config;
pub use db::dense::{ColData, DenseRecord, DenseRows};
pub use db::index::Cond;
pub use db::layout::{EvolveSpec, LayoutRef, LayoutSpec};
pub use db::maint::CompactOptions;
pub use db::read::GetMode;
pub use db::sparse::{CellInput, MaterializedPoint};
pub use db::{Db, OpenOptions, SVal};
pub use error::{Error, Result};
pub use nat::Nat;
pub use platform::{Durability, LockConfig};
pub use storage::SyncMode;
pub use val::Val;
