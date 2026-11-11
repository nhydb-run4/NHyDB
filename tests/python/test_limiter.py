"""RUN¹⁰: central RAM limiter of the process (``nhydb.resource_status()``)."""
import json
import subprocess
import sys
import textwrap

import nhydb

KEYS = {
    "enabled", "mode", "zone", "level_bytes", "limit_bytes", "yellow_bytes", "ram_bytes", "ram_cap_bytes",
    "reserve_percent", "warn_percent", "tick_ms", "source", "config_source", "thread_running", "active_jobs",
    "counters", "hints", "error",
}


def test_resource_status_fields():
    s = nhydb.resource_status()
    assert KEYS <= set(s), KEYS - set(s)
    assert s["source"] in ("phys_footprint", "statm_rss", "private_usage")
    assert s["level_bytes"] > 0
    assert 0 < s["yellow_bytes"] < s["limit_bytes"] < s["ram_bytes"]
    assert s["zone"] in ("green", "yellow", "red")
    assert s["error"] is None
    assert {"ticks", "peak_level", "tick_panics"} <= set(s["counters"])


def _run(code, tmp_path):
    r = subprocess.run([sys.executable, "-c", textwrap.dedent(code)], cwd=tmp_path, capture_output=True, text=True, timeout=120)
    assert r.returncode == 0, r.stderr
    return json.loads(r.stdout.strip().splitlines()[-1])


def test_first_handle_defines_the_limiter(tmp_path):
    (tmp_path / "a.toml").write_text("[limiter]\nreserve_percent = 20\nwarn_percent = 4\n")
    (tmp_path / "b.toml").write_text("[limiter]\nreserve_percent = 30\n")
    out = _run(
        """
        import json, nhydb
        a = nhydb.NHyDB.create("a.nhydb", config="a.toml")
        s1 = nhydb.resource_status()
        b = nhydb.NHyDB.create("b.nhydb", config="b.toml")
        s2 = nhydb.resource_status()
        print(json.dumps({"s1": s1, "s2": s2, "verify_b": b.verify()["warnings"]}))
        """,
        tmp_path,
    )
    s1, s2 = out["s1"], out["s2"]
    assert (s1["reserve_percent"], s1["warn_percent"]) == (20, 4)
    assert s1["config_source"].endswith("a.toml")
    assert s1["limit_bytes"] == s1["ram_bytes"] * 80 // 100 or s1["ram_cap_bytes"] is not None
    assert s1["hints"] == []
    assert s2["reserve_percent"] == 20, "the first handle defines the limiter"
    assert len(s2["hints"]) == 1 and "[limiter]" in s2["hints"][0]
    assert any("[limiter]" in w for w in out["verify_b"])


def test_invalid_limiter_settings_are_rejected(tmp_path):
    (tmp_path / "bad.toml").write_text("[limiter]\nreserve_percent = 95\n")
    r = subprocess.run(
        [sys.executable, "-c", "import nhydb; nhydb.NHyDB.create('x.nhydb', config='bad.toml')"],
        cwd=tmp_path, capture_output=True, text=True, timeout=120,
    )
    assert r.returncode != 0 and "reserve_percent" in r.stderr


def test_every_large_path_reaches_its_checkpoints(tmp_path):
    """RUN¹⁰ AP10.2: each path of the plan (§2.5) registers with the
    limiter and reaches checkpoints under a large enough load; afterwards
    no job is left in the register."""
    out = _run(
        """
        import json
        import numpy as np
        import nhydb
        from nhydb.query import Q, QueryEngine
        n = 60000
        db = nhydb.NHyDB.create("p.nhydb")
        db.create_realm(1, "r")
        db.create_shape(1, 1, "s", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "body", "type": "utf8"}])
        db.create_classic_layout(1, 1, "c", shape_id=1)
        db.create_trigram_index(3, 1, "tri_body", 2)
        db.create_fulltext_index(4, 1, "ft_body", 2)
        body = np.array([("word%d alpha beta " % (u % 97)) + "x" * 150 for u in range(1, n + 1)], dtype=object)
        db.put_numpy_batch(1, 1, {"id": np.arange(1, n + 1, dtype=np.uint64), "body": body}, shape_id=1)
        db.commit()
        db.build_indexes(commit=True)
        rows = db.sql("SELECT * FROM c")["rows"]
        assert len(rows) == n
        assert sum(1 for _ in db.sql_iter("SELECT id, body FROM c ORDER BY body DESC")) == n
        qe = QueryEngine(db)
        qe.search(Q.field("body").fulltext("alpha"), realm=1, select=["id"], limit=None)
        assert len(db.sql("SELECT id FROM c WHERE CONTAINS(body, 'pha bet')")["rows"]) == n
        qe.execute(~Q.field("body").fulltext("word5"), realm=1, return_mode="n")
        db.close()
        nhydb.NHyDB.compact_file("p.nhydb", make_backup=False)
        s = nhydb.resource_status()
        print(json.dumps({"paths": s["paths"], "jobs": s["jobs"], "active": s["active_jobs"]}))
        """,
        tmp_path,
    )
    missing = [p for p, c in out["paths"].items() if c["registered"] == 0 or c["checkpoints"] == 0]
    assert not missing, (missing, out["paths"])
    assert out["jobs"] == [] and out["active"] == 0


def test_cancel_job_ends_a_waiting_query(tmp_path):
    """RUN¹⁰ AP10.6: with a tiny RAM setting the younger of two collecting
    queries waits; nhydb.cancel_job() ends it with ResourceLimitError
    (reason cancelled_by_app); the other one completes."""
    (tmp_path / "small.toml").write_text(
        "[limiter]\nram_bytes = 67108864\ncancel_over_limit = false\nprobe_bytes = 0\nnotify_wait_s = 0\n"
    )
    out = _run(
        """
        import json, threading, time
        import numpy as np
        import nhydb
        n = 300000
        db = nhydb.NHyDB.create("w.nhydb", config="small.toml")
        db.create_realm(1, "r")
        db.create_shape(1, 1, "s", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "body", "type": "utf8"}])
        db.create_classic_layout(1, 1, "c", shape_id=1)
        db.put_numpy_batch(1, 1, {"id": np.arange(1, n + 1, dtype=np.uint64),
                                  "body": np.array(["b%07d" % u + "y" * 40 for u in range(n)], dtype=object)}, shape_id=1)
        db.commit()
        res = {}
        def q(name):
            try:
                res[name] = len(db.sql("SELECT * FROM c")["rows"])
            except nhydb.ResourceLimitError as e:
                res[name] = ("ResourceLimitError", e.reason)
        a = threading.Thread(target=q, args=("a",)); a.start()
        time.sleep(0.05)
        b = threading.Thread(target=q, args=("b",)); b.start()
        cancelled, seen_long = None, False
        end = time.time() + 60
        while (a.is_alive() or b.is_alive()) and time.time() < end:
            for j in nhydb.resource_status()["jobs"]:
                if j["state"] == "waiting" and cancelled is None:
                    seen_long = j["waiting_long"]
                    cancelled = (j["id"], nhydb.cancel_job(j["id"]))
            time.sleep(0.001)
        a.join(); b.join()
        s = nhydb.resource_status()
        print(json.dumps({"res": res, "cancelled": cancelled, "unknown": nhydb.cancel_job(10**12),
                          "seen_long": seen_long, "jobs": s["jobs"], "zone_seen": s["counters"]["red_ms"] > 0}))
        """,
        tmp_path,
    )
    assert out["cancelled"] is not None and out["cancelled"][1] is True, out
    assert out["unknown"] is False
    assert sorted(map(str, out["res"].values())) == sorted(["300000", "['ResourceLimitError', 'cancelled_by_app']"]), out["res"]
    assert out["seen_long"] is True, "notify_wait_s = 0 marks every wait as long"
    assert out["jobs"] == []


def test_fork_child_gets_its_own_monitor(tmp_path):
    """RUN¹⁰ AP10.6: a fork() child inherits no threads; its first large
    job starts its own monitor thread with fresh counters, without a hang;
    the parent is not touched."""
    import os
    import pytest
    if not hasattr(os, "fork"):
        pytest.skip("no fork() on this platform")
    out = _run(
        """
        import json, os, time, warnings
        import numpy as np
        import nhydb
        warnings.simplefilter("ignore", DeprecationWarning)  # fork with threads
        n = 60000
        db = nhydb.NHyDB.create("f.nhydb")
        db.create_realm(1, "r")
        db.create_shape(1, 1, "s", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "body", "type": "utf8"}])
        db.create_classic_layout(1, 1, "c", shape_id=1)
        db.put_numpy_batch(1, 1, {"id": np.arange(1, n + 1, dtype=np.uint64),
                                  "body": np.array(["x" * 60] * n, dtype=object)}, shape_id=1)
        db.commit()
        assert len(db.sql("SELECT * FROM c")["rows"]) == n      # parent: monitor runs
        p0 = nhydb.resource_status()
        r, w = os.pipe()
        pid = os.fork()
        if pid == 0:
            try:
                c0 = nhydb.resource_status()
                rows = len(db.sql("SELECT * FROM c")["rows"])
                c1 = nhydb.resource_status()
                os.write(w, json.dumps({"rows": rows, "before": c0["thread_running"], "after": c1["thread_running"],
                                        "starts": c1["counters"]["thread_starts"], "jobs_reg": c1["counters"]["jobs_registered"]}).encode())
            finally:
                os._exit(0)
        os.close(w)
        end = time.time() + 60
        while time.time() < end:
            done, status = os.waitpid(pid, os.WNOHANG)
            if done:
                break
            time.sleep(0.01)
        else:
            os.kill(pid, 9)
            raise SystemExit("child hangs")
        child = json.loads(os.read(r, 65536).decode())
        p1 = nhydb.resource_status()
        print(json.dumps({"child": child, "parent_running": p1["thread_running"],
                          "parent_starts": p1["counters"]["thread_starts"], "parent_starts0": p0["counters"]["thread_starts"]}))
        """,
        tmp_path,
    )
    ch = out["child"]
    assert ch["rows"] == 60000
    assert ch["before"] is False and ch["after"] is True, "the child starts its own thread"
    assert ch["starts"] == 1 and ch["jobs_reg"] >= 1, "fresh counters in the child"
    assert out["parent_running"] is True and out["parent_starts"] == out["parent_starts0"] == 1


def test_limiter_txt_next_to_the_database_and_rotation(tmp_path):
    """RUN¹⁰ AP10.7: closing the last handle writes the started interval
    to limiter.txt next to the database; with a small max_bytes the file is
    archived as a ZIP after a write and begun anew."""
    import zipfile
    (tmp_path / "log.toml").write_text("[limiter_log]\nmax_bytes = 2048\n")
    out = _run(
        """
        import json, nhydb
        for i in range(8):
            db = nhydb.NHyDB.create("l%d.nhydb" % i, config="log.toml", overwrite=True)
            db.create_realm(1, "r")
            db.close()
        print(json.dumps(nhydb.resource_status()["log"]))
        """,
        tmp_path,
    )
    assert out["file"].endswith("limiter.txt") and out["last_error"] is None
    assert out["lines_written"] == 8 and out["rotations"] >= 1, out
    zips = sorted(tmp_path.glob("limiter_*.zip"))
    assert zips, list(tmp_path.iterdir())
    text = ""
    for zp in zips:
        with zipfile.ZipFile(zp) as z:
            assert z.namelist() == ["limiter.txt"]
            part = z.read("limiter.txt").decode()
            assert len(part) >= 2048, "archived only after reaching the threshold"
            text += part
    cur = tmp_path / "limiter.txt"   # absent right after a rotation
    if cur.exists():
        text += cur.read_text()
    lines = [json.loads(l) for l in text.splitlines()]
    assert len(lines) == 8 and all(l["final"] for l in lines)
    assert all(l["db"].endswith("l0.nhydb") for l in lines), "the first database of the process"
    assert {"ts", "from", "interval_s", "pid", "counters", "paths"} <= set(lines[0])
    # off: no file
    (tmp_path / "off").mkdir()
    (tmp_path / "off" / "off.toml").write_text("[limiter_log]\nenabled = false\n")
    _run('import nhydb, json; nhydb.NHyDB.create("o.nhydb", config="off.toml").close(); print("{}")', tmp_path / "off")
    assert not (tmp_path / "off" / "limiter.txt").exists()
