"""Delete API, cell existence and write order.

Rules:

* The public delete API is ``delete_cell``, ``delete_area`` and
  ``delete_record`` (``delete`` and ``delete_point`` are gone); the batch
  offers the same three.
* The newest write of a cell wins, whatever layer it went to (F1): a
  ``put`` after a ``put_cell``/``delete_cell`` of a field reads the ``put``.
* ``put`` replaces the BASE record of its shape as a whole; ``put_cell``
  changes one cell. Cells outside the new shape (extensions, reserved BASE
  positions) follow their own writes.
* A stored ``None`` is a live cell; a tombstoned, never written or
  missing-from-the-stored-shape cell is not.
* ``delete_area``/``delete_record`` return the number of distinct live
  cells deleted, ``delete_cell`` whether the cell lived.

The oracle :class:`Model` knows only these rules (cells with the origin of
their last write); every read path of the database is compared with it in
every state: uncommitted, committed, reopened, after an index build, with
incremental index maintenance, compacted and reopened again.
"""
import gc
import os
import random
import subprocess
import sys
import textwrap

import pytest

import nhydb
from nhydb.query import Q, QueryEngine

import test_perf_query_paths as pq

R = 1
SHAPE_A = [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "x", "type": "varint"}, {"v": 3, "name": "y", "type": "utf8"}]
SHAPE_B = [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "x", "type": "varint"}]
RESERVE = 20                     # BASE = v1..v20, extensions from v21
V_RES = 15                       # reserved BASE position, no name
V_EXT = 30                       # named extension "ext"
V_UNNAMED = 40                   # extension cell without definition
VS = (1, 2, 3, V_RES, V_EXT, V_UNNAMED)
TYPES = {1: "varuint", 2: "varint", 3: "utf8", V_RES: "varint", V_EXT: "varuint", V_UNNAMED: "utf8"}


def _schema(db, index=True):
    db.create_realm(R, "r")
    db.create_shape(1, R, "A", SHAPE_A)
    db.create_shape(2, R, "B", SHAPE_B)
    db.create_classic_layout(1, R, "main", shape_id=1, base_reserve_to=RESERVE)
    db.define_extension(R, V_EXT, "varuint", name="ext")
    if index:
        db.create_index(1, R, "ix", "x")
    db.commit()


class Model:
    """Logical cells (u, v) → (value, origin); origin "d" = written by the
    DENSE record of u, "c" = written alone. ``put`` drops the "d" cells its
    shape lacks; nothing else knows about storage."""

    def __init__(self):
        self.by_u = {}              # u -> {v: (value, origin)}
        self.dense = set()          # u with a DENSE record (put_extensions needs one)

    def copy(self):
        m = Model()
        m.by_u = {u: dict(c) for u, c in self.by_u.items()}
        m.dense = set(self.dense)
        return m

    def _u(self, u):
        return self.by_u.setdefault(u, {})

    def _tidy(self, u):
        if not self.by_u.get(u):
            self.by_u.pop(u, None)

    def put(self, u, vs, vals):
        c = self._u(u)
        for v in [v for v, (_, o) in c.items() if o == "d" and v not in vs]:
            del c[v]
        for v, x in zip(vs, vals):
            c[v] = (x, "d")
        self.dense.add(u)

    def put_cell(self, u, v, x):
        self._u(u)[v] = (x, "c")

    def delete_cell(self, u, v):
        hit = self.by_u.get(u, {}).pop(v, None) is not None
        self._tidy(u)
        return hit

    def delete_area(self, u0, u1, v0, v1):
        n = 0
        for u in [u for u in self.by_u if u0 <= u <= u1]:
            c = self.by_u[u]
            hit = [v for v in c if v0 <= v <= v1]
            if hit and len(hit) == len(c):
                self.dense.discard(u)       # the whole record goes (DELETE block)
            for v in hit:
                del c[v]
            n += len(hit)
            self._tidy(u)
        return n

    def delete_record(self, u):
        n = self.delete_area(u, u, 1, 10**9)
        if n:
            self.dense.discard(u)
        return n

    def put_extensions(self, u, items):
        """merge=False: every extension cell (v > RESERVE) not listed goes."""
        if u not in self.dense:
            raise KeyError(u)
        c = self._u(u)
        for v in [v for v in c if v > RESERVE and v not in items]:
            del c[v]
        for v, x in items.items():
            c[v] = (x, "c")
        self._tidy(u)

    def live(self):
        return {(u, v): x for u, c in self.by_u.items() for v, (x, _) in c.items()}

    def records(self):
        return sorted(u for u, c in self.by_u.items() if c)


def _val(v, rng):
    if rng.random() < 0.12:
        return None
    return {1: rng.randrange(50), 2: rng.randint(-3, 3), 3: rng.choice("abc"), V_RES: rng.randint(-9, 9),
            V_EXT: rng.randrange(7), V_UNNAMED: rng.choice(["p", "q"])}[v]


def _dense_val(v, rng):
    return {1: rng.randrange(50), 2: rng.randint(-3, 3), 3: rng.choice("abc")}[v]


def _strip(cell):
    return None if cell is None else (cell["present"], cell["value"] if cell["present"] else None)


def _us(db, res):
    """u of the record refs of a QueryEngine result (CLASSIC: anchor N)."""
    return sorted(nhydb.n_to_dynadron_address(int(r[1:]))["u"] for r in res["refs"])


def check(db, m, us, where):
    """Every read path against the model."""
    live = m.live()
    # single cells: get_cell / get_value
    for u in us:
        for v in VS:
            want = (u, v) in live
            got = db.get_cell(R, u, v, dyn_address=False)
            present = bool(got and got["present"])
            assert present == want, (where, "get_cell present", u, v, got, live.get((u, v), "-"))
            if want:
                assert got["value"] == live[(u, v)], (where, "get_cell value", u, v, got)
                assert db.get_value(R, u, v) == live[(u, v)], (where, "get_value", u, v)
            else:
                assert db.get_value(R, u, v) is None, (where, "get_value dead", u, v)
    # the cell cursor (materialization scan): exactly the live cells
    cells = {(c["u"], c["v"]): c["value"] for c in db.iter_materialized_cells(R)}
    assert cells == live, (where, "cell cursor", sorted(set(cells.items()) ^ set(live.items()), key=str)[:8])
    # pure SPARSE points: never a state a newer record hides
    for p in db.iter_materialized_sparse_points(R):
        assert live.get((p["u"], p["v"]), "-") == p["value"], (where, "sparse point", p)
    # records: SQL and QueryEngine (both paths), field predicates
    recs = m.records()
    assert db.sql("SELECT COUNT(*) FROM main")["rows"][0]["count_all"] == len(recs), (where, "COUNT(*)")
    for k in (-3, 0, 2):
        want = sorted(u for u in recs if live.get((u, 2), "-") == k)
        try:
            by_index = db.find_u(R, {"ix": k})
        except (nhydb.SchemaError, nhydb.StaleIndexError):   # no index / never built
            by_index = want
        assert list(by_index) == want, (where, "index", k, list(by_index))
        assert db.sql(f"SELECT COUNT(*) FROM main WHERE x = {k}")["rows"][0]["count_all"] == len(want), (where, "sql x =", k)
    # T-07: read_numpy (record existence and cell masks), select, sql_numpy
    rec_set = set(recs)
    if us:
        u0, u1 = min(us), max(us)
        out = db.read_numpy(R, u0, u1 - u0 + 1, ["id", "x"], masks=True)
        for i, u in enumerate(range(u0, u1 + 1)):
            assert bool(out["_present"][i]) == (u in rec_set), (where, "read_numpy _present", u)
            for name, v in (("id", 1), ("x", 2)):
                lv = (u, v) in live
                assert bool(out["_cells"][name][i]) == lv, (where, "read_numpy _cells", u, name, live.get((u, v), "-"))
                if lv and live[(u, v)] is not None:
                    assert int(out[name][i]) == live[(u, v)], (where, "read_numpy value", u, name)
        # select: the record view of get() (rows of u with a DENSE record)
        for row in db.select(R, u0, u1, ["x"]):
            assert row["x"] == live.get((row["_u"], 2)), (where, "select x", row)
    got = db.sql_numpy("SELECT x FROM main")["x"].tolist()
    assert sorted(got, key=repr) == sorted((live.get((u, 2)) for u in recs), key=repr), (where, "sql_numpy x")
    q = QueryEngine(db)
    for k in (-3, 0, 2):
        want = sorted(u for u in recs if live.get((u, 2), "-") == k)
        assert _us(db, q.execute(Q.field("x") == k, realm=R, return_mode="n")) == want, (where, "engine x", k)
    assert q.execute(Q.all(), realm=R, return_mode="count")["count"] == len(recs), (where, "engine count")
    want_y = sorted(u for u in recs if live.get((u, 3), "-") == "a")
    assert _us(db, q.execute(Q.field("y") == "a", realm=R, return_mode="n")) == want_y, (where, "engine y")
    # get(): named fields of the live DENSE record, unnamed BASE cells always
    for u in us:
        rec = db.get(R, u)
        if rec is None:
            continue
        for v, name in ((1, "id"), (2, "x"), (3, "y"), (V_EXT, "ext"), (V_RES, f"v{V_RES}"), (V_UNNAMED, f"v{V_UNNAMED}")):
            if (u, v) in live:
                assert rec.get(name) == live[(u, v)], (where, "get()", u, name, rec)
            elif name in rec:
                assert rec[name] is None, (where, "get() dead field", u, name, rec)


def _random_ops(db, m, rng, steps, max_u):
    """Random history with commits and rollbacks; the model follows."""
    committed = m.copy()
    for _ in range(steps):
        k = rng.random()
        u = rng.randint(1, max_u)
        if k < 0.22:
            n = rng.choice([1, 1, 2, 5])
            shape = rng.choice([(1, SHAPE_A), (2, SHAPE_B)])
            vs = [f["v"] for f in shape[1]]
            rows = [[_dense_val(v, rng) for v in vs] for _ in range(n)]
            db.put_dense_batch(R, u, rows, shape_id=shape[0])
            for i, row in enumerate(rows):
                m.put(u + i, vs, row)
        elif k < 0.45:
            v = rng.choice(VS)
            x = _val(v, rng)
            db.put_cell(R, u, v, x, type=TYPES[v])
            m.put_cell(u, v, x)
        elif k < 0.58:
            v = rng.choice(VS)
            assert db.delete_cell(R, u, v) == m.delete_cell(u, v), ("delete_cell", u, v)
        elif k < 0.66:
            assert db.delete_record(1, u) == m.delete_record(u), ("delete_record", u)
        elif k < 0.74:
            u1 = u + rng.choice([0, 0, 1, 4])
            v0 = rng.choice([1, 2, 3, 10, 16, 21])
            v1 = v0 + rng.choice([0, 1, 13, 40])
            assert db.delete_area(R, u, u1, v0, v1) == m.delete_area(u, u1, v0, v1), ("delete_area", u, u1, v0, v1)
        elif k < 0.79:
            items = {V_EXT: rng.randrange(7)} if rng.random() < 0.7 else {}
            try:
                db.put_extensions(R, u, {"ext": items[V_EXT]} if items else {}, merge=False)
                m.put_extensions(u, items)
            except KeyError:
                with pytest.raises(KeyError):
                    m.put_extensions(u, items)
        elif k < 0.92:
            db.commit()
            committed = m.copy()
        else:
            db.rollback()
            m = committed.copy()
    return m


# ----------------------------------------------------------------------
# F1: the newest write wins
# ----------------------------------------------------------------------

def _f1_db(path):
    db = nhydb.NHyDB.create(str(path))
    _schema(db)
    m = Model()
    A = [1, 2, 3]
    # put(x=10), put_cell(x=11), put(x=12) → 12
    db.put_dense_batch(R, 1, [[1, 10, "a"]]); m.put(1, A, [1, 10, "a"])
    db.put_cell(R, 1, 2, 11); m.put_cell(1, 2, 11)
    db.put_dense_batch(R, 1, [[1, 12, "a"]]); m.put(1, A, [1, 12, "a"])
    # put(x=10), delete_cell(x), put(x=20) → 20
    db.put_dense_batch(R, 2, [[2, 10, "b"]]); m.put(2, A, [2, 10, "b"])
    assert db.delete_cell(R, 2, 2) is True; m.delete_cell(2, 2)
    db.put_dense_batch(R, 2, [[2, 20, "b"]]); m.put(2, A, [2, 20, "b"])
    # delete_record, then put → only the new record
    db.put_dense_batch(R, 3, [[3, 1, "c"]]); m.put(3, A, [3, 1, "c"])
    db.put_cell(R, 3, 2, 2); db.put_cell(R, 3, V_EXT, 4); m.put_cell(3, 2, 2); m.put_cell(3, V_EXT, 4)
    assert db.delete_record(1, 3) == 4 == m.delete_record(3)      # id, x (one cell), y, ext
    db.put_dense_batch(R, 3, [[3, 30, "c"]]); m.put(3, A, [3, 30, "c"])
    # put_cell before the first put: the put wins
    db.put_cell(R, 4, 2, 5); m.put_cell(4, 2, 5)
    db.put_dense_batch(R, 4, [[4, 2, "a"]]); m.put(4, A, [4, 2, "a"])
    # put_cell(None) before a put: the put wins; after: the None wins
    db.put_cell(R, 5, 3, None, type="utf8"); m.put_cell(5, 3, None)
    db.put_dense_batch(R, 5, [[5, 0, "a"]]); m.put(5, A, [5, 0, "a"])
    db.put_cell(R, 5, 2, None, type="varint"); m.put_cell(5, 2, None)
    return db, m


def test_f1_examples_of_the_order(tmp_path):
    p = tmp_path / "f1.nhydb"
    db, m = _f1_db(p)
    assert db.get_value(R, 1, 2) == 12 and db.get_value(R, 2, 2) == 20 and db.get_value(R, 3, 2) == 30
    assert db.get_value(R, 3, V_EXT) is None and db.get_value(R, 4, 2) == 2
    assert db.get_cell(R, 5, 2, dyn_address=False)["present"] is True and db.get_value(R, 5, 3) == "a"
    us = range(1, 7)
    check(db, m, us, "uncommitted")
    db.commit()
    check(db, m, us, "committed")
    db.build_indexes(commit=True)
    check(db, m, us, "index built")
    db.close()
    db = nhydb.NHyDB(str(p))
    check(db, m, us, "reopened")
    db.close()
    nhydb.NHyDB.compact_file(str(p), make_backup=False)
    db = nhydb.NHyDB(str(p))
    check(db, m, us, "compacted")
    assert db.verify(deep=True)["ok"]
    db.close()


def test_compaction_never_revives_and_never_loses(tmp_path):
    """Compaction copies the DENSE records and the surviving SPARSE states
    in chunks; a surviving state must land after its record (higher block
    id), a hidden one must be dropped – also across chunk boundaries."""
    p = tmp_path / "c.nhydb"
    db = nhydb.NHyDB.create(str(p))
    _schema(db, index=False)
    m = Model()
    n = 60_000                                   # > one sparse chunk of the compaction
    db.put_numpy_batch(R, 1, {"id": list(range(n)), "x": [1] * n, "y": ["a"] * n}, shape_id=1)
    for u in range(1, n + 1):
        m.put(u, [1, 2, 3], [u - 1, 1, "a"])
    # every u: x = 7, newer than its record – 60 000 surviving states, more
    # than one sparse chunk of the compaction (50 000), so a chunk is full
    # while a dense batch is still pending
    db.put_points(R, [(u, 2, 7, "varint") for u in range(1, n + 1)])
    for u in range(1, n + 1):
        m.put_cell(u, 2, 7)
    db.put_dense_batch(R, 1, [[0, 1, "a"]] * 100, shape_id=1)          # u 1..100 rewritten: x = 1 again
    for u in range(1, 101):
        m.put(u, [1, 2, 3], [0, 1, "a"])
    db.commit()
    db.close()
    nhydb.NHyDB.compact_file(str(p), make_backup=False)
    db = nhydb.NHyDB(str(p))
    live = m.live()
    for u in (1, 2, 99, 100, 101, 102, 30_001, 59_999, 60_000):
        assert db.get_value(R, u, 2) == live[(u, 2)], u
    xs = db.sql("SELECT x FROM main")["rows"]
    assert sum(1 for r in xs if r["x"] == 7) == sum(1 for (u, v), x in live.items() if v == 2 and x == 7)
    assert db.verify(deep=True)["ok"]
    db.close()


# ----------------------------------------------------------------------
# put replaces the BASE record, put_cell changes one cell
# ----------------------------------------------------------------------

def test_put_replaces_base_record_put_cell_changes_one_cell(tmp_path):
    db = nhydb.NHyDB.create(str(tmp_path / "s.nhydb"))
    _schema(db)
    db.put_dense_batch(R, 1, [[1, 10, "y20"]], shape_id=1)
    db.put_dense_batch(R, 1, [[1, 11]], shape_id=2)          # shape B: no y
    assert db.get_value(R, 1, 2) == 11
    assert db.get_cell(R, 1, 3, dyn_address=False)["present"] is False   # y of the replaced record is gone
    assert db.get(R, 1)["y"] is None
    db.put_dense_batch(R, 2, [[2, 10, "y20"]], shape_id=1)
    db.put_cell(R, 2, 2, 11)                                  # one cell
    assert (db.get_value(R, 2, 2), db.get_value(R, 2, 3)) == (11, "y20")
    # cells outside the new shape follow their own writes
    db.put_dense_batch(R, 3, [[3, 1, "a"]], shape_id=1)
    db.put_cell(R, 3, 3, "late")                              # y written alone after the record
    db.put_cell(R, 3, V_RES, 5, type="varint")                # reserved BASE position
    db.put_cell(R, 3, V_EXT, 6)                               # extension
    db.put_dense_batch(R, 3, [[3, 2]], shape_id=2)
    assert db.get_value(R, 3, 3) == "late" and db.get(R, 3)["y"] == "late"
    assert db.get_value(R, 3, V_RES) == 5 and db.get_value(R, 3, V_EXT) == 6
    # an overwritten value never shows again: y=7 alone, then the record
    # (y=20) hides it, then shape B replaces the record
    db.put_cell(R, 4, 3, "seven")
    db.put_dense_batch(R, 4, [[4, 1, "twenty"]], shape_id=1)
    db.put_dense_batch(R, 4, [[4, 2]], shape_id=2)
    assert db.get_cell(R, 4, 3, dyn_address=False)["present"] is False
    db.commit()
    db.create_index(2, R, "iy", "y")
    db.build_indexes(commit=True)
    assert db.find_u(R, {"iy": "seven"}) == [] and db.find_u(R, {"iy": "late"}) == [3]
    db.close()


def test_reserved_base_cell_stays_base(tmp_path):
    """v15 (base_reserve_to=20, no name) is BASE: shown by get() also
    without extensions, untouched by put_extensions(merge=False), deleted
    and counted like every cell."""
    db = nhydb.NHyDB.create(str(tmp_path / "r.nhydb"))
    _schema(db)
    db.put_dense_batch(R, 1, [[1, 1, "a"]], shape_id=1)
    db.put_cell(R, 1, V_RES, 3, type="varint")
    db.put_cell(R, 1, V_UNNAMED, "e", type="utf8")
    assert db.get(R, 1, include_extensions=False)[f"v{V_RES}"] == 3
    assert f"v{V_UNNAMED}" not in db.get(R, 1, include_extensions=False)
    db.put_extensions(R, 1, {}, merge=False)                  # drops extensions only
    assert db.get_value(R, 1, V_RES) == 3 and db.get_value(R, 1, V_UNNAMED) is None
    assert db.delete_area(R, 1, 1, V_RES, V_RES) == 1
    assert db.get_value(R, 1, V_RES) is None
    assert db.delete_record(1, 1) == 3
    db.close()


# ----------------------------------------------------------------------
# None is a value
# ----------------------------------------------------------------------

def test_none_cell_states(tmp_path):
    db = nhydb.NHyDB.create(str(tmp_path / "n.nhydb"))
    _schema(db)
    db.create_shape(3, R, "C", SHAPE_A + [{"v": 4, "name": "z", "type": "varint"}])
    db.set_realm_default_shape(R, 3)
    # live cell with a value, live None, tombstoned, never written,
    # field the stored shape lacks
    db.put_dense_batch(R, 1, [[1, 5, "a"]], shape_id=1)
    db.put_cell(R, 1, 3, None, type="utf8")
    states = {
        "value": (1, 2),
        "stored None": (1, 3),
        "never written": (1, V_EXT),
        "missing in stored shape": (1, 4),
    }
    db.put_cell(R, 2, 2, 1)
    assert db.delete_cell(R, 2, 2) is True
    states["tombstoned"] = (2, 2)
    want = {"value": True, "stored None": True, "never written": False, "missing in stored shape": False, "tombstoned": False}
    for name, (u, v) in states.items():
        c = db.get_cell(R, u, v, dyn_address=False)
        assert bool(c and c["present"]) == want[name], (name, c)
    for name, (u, v) in states.items():
        assert db.delete_cell(R, u, v) is want[name], name
        assert db.delete_cell(R, u, v) is False, name         # nothing left to delete
    # a pure SPARSE None cell
    db.put_cell(R, 9, V_UNNAMED, None, type="utf8")
    assert db.sql("SELECT COUNT(*) FROM main")["rows"][0]["count_all"] == 2   # u1 (id), u9
    assert db.delete_cell(R, 9, V_UNNAMED) is True
    assert db.sql("SELECT COUNT(*) FROM main")["rows"][0]["count_all"] == 1
    db.close()


# ----------------------------------------------------------------------
# random histories against the model, through every state
# ----------------------------------------------------------------------

@pytest.mark.parametrize("seed", range(12))
def test_random_history_equals_model(tmp_path, seed):
    rng = random.Random(7100 + seed)
    p = tmp_path / "h.nhydb"
    db = nhydb.NHyDB.create(str(p))
    _schema(db)
    max_u = 40
    us = range(1, max_u + 6)
    m = _random_ops(db, Model(), rng, 120, max_u)
    check(db, m, us, "uncommitted")
    db.commit()
    db.build_indexes(commit=True)
    check(db, m, us, "index built")
    m = _random_ops(db, m, rng, 80, max_u)           # incremental index maintenance (AP9)
    db.commit()
    check(db, m, us, "index maintained")
    db.close()
    db = pq.open_cfg(p, pq.GENERAL)                    # general path instead of the winner view
    check(db, m, us, "general path")
    db.close()
    db = nhydb.NHyDB(str(p))
    check(db, m, us, "reopened")
    db.close()
    nhydb.NHyDB.compact_file(str(p), make_backup=False)
    db = nhydb.NHyDB(str(p))
    check(db, m, us, "compacted")
    m = _random_ops(db, m, rng, 40, max_u)
    db.commit()
    check(db, m, us, "after compaction")
    assert db.verify(deep=True)["ok"]
    db.close()


# ----------------------------------------------------------------------
# delete_area
# ----------------------------------------------------------------------

def _area_db(path):
    db = nhydb.NHyDB.create(str(path))
    _schema(db)
    m = Model()
    for u in range(1, 21):
        db.put_dense_batch(R, u, [[u, u % 3, "a"]], shape_id=1)
        m.put(u, [1, 2, 3], [u, u % 3, "a"])
    for u in range(5, 16):
        db.put_cell(R, u, V_EXT, u % 7); m.put_cell(u, V_EXT, u % 7)
        db.put_cell(R, u, V_UNNAMED, "q", type="utf8"); m.put_cell(u, V_UNNAMED, "q")
    for u in range(30, 35):                              # pure SPARSE records
        db.put_cell(R, u, 2, 1); m.put_cell(u, 2, 1)
    db.put_cell(R, 31, V_RES, None, type="varint"); m.put_cell(31, V_RES, None)
    db.commit()
    return db, m


@pytest.mark.parametrize("area", [
    (50, 60, 1, 100),            # empty
    (7, 7, 2, 2),                # one cell
    (1, 40, 3, 30),              # across the BASE/extension boundary
    (1, 40, 1, 10**6),           # everything: DELETE blocks + tombstones
    (10, 12, 2, 40),             # records partly
    (30, 34, 1, 20),             # pure sparse, the None cell included
])
def test_delete_area_counts_and_state(tmp_path, area):
    p = tmp_path / "a.nhydb"
    db, m = _area_db(p)
    us = range(1, 41)
    n = db.delete_area(R, *area)
    assert n == m.delete_area(*area)
    assert db.delete_area(R, *area) == 0                  # repeated: nothing left
    check(db, m, us, f"area {area}")
    db.commit()
    db.close()
    nhydb.NHyDB.compact_file(str(p), make_backup=False)
    db = nhydb.NHyDB(str(p))
    check(db, m, us, f"area {area} compacted")
    db.close()


def test_delete_area_rejects_bad_bounds_and_math_realm(tmp_path):
    import nhydb.math as nm
    db, _ = _area_db(tmp_path / "b.nhydb")
    for bad in ((5, 4, 1, 2), (1, 2, 5, 4)):
        with pytest.raises(ValueError):
            db.delete_area(R, *bad)
    with pytest.raises(nhydb.RealmLimitError):
        db.delete_area(R, 1, 2, 0, 3)
    with pytest.raises(ValueError):
        db.delete_area(R, 0, 2, 1, 3)
    with pytest.raises(nhydb.SchemaError):
        db.delete_area(99, 1, 2, 1, 3)
    mm = nm.MathMap.compile("mm", ("x",), u="x + 1", V="1", side="0")
    db.create_math_layout(nm.MathMapLayoutSpec("mm", mm, 4), layout_id=9, payload_type="varint")
    mr = db.put_math(9, (1,), 5)["physical_realm"]
    for call in (lambda: db.delete_area(mr, 1, 9, 1, 9), lambda: db.delete_cell(mr, 2, 1)):
        with pytest.raises(nhydb.SchemaError, match="MATH"):
            call()
    assert db.get_math(9, (1,)) == 5
    db.close()


def test_delete_area_is_atomic_on_error(tmp_path):
    """A size limit hit while the tombstones are written: nothing of the
    call stays (the file is cut back), the data is unchanged."""
    p = tmp_path / "t.nhydb"
    db = nhydb.NHyDB.create(str(p))
    _schema(db, index=False)
    n = 120_000
    db.put_points(R, [(u, V_UNNAMED, "v", "utf8") for u in range(1, n + 1)], commit=True)
    size = p.stat().st_size
    db.set_max_db_size(size + 60_000, commit=True)                 # room for less than all tombstones
    size = p.stat().st_size
    with pytest.raises(nhydb.DatabaseSizeLimitError):
        db.delete_area(R, 1, n, 1, 100)
    assert p.stat().st_size == size
    assert db.sql("SELECT COUNT(*) FROM main")["rows"][0]["count_all"] == n
    db.set_max_db_size(10**10, commit=True)
    assert db.delete_area(R, 1, n, 1, 100, commit=True) == n
    assert db.sql("SELECT COUNT(*) FROM main")["rows"][0]["count_all"] == 0
    assert db.verify(deep=True)["ok"]
    db.close()


_RSS = textwrap.dedent("""
    import resource, sys, nhydb
    p, n = sys.argv[1], int(sys.argv[2])
    db = nhydb.NHyDB(p)
    before = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    got = db.delete_area(1, 1, n, 1, 10**6, commit=True)
    after = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    scale = 1 if sys.platform == "darwin" else 1024
    print(got, (after - before) * scale)
""")


@pytest.mark.skipif(sys.platform == "win32", reason="ru_maxrss (resource) is POSIX only")
def test_delete_area_memory_is_bounded(tmp_path):
    """400 000 SPARSE cells and 200 000 records in one area: the peak RSS
    of the delete stays far below a list of all cells (> 60 MB); measured
    2026-11-08 on macOS: about 15 MB (chunks of 8 192 tombstones, no write
    cache for the tombstone blocks; the DELETE entries of 200 000 records
    stay in memory until the directory is flushed, as for every write)."""
    p = tmp_path / "m.nhydb"
    db = nhydb.NHyDB.create(str(p))
    _schema(db, index=False)
    n = 200_000
    db.put_numpy_batch(R, 1, {"id": list(range(n)), "x": [1] * n, "y": ["a"] * n}, shape_id=1)
    db.put_points(R, [(u, V_UNNAMED, "v", "utf8") for u in range(1, n + 1)])
    db.put_points(R, [(u, 2, 3, "varint") for u in range(1, n + 1)])
    db.commit()
    db.close()
    del db
    gc.collect()
    out = subprocess.run([sys.executable, "-c", _RSS, str(p), str(n)], capture_output=True, text=True, check=True).stdout.split()
    deleted, grown = int(out[0]), int(out[1])
    assert deleted == n * 4                                         # id, x, y, v40
    assert grown < 40 * 2**20, grown
    db = nhydb.NHyDB(str(p))
    assert db.sql("SELECT COUNT(*) FROM main")["rows"][0]["count_all"] == 0
    assert db.verify(deep=True)["ok"]
    db.close()


# ----------------------------------------------------------------------
# delete_record in every layout
# ----------------------------------------------------------------------

def _layouts(path):
    db = nhydb.NHyDB.create(str(path))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "N", "type": "varuint"}, {"v": 2, "name": "tp", "type": "varint"}])
    db.create_classic_layout(1, 1, "classic", shape_id=1, base_reserve_to=10)
    db.create_column_layout(2, 1, "col", fields=[{"u": 1000, "name": "N", "type": "varuint"}, {"u": 1001, "name": "tp", "type": "varint"}])
    db.create_packed_layout(3, 1, "pk", u=2000, span=16, fields=[{"offset": 0, "name": "N", "type": "varuint"}, {"offset": 1, "name": "tp", "type": "varint"}])
    db.commit()
    return db


def test_delete_record_counts_live_cells_in_every_layout(tmp_path):
    db = _layouts(tmp_path / "l.nhydb")
    for lid in (1, 2, 3):
        db.put_record(lid, 7, {"N": 7, "tp": 7})
    addr = db.logical_address(2, 7, "tp")
    db.delete_cell(1, addr["u"], addr["v_local"])                 # COLUMN: one field gone
    db.put_cell(1, 7, 2, None)                                     # CLASSIC: tp = None (live)
    assert db.delete_record(1, 7) == 2
    assert db.delete_record(2, 7) == 1
    assert db.delete_record(3, 7) == 2
    for lid in (1, 2, 3):
        assert db.delete_record(lid, 7) == 0
        assert db.get_record(lid, 7) is None
    assert list(db.iter_materialized_cells(1)) == []
    assert db.verify(deep=True)["ok"]
    db.close()


def test_delete_record_custom_plugin_counts_only_live(tmp_path):
    db = _layouts(tmp_path / "c.nhydb")

    class Occ:
        def validate(self, layout):
            pass

        def logical_address(self, layout, object_id, field, lane):
            off = {None: 0, "a": 0, "b": 1}[field]
            return {"u": 100, "v": (object_id - 1) * 2 + off + 1, "lane": lane}

        def type_of(self, layout, address):
            return "varint" if (address["v"] - 1) % 2 == 0 else "utf8"

        def describe(self, layout, address):
            v = address["v_local"]
            return {"record": (v - 1) // 2 + 1, "field": "ab"[(v - 1) % 2], "anchor": (v - 1) % 2 == 0}

        def occupied_addresses(self, db, layout, object_id, lane):
            # names both cells, occupied or not
            return [(100, (object_id - 1) * 2 + off + 1) for off in (0, 1)]

    db.create_custom_layout(4, 1, "occ", plugin="occ", u_start=100, u_end=100,
                            params={"fields": [{"name": "a"}, {"name": "b", "type": "utf8"}]})
    db.register_layout_plugin("occ", Occ())
    db.put_object(4, 3, {"a": -5})
    assert db.delete_record(4, 3) == 1                             # only "a" lived
    assert db.delete_record(4, 3) == 0
    db.close()


def test_math_map_layout_delete_record(tmp_path):
    import nhydb.math as nm
    db = _layouts(tmp_path / "mm.nhydb")
    mm = nm.MathMap.compile("mm", ("x",), u="x + 1", V="1", side="0")
    db.create_math_layout(nm.MathMapLayoutSpec("mm", mm, 4), layout_id=9, payload_type="varint")
    db.put_math(9, (1,), 5)
    with pytest.raises(nhydb.SchemaError, match="MATH"):
        db.delete_area(db.put_math(9, (2,), 6)["physical_realm"], 1, 9, 1, 9)
    # T-05: delete_record itself on the MATH layout – rejected, named as MATH
    for obj in (1, 2):
        with pytest.raises(nhydb.SchemaError, match="MATH layout"):
            db.delete_record(9, obj)
        with pytest.raises(nhydb.SchemaError, match="MATH layout"):
            db.batch().delete_record(9, obj)
    assert db.get_math(9, (1,)) == 5 and db.get_math(9, (2,)) == 6
    db.close()


# ----------------------------------------------------------------------
# the old functions are gone
# ----------------------------------------------------------------------

def test_old_delete_functions_removed(tmp_path):
    db = _layouts(tmp_path / "o.nhydb")
    for name in ("delete", "delete_point"):
        assert not hasattr(db, name), name
    with db.batch() as b:
        for name in ("delete_cell", "delete_record", "delete_area"):
            assert hasattr(b, name), name
    db.close()


# ----------------------------------------------------------------------
# batch
# ----------------------------------------------------------------------

def test_batch_equals_single_calls(tmp_path):
    """The same random operation sequence once as single calls and once in
    batches: same final state, same counters."""
    for seed in range(6):
        rng = random.Random(9300 + seed)
        ops = []
        for _ in range(160):
            k = rng.random()
            u = rng.randint(1, 30)
            if k < 0.35:
                v = rng.choice(VS)
                ops.append(("put_cell", (R, u, v, _val(v, rng)), {"type": TYPES[rng.choice([v])]}))
            elif k < 0.6:
                ops.append(("delete_cell", (R, u, rng.choice(VS)), {}))
            elif k < 0.8:
                ops.append(("delete_record", (1, u), {}))
            else:
                v0 = rng.choice([1, 2, 16, 21])
                ops.append(("delete_area", (R, u, u + rng.choice([0, 3]), v0, v0 + rng.choice([0, 20])), {}))
        dbs = []
        for mode in ("single", "batch"):
            db = nhydb.NHyDB.create(str(tmp_path / f"{mode}{seed}.nhydb"))
            _schema(db)
            db.put_dense_batch(R, 1, [[u, u % 3, "a"] for u in range(1, 31)], shape_id=1)
            db.commit()
            deleted = 0
            if mode == "single":
                for name, args, kw in ops:
                    r = getattr(db, name)(*args, **kw)
                    if name.startswith("delete"):
                        deleted += int(r)
            else:
                i = 0
                while i < len(ops):
                    with db.batch() as b:
                        for name, args, kw in ops[i:i + rng.choice([1, 7, 40])]:
                            getattr(b, name)(*args, **kw)
                            i += 1
                        deleted += b.flush()["deleted"]
            db.commit()
            dbs.append((db, deleted))
        (a, da), (b, dbb) = dbs
        assert da == dbb, seed
        ca = sorted((c["u"], c["v"], repr(c["value"])) for c in a.iter_materialized_cells(R))
        cb = sorted((c["u"], c["v"], repr(c["value"])) for c in b.iter_materialized_cells(R))
        assert ca == cb, seed
        for db, _ in dbs:
            assert db.verify(deep=True)["ok"]
            db.close()


def test_batch_order_put_before_and_after_wide_deletes(tmp_path):
    db, m = _area_db(tmp_path / "o.nhydb")
    with db.batch() as b:
        b.put_cell(R, 7, V_EXT, 1)          # before the area: deleted by it
        b.delete_area(R, 5, 8, 1, 40)
        b.put_cell(R, 6, 2, 9)              # after the area: stays
        b.delete_record(1, 9)
        b.delete_record(1, 9)               # repeated: nothing left
        b.put_cell(R, 9, 3, "back")         # after the record delete: stays
        b.delete_area(R, 30, 31, 1, 20)
        b.delete_area(R, 31, 33, 1, 20)     # overlapping: 31 already gone
        st = b.flush()
    m.put_cell(7, V_EXT, 1)
    n = m.delete_area(5, 8, 1, 40)
    m.put_cell(6, 2, 9)
    n += m.delete_record(9) + m.delete_record(9)
    m.put_cell(9, 3, "back")
    n += m.delete_area(30, 31, 1, 20) + m.delete_area(31, 33, 1, 20)
    assert st["deleted"] == n
    check(db, m, range(1, 41), "batch order")
    db.commit()
    db.close()


def test_batch_many_record_deletes_few_blocks(tmp_path):
    db = nhydb.NHyDB.create(str(tmp_path / "many.nhydb"))
    _schema(db, index=False)
    n = 20_000
    db.put_numpy_batch(R, 1, {"id": list(range(n)), "x": [1] * n, "y": ["a"] * n}, shape_id=1)
    db.put_points(R, [(u, V_EXT, 1, "varuint") for u in range(1, n + 1)])
    db.commit()
    with db.batch(commit=True) as b:
        for u in range(1, n + 1):
            b.delete_record(1, u)
        st = b.flush()
    assert st["deleted"] == 4 * n
    # one DELETE block per record, the tombstones in a handful of SPARSE
    # blocks (directory blocks depend on the configuration and are not counted)
    db.close()
    from test_stab_format_golden import describe_file
    kinds = [b["type"] for b in describe_file(str(tmp_path / "many.nhydb"))["blocks"]]
    assert kinds.count("DELETE") == n
    assert kinds.count("SPARSE_CELLS") <= 1 + 10, kinds.count("SPARSE_CELLS")   # 1 = the extension cells
    db = nhydb.NHyDB(str(tmp_path / "many.nhydb"))
    assert db.sql("SELECT COUNT(*) FROM main")["rows"][0]["count_all"] == 0
    db.close()


def test_batch_error_leaves_nothing(tmp_path):
    p = tmp_path / "e.nhydb"
    db, _ = _area_db(p)
    size = p.stat().st_size
    with pytest.raises(nhydb.SchemaError):
        with db.batch() as b:
            b.delete_record(1, 3)
            b.delete_area(R, 1, 4, 1, 3)
            b.delete_cell(99, 1, 1)         # invalid at the call
    assert p.stat().st_size == size and db.get_value(R, 3, 2) is not None
    db.set_max_db_size(size + 300, commit=True)
    size = p.stat().st_size
    with pytest.raises(nhydb.DatabaseSizeLimitError):
        with db.batch() as b:
            for u in range(1, 21):
                b.delete_record(1, u)       # the limit hits while writing
    assert p.stat().st_size == size
    assert db.get_value(R, 3, 2) is not None
    db.set_max_db_size(10**10, commit=True)
    assert db.verify(deep=True)["ok"]
    db.close()


def test_batch_persistent_after_reopen(tmp_path):
    p = tmp_path / "p.nhydb"
    db, m = _area_db(p)
    with db.batch(commit=True) as b:
        b.delete_cell(R, 1, 2)
        b.delete_record(1, 2)
        b.delete_area(R, 3, 4, 1, 3)
    m.delete_cell(1, 2); m.delete_record(2); m.delete_area(3, 4, 1, 3)
    db.close()
    db = nhydb.NHyDB(str(p))
    check(db, m, range(1, 41), "batch reopened")
    db.close()


def test_batch_delete_after_null_put_deletes(tmp_path):
    """put_cell(None) then delete_cell of the same cell in one batch: the
    None cell lived after the put, so the delete takes effect (as the single
    calls would); before 0.28.2/delete-api-f1 the null cell stayed."""
    db = nhydb.NHyDB.create(str(tmp_path / "n.nhydb"))
    _schema(db, index=False)
    with db.batch() as b:
        b.put_cell(R, 50, V_UNNAMED, None, type="utf8")
        b.delete_cell(R, 50, V_UNNAMED)
        st = b.flush()
    assert st["deleted"] == 1
    assert db.get_cell(R, 50, V_UNNAMED, dyn_address=False) is None
    assert db.sql("SELECT COUNT(*) FROM main")["rows"][0]["count_all"] == 0
    db.close()


def test_compaction_chunk_full_at_pure_sparse_u(tmp_path):
    """Even u: dense records with a newer x; odd u: deleted records with two
    cells written alone afterwards (pure sparse). 90 000 surviving states in
    the order odd, odd, even …: the sparse chunk of the compaction (50 000)
    fills at a pure sparse u while a dense batch is pending – that batch must
    be written first, or the newer x of its records would be hidden."""
    p = tmp_path / "pc.nhydb"
    db = nhydb.NHyDB.create(str(p))
    _schema(db, index=False)
    n = 60_000
    db.put_numpy_batch(R, 1, {"id": list(range(n)), "x": [1] * n, "y": ["a"] * n}, shape_id=1)
    with db.batch() as b:
        for u in range(1, n + 1, 2):
            b.delete_record(1, u)
    db.put_points(R, [(u, 2, 7, "varint") for u in range(2, n + 1, 2)])
    db.put_points(R, [c for u in range(1, n + 1, 2) for c in ((u, 2, 5, "varint"), (u, V_UNNAMED, "p", "utf8"))])
    db.commit()
    db.close()
    nhydb.NHyDB.compact_file(str(p), make_backup=False)
    db = nhydb.NHyDB(str(p))
    xs = [r["x"] for r in db.sql("SELECT x FROM main")["rows"]]
    assert xs.count(7) == n // 2 and xs.count(5) == n // 2, (xs.count(7), xs.count(5), xs.count(1))
    assert db.get_value(R, 1, V_UNNAMED) == "p" and db.get_value(R, 2, 3) == "a"
    assert db.verify(deep=True)["ok"]
    db.close()
