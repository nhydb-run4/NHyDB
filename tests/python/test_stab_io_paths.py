"""F048 (rest): OS errors on the layout plan or key file name that file in
``OSError.filename`` (not the database path)."""
import os
import sys
from pathlib import Path

import pytest

import nhydb

posix_non_root = pytest.mark.skipif(sys.platform == "win32" or (hasattr(os, "geteuid") and os.geteuid() == 0),
                                    reason="POSIX permissions, not as root")


def _make(p):
    db = nhydb.NHyDB.create(p)
    db.create_realm(1, "x")
    db.close()
    return Path(str(p) + ".layout.enc"), Path(str(p) + ".layout.key")


def test_plan_path_is_directory_names_plan(tmp_path):
    p = tmp_path / "a.nhydb"
    lp, _ = _make(p)
    lp.rename(tmp_path / "plan.bak")
    lp.mkdir()
    with pytest.raises(IsADirectoryError) as ei:
        nhydb.NHyDB(p)
    assert ei.value.filename == str(lp)


@posix_non_root
def test_unreadable_key_names_key_file(tmp_path):
    p = tmp_path / "a.nhydb"
    _, kp = _make(p)
    os.chmod(kp, 0)
    try:
        with pytest.raises(PermissionError) as ei:
            nhydb.NHyDB(p)
        assert ei.value.filename == str(kp)
    finally:
        os.chmod(kp, 0o600)


@posix_non_root
def test_unreadable_plan_names_plan_file(tmp_path):
    p = tmp_path / "a.nhydb"
    lp, _ = _make(p)
    os.chmod(lp, 0)
    try:
        with pytest.raises(PermissionError) as ei:
            nhydb.NHyDB(p)
        assert ei.value.filename == str(lp)
    finally:
        os.chmod(lp, 0o600)


@posix_non_root
@pytest.mark.parametrize("which", ["key_path", "layout_path"])
def test_create_in_readonly_dir_names_side_file(tmp_path, which):
    ro = tmp_path / "ro"
    ro.mkdir()
    target = ro / "side.file"
    os.chmod(ro, 0o500)
    try:
        with pytest.raises(PermissionError) as ei:
            nhydb.NHyDB.create(tmp_path / "b.nhydb", **{which: target})
        assert ei.value.filename == str(target)
    finally:
        os.chmod(ro, 0o700)
    assert not (tmp_path / "b.nhydb").exists()


@posix_non_root
def test_db_errors_still_name_db(tmp_path):
    d = tmp_path / "ro"
    d.mkdir()
    os.chmod(d, 0o500)
    try:
        with pytest.raises(PermissionError) as ei:
            nhydb.NHyDB.create(d / "c.nhydb", layout_path=tmp_path / "c.enc", key_path=tmp_path / "c.key")
        assert Path(ei.value.filename).parent == d  # DB (or its lock file), not a side file
    finally:
        os.chmod(d, 0o700)


@pytest.mark.skipif(os.name != "posix" or (hasattr(os, "geteuid") and os.geteuid() == 0), reason="POSIX permissions")
def test_g2_5_compact_output_error_names_output(tmp_path):
    """G2-5: an OS error at the export target names the output path."""
    p = tmp_path / "a.nhydb"
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(1, "r")
    db.close()
    ro = tmp_path / "ro"
    ro.mkdir()
    ro.chmod(0o555)
    try:
        with pytest.raises(OSError) as ei:
            nhydb.NHyDB.compact_file(str(p), output=str(ro / "out.nhydb"))
        assert ei.value.filename is not None and os.path.dirname(ei.value.filename) == str(ro)
        assert os.path.basename(ei.value.filename).startswith("out.nhydb")
    finally:
        ro.chmod(0o755)
    assert sorted(os.listdir(ro)) == []


@pytest.mark.skipif(os.name != "posix" or (hasattr(os, "geteuid") and os.geteuid() == 0), reason="POSIX permissions")
@pytest.mark.parametrize("which", ["key_path", "layout_path"])
def test_g2_6_failed_create_leaves_no_lock_file(tmp_path, which):
    """G2-6: a failed create removes the lock file it created itself; a
    pre-existing lock file is kept."""
    ro = tmp_path / "ro"
    ro.mkdir()
    ro.chmod(0o555)
    try:
        p = tmp_path / "b.nhydb"
        with pytest.raises(OSError):
            nhydb.NHyDB.create(str(p), **{which: str(ro / "side")})
        assert sorted(os.listdir(tmp_path)) == ["ro"], "no residue (incl. b.nhydb.lock)"
        lock = tmp_path / "b.nhydb.lock"
        lock.write_bytes(b"")
        with pytest.raises(OSError):
            nhydb.NHyDB.create(str(p), **{which: str(ro / "side")})
        assert lock.exists(), "pre-existing lock file is never deleted"
    finally:
        ro.chmod(0o755)
