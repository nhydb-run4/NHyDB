"""AP9 S2′ + automatic maintenance: NHyDB renews stale indexes itself.

* property test with maintenance on: after every commit the answers equal
  a full rebuild of a copy, and merge rebuilds really happen;
* bulk commits defer the rebuild to the next smaller commit, the crisis
  bound forces it;
* a failed rebuild never touches the committed data;
* a red RAM limiter defers it (own process: the limiter is per process);
* readers keep their view while the writer rebuilds;
* after a reopen the counter is seeded exactly.
"""
import os
import random
import subprocess
import sys
import textwrap
from pathlib import Path

import pytest

import nhydb

import test_index_overlay as ov


_ENV0 = os.environ.get("NHYDB_CONFIG")


@pytest.fixture(autouse=True)
def _restore_config_env():
    yield
    if _ENV0 is None:
        os.environ.pop("NHYDB_CONFIG", None)
    else:
        os.environ["NHYDB_CONFIG"] = _ENV0


def cfg(path: Path, **kw):
    """Writes the `[index]` settings of a test next to the database. A
    configuration given by NHYDB_CONFIG (e.g. the dir3 runs of the QA) takes
    precedence over that file – its contents are kept and the settings
    appended, and NHYDB_CONFIG points to the combined file for the test."""
    kw.setdefault("maintenance", '"auto"')
    lines = ["[index]"]
    for k, v in kw.items():
        lines.append(f"{k} = {v}")
    path.parent.mkdir(parents=True, exist_ok=True)
    f = path.parent / "nhydb_config.toml"
    base = Path(_ENV0).read_text() + "\n" if _ENV0 else ""
    f.write_text(base + "\n".join(lines) + "\n")
    if _ENV0:
        os.environ["NHYDB_CONFIG"] = str(f)


def maint(db):
    checks = {c["name"]: c for c in db.health()["checks"]}
    return checks["indexes"]["details"]["maintenance"]


def lane0(db):
    lanes = [x for x in maint(db)["lanes"] if x["lane"] == 0]
    return lanes[0] if lanes else {}


def fresh(db, iid):
    st = {i["index_id"]: i for i in db.stats()["indexes"]}
    return st[iid]["lanes"]["NL"]["fresh"]


def big_db(path: Path, n=2000, **kw):
    cfg(path, **kw)
    db = nhydb.NHyDB.create(str(path))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "cat", "type": "varuint"},
                                {"v": 3, "name": "body", "type": "utf8"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.put_dense_batch(1, 1, [[u, u % 7, f"w{u % 11} x{u % 5}"] for u in range(1, n + 1)])
    db.create_index(1, 1, "i_cat", "cat")
    db.create_token_index(2, 1, "t_body", 3)
    db.commit()
    db.build_indexes(commit=True)
    return db


@pytest.mark.parametrize("column", [True, False])
def test_maintained_answers_equal_rebuild(tmp_path, column):
    path = tmp_path / "w" / "ov.nhydb"
    db = ov.setup(path, column)
    db.close()
    cfg(path, rebuild_ratio=0.02, rebuild_min_changes=12, bulk_commit_ratio=1.0, crisis_ratio=1.0)
    db = nhydb.NHyDB(str(path))
    rng = random.Random(7)
    checks = 0
    for step in range(700):
        ov.random_step(db, rng)
        if rng.random() < 0.12:
            db.commit()
            mine = ov.answers(db)
            ro = nhydb.NHyDB(str(path), readonly=True)
            other = ov.answers(ro)
            ro.close()
            want = ov.reference(path, tmp_path, checks)
            assert mine == want, f"step {step}"
            assert other == want, f"step {step}"
            checks += 1
    st = lane0(db)
    assert st["merge_rebuilds"] >= 5, st
    assert st["failures"] == 0, st
    assert "7]" in st["last"] and "full []" in st["last"], st   # FULLTEXT merged as well
    v = db.verify(deep=True)
    assert v["ok"], v["errors"]
    db.close()


def test_bulk_commit_defers_until_smaller_commit(tmp_path):
    db = big_db(tmp_path / "b" / "b.nhydb", rebuild_min_changes=50, bulk_commit_ratio=0.05, crisis_ratio=0.9)
    # one bulk commit: 400 new records (>= 5 % of N)
    db.put_dense_batch(1, 2001, [[u, 3, "bulk"] for u in range(2001, 2401)], commit=True)
    assert not fresh(db, 1)
    assert "deferred: bulk commit" in lane0(db)["last"]
    assert list(db.find_u(1, {"i_cat": 3})) == [u for u in range(1, 2001) if u % 7 == 3] + list(range(2001, 2401))
    # the next small commit renews the indexes
    db.put(1, 5, [5, 6, "small"], commit=True)
    assert fresh(db, 1) and fresh(db, 2)
    st = lane0(db)
    assert st["merge_rebuilds"] == 1 and "merge [1, 2]" in st["last"], st
    assert list(db.index_candidates(2, "token", "bulk", None)) == list(range(2001, 2401))
    db.close()


def test_crisis_bound_forces_rebuild_during_bulk(tmp_path):
    db = big_db(tmp_path / "c" / "c.nhydb", rebuild_min_changes=50, bulk_commit_ratio=0.05, crisis_ratio=0.25)
    db.put_dense_batch(1, 2001, [[u, 3, "bulk"] for u in range(2001, 2701)], commit=True)   # 700 >= 25 % of N
    assert fresh(db, 1)
    assert "(crisis bound)" in lane0(db)["last"]
    db.close()


def test_failed_rebuild_keeps_data_commit(tmp_path):
    path = tmp_path / "f" / "f.nhydb"
    kw = dict(rebuild_min_changes=50, bulk_commit_ratio=1.0, crisis_ratio=1.0)
    db = big_db(path, maintenance='"manual"', **kw)
    for u in range(1, 120):
        db.put_cell(1, u, 2, 6)
    db.commit()                                             # data committed, index stale
    db.set_max_db_size(path.stat().st_size + 2_000, commit=True)   # no room for new trees (~10 KB)
    db.close()
    cfg(path, **kw)
    db = nhydb.NHyDB(str(path))
    db.commit()                                             # maintenance after the commit: the rebuild fails
    st = lane0(db)
    assert st["failures"] == 1 and "DB maximum" in (st["last_error"] or ""), st
    assert not fresh(db, 1)
    want = sorted(set(range(1, 120)) | {u for u in range(120, 2001) if u % 7 == 6})
    assert list(db.find_u(1, {"i_cat": 6})) == want         # overlay answers
    db.close()
    db = nhydb.NHyDB(str(path))                             # the committed data survived
    assert list(db.find_u(1, {"i_cat": 6})) == want
    db.set_max_db_size(10**9, commit=True)
    for u in range(120, 200):
        db.put_cell(1, u, 2, 0)
    db.commit()
    assert fresh(db, 1), lane0(db)
    assert db.verify(deep=True)["ok"]
    db.close()


def test_red_limiter_defers_maintenance(tmp_path):
    # own process: the limiter settings come from the first handle of a process
    # build the database in this process (normal limiter), test in a child
    path = tmp_path / "l" / "l.nhydb"
    db = big_db(path, rebuild_min_changes=50, bulk_commit_ratio=1.0, crisis_ratio=1.0)
    db.close()
    with open(path.parent / "nhydb_config.toml", "a") as f:
        f.write("[limiter]\nram_bytes = 67108864\n")
    child = textwrap.dedent(f"""
        import sys
        sys.path.insert(0, {str(Path(__file__).parent)!r})
        import nhydb, test_index_maintenance as t
        db = nhydb.NHyDB({str(path)!r})
        ballast = bytearray(96 << 20)                       # footprint above the 64 MiB limit
        for u in range(1, 120):
            db.put_cell(1, u, 2, 6)
        db.commit()
        print(t.lane0(db)["last"])
        print(t.fresh(db, 1))
        print(len(list(db.find_u(1, {{"i_cat": 6}}))))
        db.close()
    """)
    env = dict(os.environ)
    if _ENV0:
        env["NHYDB_CONFIG"] = str(path.parent / "nhydb_config.toml")
    r = subprocess.run([sys.executable, "-c", child], capture_output=True, text=True, timeout=120, env=env)
    assert r.returncode == 0, r.stderr
    last, is_fresh, n = r.stdout.strip().splitlines()[-3:]
    assert "deferred: RAM limiter red" in last, r.stdout
    assert is_fresh == "False"
    assert int(n) == len(set(range(1, 120)) | {u for u in range(120, 2001) if u % 7 == 6})


def test_readers_keep_their_view_during_rebuilds(tmp_path):
    path = tmp_path / "r" / "r.nhydb"
    db = big_db(path, rebuild_min_changes=50, bulk_commit_ratio=1.0, crisis_ratio=1.0)
    ro = nhydb.NHyDB(str(path), readonly=True)
    before = list(ro.find_u(1, {"i_cat": 6}))
    for u in range(1, 200):
        db.put_cell(1, u, 2, 6)
    db.commit()                                             # rebuild by the writer
    assert fresh(db, 1)
    assert list(ro.find_u(1, {"i_cat": 6})) == before       # old trees stay valid for the reader
    ro.close()
    ro = nhydb.NHyDB(str(path), readonly=True)
    assert list(ro.find_u(1, {"i_cat": 6})) == sorted(set(range(1, 200)) | set(before))
    ro.close()
    db.close()


def test_reopen_seeds_the_counter(tmp_path):
    path = tmp_path / "o" / "o.nhydb"
    db = big_db(path, rebuild_min_changes=100, bulk_commit_ratio=1.0, crisis_ratio=1.0, maintenance='"manual"')
    for u in range(1, 150):
        db.put_cell(1, u, 2, 5)
    db.commit()                                             # manual: stays stale
    db.close()
    cfg(path, rebuild_min_changes=100, bulk_commit_ratio=1.0, crisis_ratio=1.0)
    db = nhydb.NHyDB(str(path))
    assert not fresh(db, 1)
    db.put(1, 3, [3, 1, "z"], commit=True)                  # first commit after the open: exact count
    assert fresh(db, 1), lane0(db)
    db.close()


def test_new_index_is_built_by_build_indexes_not_twice(tmp_path):
    path = tmp_path / "n" / "n.nhydb"
    db = big_db(path, rebuild_min_changes=50)
    db.create_index(3, 1, "i_id", "id")
    db.commit()                                             # the defining commit does not build it
    assert lane0(db).get("rebuilds", 0) == 0
    db.build_indexes([3], commit=True)
    assert fresh(db, 3)
    db.close()
