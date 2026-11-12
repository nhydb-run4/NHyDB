"""Shared helpers of the NHyDB KQ-10M benchmark.

* constants of the benchmark protocol
* process memory and CPU time on macOS, Linux and Windows (no extra packages)
* anonymous description of the machine (hardware and software only)
* NHyDB version check and the KQ database schema
"""
import ctypes
import json
import os
import platform
import re
import subprocess
import sys
import time
from pathlib import Path
from types import SimpleNamespace

BENCHMARK_VERSION = "1.0.0"
MIN_NHYDB = "0.28.2"

RECORDS = 10_000_000
BATCH = 10_000
REALM = 1
INDEX_FIELDS = ["lage", "stufe", "tq"]
INDEX_NAME = {f: f"idx_{f}" for f in INDEX_FIELDS}
INDEX_ID = {f: i for i, f in enumerate(INDEX_FIELDS, 1)}
DURABILITY = "fsync"
MIB = 2 ** 20

ns = time.perf_counter_ns


# ----------------------------------------------------------------------
# versions
# ----------------------------------------------------------------------

def version_tuple(text):
    """'0.28.2', '0.28.3.dev1', '0.29.0rc1' -> (0, 28, 2) ..."""
    m = re.match(r"\s*(\d+)\.(\d+)(?:\.(\d+))?", str(text))
    if not m:
        return None
    return tuple(int(x or 0) for x in m.groups())


def nhydb_info():
    import nhydb
    info = {"engine_version": getattr(nhydb, "ENGINE_VERSION", None),
            "file_format": getattr(nhydb, "FILE_FORMAT_VERSION", None)}
    try:
        from importlib.metadata import version
        info["package_version"] = version("nhydb")
    except Exception:  # noqa: BLE001
        info["package_version"] = None
    native = getattr(nhydb, "_native", None)
    info["build_hash"] = getattr(native, "BUILD_SOURCE_HASH", None)
    return info


# ----------------------------------------------------------------------
# process memory and CPU time (peak RSS in bytes on every platform)
# ----------------------------------------------------------------------

if sys.platform == "win32":
    from ctypes import wintypes

    class _PMC(ctypes.Structure):
        _fields_ = [("cb", wintypes.DWORD), ("PageFaultCount", wintypes.DWORD),
                    ("PeakWorkingSetSize", ctypes.c_size_t), ("WorkingSetSize", ctypes.c_size_t),
                    ("QuotaPeakPagedPoolUsage", ctypes.c_size_t), ("QuotaPagedPoolUsage", ctypes.c_size_t),
                    ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t), ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
                    ("PagefileUsage", ctypes.c_size_t), ("PeakPagefileUsage", ctypes.c_size_t)]

    _k32 = ctypes.WinDLL("kernel32", use_last_error=True)
    _k32.GetCurrentProcess.restype = wintypes.HANDLE
    _k32.GetProcessTimes.argtypes = [wintypes.HANDLE] + [ctypes.POINTER(wintypes.FILETIME)] * 4
    _k32.K32GetProcessMemoryInfo.argtypes = [wintypes.HANDLE, ctypes.POINTER(_PMC), wintypes.DWORD]

    def _pmc():
        pmc = _PMC()
        pmc.cb = ctypes.sizeof(_PMC)
        _k32.K32GetProcessMemoryInfo(_k32.GetCurrentProcess(), ctypes.byref(pmc), pmc.cb)
        return pmc

    def _ft(ft):
        return ((ft.dwHighDateTime << 32) | ft.dwLowDateTime) / 1e7

    def usage():
        c, e, k, u = (wintypes.FILETIME() for _ in range(4))
        _k32.GetProcessTimes(_k32.GetCurrentProcess(), ctypes.byref(c), ctypes.byref(e), ctypes.byref(k), ctypes.byref(u))
        return SimpleNamespace(user_s=_ft(u), sys_s=_ft(k), peak_bytes=int(_pmc().PeakWorkingSetSize))

    def rss_bytes():
        return int(_pmc().WorkingSetSize)
else:
    import resource as _resource

    _SCALE = 1 if sys.platform == "darwin" else 1024

    def usage():
        r = _resource.getrusage(_resource.RUSAGE_SELF)
        return SimpleNamespace(user_s=r.ru_utime, sys_s=r.ru_stime, peak_bytes=int(r.ru_maxrss) * _SCALE)

    if sys.platform == "darwin":
        _libc = ctypes.CDLL(None)

        class _TimeValue(ctypes.Structure):
            _fields_ = [("seconds", ctypes.c_int), ("microseconds", ctypes.c_int)]

        class _TaskInfo(ctypes.Structure):
            _fields_ = [("virtual_size", ctypes.c_uint64), ("resident_size", ctypes.c_uint64),
                        ("resident_size_max", ctypes.c_uint64), ("user_time", _TimeValue),
                        ("system_time", _TimeValue), ("policy", ctypes.c_int), ("suspend_count", ctypes.c_int)]

        _TASK = ctypes.c_uint.in_dll(_libc, "mach_task_self_")

        def rss_bytes():
            info = _TaskInfo()
            cnt = ctypes.c_uint(ctypes.sizeof(info) // 4)
            if _libc.task_info(_TASK, 20, ctypes.byref(info), ctypes.byref(cnt)) != 0:
                return None
            return int(info.resident_size)
    else:
        def rss_bytes():
            try:
                with open("/proc/self/statm") as f:
                    return int(f.read().split()[1]) * os.sysconf("SC_PAGE_SIZE")
            except (OSError, ValueError, IndexError):
                return None


def usage_dict():
    u = usage()
    return {"user_s": u.user_s, "sys_s": u.sys_s, "peak_bytes": u.peak_bytes}


# ----------------------------------------------------------------------
# anonymous machine description (no host name, user name, paths or serials)
# ----------------------------------------------------------------------

def _sysctl(name, as_int=False):
    try:
        libc = ctypes.CDLL(None)
        size = ctypes.c_size_t(0)
        if libc.sysctlbyname(name.encode(), None, ctypes.byref(size), None, 0) != 0 or size.value == 0:
            return None
        buf = ctypes.create_string_buffer(size.value)
        if libc.sysctlbyname(name.encode(), buf, ctypes.byref(size), None, 0) != 0:
            return None
        if as_int:
            return int.from_bytes(buf.raw[:size.value], "little")
        return buf.value.decode("utf-8", "replace").strip() or None
    except Exception:  # noqa: BLE001
        return None


def _read(path):
    try:
        return Path(path).read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None


def _system_mac():
    perf, eff = _sysctl("hw.perflevel0.physicalcpu", True), _sysctl("hw.perflevel1.physicalcpu", True)
    cores = _sysctl("hw.physicalcpu", True)
    detail = f"{cores} ({perf} performance + {eff} efficiency)" if perf and eff else (str(cores) if cores else None)
    mem = _sysctl("hw.memsize", True)
    model = _sysctl("hw.model")
    if not model:
        try:
            out = subprocess.run(["ioreg", "-c", "IOPlatformExpertDevice", "-d", "2"], capture_output=True,
                                 text=True, timeout=10).stdout
            m = re.search(r'"model" = <"([^"]+)">', out)
            model = m.group(1) if m else None
        except (OSError, subprocess.SubprocessError):
            pass
    return {"os": f"macOS {platform.mac_ver()[0]}", "cpu": _sysctl("machdep.cpu.brand_string"),
            "machine_model": model, "physical_cores": detail, "memory_bytes": mem}


def _system_linux():
    info = {}
    rel = _read("/etc/os-release") or ""
    m = re.search(r'^PRETTY_NAME="?([^"\n]+)"?', rel, re.M)
    info["os"] = f"{m.group(1) if m else 'Linux'} (kernel {platform.release()})"
    cpuinfo = _read("/proc/cpuinfo") or ""
    m = re.search(r"^model name\s*:\s*(.+)$", cpuinfo, re.M)
    cpu = m.group(1).strip() if m else None
    if not cpu:
        try:
            out = subprocess.run(["lscpu"], capture_output=True, text=True, timeout=10).stdout
            m = re.search(r"^Model name:\s*(.+)$", out, re.M)
            cpu = m.group(1).strip() if m else None
        except (OSError, subprocess.SubprocessError):
            pass
    info["cpu"] = cpu
    cores = {(p, c) for p, c in re.findall(r"physical id\s*:\s*(\d+).*?core id\s*:\s*(\d+)", cpuinfo, re.S)}
    info["physical_cores"] = str(len(cores)) if cores else None
    vendor = (_read("/sys/devices/virtual/dmi/id/sys_vendor") or "").strip()
    product = (_read("/sys/devices/virtual/dmi/id/product_name") or "").strip()
    info["machine_model"] = " ".join(x for x in (vendor, product) if x) or None
    m = re.search(r"^MemTotal:\s*(\d+)\s*kB", _read("/proc/meminfo") or "", re.M)
    info["memory_bytes"] = int(m.group(1)) * 1024 if m else None
    return info


def _system_windows():
    info = {"os": f"Windows {platform.release()} (build {platform.version()})"}
    try:
        import winreg
        with winreg.OpenKey(winreg.HKEY_LOCAL_MACHINE, r"HARDWARE\DESCRIPTION\System\CentralProcessor\0") as k:
            info["cpu"] = winreg.QueryValueEx(k, "ProcessorNameString")[0].strip()
        with winreg.OpenKey(winreg.HKEY_LOCAL_MACHINE, r"HARDWARE\DESCRIPTION\System\BIOS") as k:
            vals = []
            for name in ("SystemManufacturer", "SystemProductName", "SystemFamily"):
                try:
                    vals.append(str(winreg.QueryValueEx(k, name)[0]).strip())
                except OSError:
                    pass
            info["machine_model"] = " ".join(v for v in vals if v) or None
    except OSError:
        info.setdefault("cpu", None)
        info.setdefault("machine_model", None)

    class _MemStatus(ctypes.Structure):
        _fields_ = [("dwLength", ctypes.c_ulong), ("dwMemoryLoad", ctypes.c_ulong),
                    ("ullTotalPhys", ctypes.c_ulonglong), ("ullAvailPhys", ctypes.c_ulonglong),
                    ("ullTotalPageFile", ctypes.c_ulonglong), ("ullAvailPageFile", ctypes.c_ulonglong),
                    ("ullTotalVirtual", ctypes.c_ulonglong), ("ullAvailVirtual", ctypes.c_ulonglong),
                    ("ullAvailExtendedVirtual", ctypes.c_ulonglong)]
    st = _MemStatus()
    st.dwLength = ctypes.sizeof(_MemStatus)
    info["memory_bytes"] = int(st.ullTotalPhys) if ctypes.windll.kernel32.GlobalMemoryStatusEx(ctypes.byref(st)) else None
    info["physical_cores"] = None
    return info


def system_info():
    """Hardware and software of this machine – anonymous by design."""
    try:
        if sys.platform == "darwin":
            info = _system_mac()
        elif sys.platform == "win32":
            info = _system_windows()
        else:
            info = _system_linux()
    except Exception as ex:  # noqa: BLE001
        info = {"os": platform.platform(terse=True), "error": type(ex).__name__}
    info.update({"architecture": platform.machine(), "logical_cpus": os.cpu_count(),
                 "python": f"{platform.python_implementation()} {platform.python_version()}"})
    try:
        import numpy
        info["numpy"] = numpy.__version__
    except ImportError:
        info["numpy"] = None
    return info


# ----------------------------------------------------------------------
# NHyDB database
# ----------------------------------------------------------------------

def create_kq_db(path):
    """New NHyDB database with the KQ shape in a CLASSIC layout (realm 1)."""
    import nhydb
    from kq import LAGE_ENUM, NAMES, TYPES
    db = nhydb.NHyDB.create(path, durability=DURABILITY)
    db.create_realm(REALM, "kq")
    fields = []
    for i, nm in enumerate(NAMES):
        f = {"v": i + 1, "name": nm, "type": TYPES[nm]}
        if nm == "lage":
            f["enum"] = dict(LAGE_ENUM)
        fields.append(f)
    db.create_shape(1, REALM, "KQ", fields)
    db.create_classic_layout(1, REALM, "KQ", shape_id=1)
    db.commit()
    return db


def import_all(db, data, records, batch):
    """Writes all records in batches, one data commit at the end. Returns timings in ns."""
    from kq import NAMES
    prep = write = 0
    for u0 in range(1, records + 1, batch):
        count = min(batch, records - u0 + 1)
        t0 = ns()
        cols = {nm: data[nm][u0 - 1:u0 - 1 + count] for nm in NAMES}
        t1 = ns()
        db.put_numpy_batch(REALM, u0, cols)
        t2 = ns()
        prep += t1 - t0
        write += t2 - t1
    t0 = ns()
    db.commit()
    commit = ns() - t0
    return {"prep_ns": prep, "write_ns": write, "commit_ns": commit, "batches": -(-records // batch)}


def db_files(path):
    """Database file plus any side files (name prefix) -> (count, total bytes, main bytes)."""
    path = Path(path)
    files = [p for p in path.parent.glob(path.name + "*") if p.is_file()]
    return {"files": len(files), "total_bytes": sum(p.stat().st_size for p in files),
            "main_bytes": path.stat().st_size if path.exists() else 0}


def remove_db(path):
    path = Path(path)
    for p in path.parent.glob(path.name + "*"):
        if p.is_file():
            p.unlink()


def write_json(path, obj):
    Path(path).write_text(json.dumps(obj, indent=2, ensure_ascii=False), encoding="utf-8")
