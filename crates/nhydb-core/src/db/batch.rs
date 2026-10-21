//! Opt-in write grouping (P011) and explicit rollback (P024).
//!
//! `check_point` validates a single cell exactly like `put_points` without
//! writing anything, so that a write batch of the binding layer reports
//! errors at the call that caused them and writes all collected cells with
//! one `put_points` per realm/lane later. `rollback` discards everything
//! written since the last commit of this handle.

use std::collections::BTreeMap;

use crate::address::{require_positive, validate_local_v, Lane};
use crate::error::{Error, Result};
use crate::nat::Nat;
use crate::val::Val;

use super::sparse::CellInput;
use super::{system, Db};

impl Db {
    /// Validates one cell like `put_points` (open/writable handle, realm,
    /// exclusive MATH realm, u/v range, data type and encoding) without
    /// writing anything (P011).
    pub fn check_point(&self, realm: &Nat, cell: &CellInput) -> Result<()> {
        self.check_open()?;
        if self.readonly {
            return Err(Error::NHyDB("database is opened read-only (readonly=True); writing is not possible".into()));
        }
        self.require_realm(realm)?;
        if let Some(owner) = self.math_owner_of_realm(realm) {
            if !self.internal_math_writes {
                return Err(Error::Schema(format!(
                    "Realm {realm} belongs exclusively to MATH layout {owner}; direct put_points blocked"
                )));
            }
        }
        require_positive("u", &cell.u)?;
        validate_local_v(&cell.v, Some(realm))?;
        let mut m = BTreeMap::new();
        m.insert(cell.u.clone(), vec![cell.clone()]);
        self.encode_sparse_segment(realm, &mut m)?;
        Ok(())
    }

    /// Writes several `(realm, lane)` groups of cells (one `put_points` per
    /// group, without commit) as one unit (G3-04): if a group fails (size
    /// limit, I/O error, realm gone), the file is cut back to its length
    /// before the first group and the in-memory state (segments, overlays,
    /// deletes, index freshness, caches and verified registry, schema) is
    /// rebuilt as it was – the same append rollback as for a failed MATH
    /// batch. Earlier uncommitted writes of the handle are kept. If the
    /// rollback itself fails, the handle is poisoned (reopen required).
    /// Returns the `put_points` results per group.
    pub fn put_point_groups(&mut self, groups: Vec<(Nat, Lane, Vec<CellInput>)>, u_window: u64) -> Result<Vec<Val>> {
        let (start_len, start_next, start_last) = (self.store.len(), self.next_block_id, self.last_valid_end);
        let prior_dirty = self.sys.layout_dirty;
        let mut out = Vec::with_capacity(groups.len());
        for (realm, lane, cells) in groups {
            match self.put_points(&realm, cells, lane, false, u_window) {
                Ok(r) => out.push(r),
                Err(e) => {
                    if self.store.len() != start_len || self.next_block_id != start_next {
                        // put_points never changes the schema, so the
                        // current one is the one before the first group
                        let restored = self
                            .export_schema()
                            .and_then(|schema| self.rollback_math_tail(start_len, start_next, start_last, &schema, prior_dirty));
                        if restored.is_err() {
                            self.sys.poisoned = true;
                        }
                    }
                    return Err(e);
                }
            }
        }
        Ok(out)
    }

    /// `rollback()` (P024, opt-in): discards every block and schema change
    /// of this handle since its last commit and restores the committed state
    /// (data, R0,u1, schema from the active layout plan entry, MATH states,
    /// size limit). Returns the commit boundary. Session settings (plugins,
    /// durability, dyn mode) stay. Nothing beyond the commit boundary of
    /// this handle is touched: with `lock=False` a concurrent writer is
    /// detected first (as for `commit()`).
    pub fn rollback(&mut self) -> Result<u64> {
        self.check_open()?;
        if self.readonly {
            return Err(Error::NHyDB("database is opened read-only (readonly=True); rollback is not possible".into()));
        }
        if self.sys.poisoned {
            return Err(Error::NHyDB(
                "security/durability state unclear after a failed commit (fsync or schema publication); reopen the database".into(),
            ));
        }
        let committed = self.sup.last_valid_end;
        if !self.has_pending() && !self.sys.layout_dirty {
            return Ok(committed);
        }
        self.check_no_concurrent_writer()?;
        self.store.rollback_to(committed)?;
        self.reset_in_memory_state();
        self.next_block_id = self.sup.next_block_id;
        self.checkpoint_offset = self.sup.checkpoint_offset;
        self.last_valid_end = committed;
        self.max_db_size_bytes = crate::config::DEFAULT_DB_MAX_SIZE_BYTES;
        self.last_checkpoint_obj = Val::map();
        self.sys.math_constants_stored = false;
        let record = self.open_scan(committed)?;
        self.sys.record = record;
        let snapshot = system::open_entry(&self.sys.db_id(), &self.sys.active_entry, &self.sys.key)?;
        self.import_schema(&snapshot)?;
        self.sys.layout_dirty = false;
        self.validate_math_layout_state()?;
        Ok(committed)
    }
}
