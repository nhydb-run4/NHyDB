//! RUN¹⁰ AP10.1: water level of the process (its own memory footprint) and
//! the physical RAM, per platform.
//!
//! | platform | water level | why |
//! |---|---|---|
//! | macOS | `task_info(TASK_VM_INFO).phys_footprint` | what the system charges the process (resident + compressed, incl. memory the allocator keeps), 1.7 µs |
//! | Linux | resident set from `/proc/self/statm` | NHyDB reads with `pread`, no file mappings: RSS holds no file pages; the page cache stays outside |
//! | Windows | `PrivateUsage` (commit charge) of `GetProcessMemoryInfo` | cannot be hidden by working-set trimming; returns to the base at once after a free (AP10.0 measurement), 0.7 µs |
//!
//! Under Linux the RAM side is additionally bounded by the cgroup v2 limit
//! of the process (`memory.max`), if one is set. The water level stays the
//! RSS of the process (decision AP10.0 M-K).

/// Which source the water level comes from (for the status).
#[cfg(target_os = "macos")]
pub const SOURCE: &str = "phys_footprint";
#[cfg(target_os = "linux")]
pub const SOURCE: &str = "statm_rss";
#[cfg(windows)]
pub const SOURCE: &str = "private_usage";
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub const SOURCE: &str = "unsupported";

#[cfg(target_os = "macos")]
mod imp {
    /// `task_vm_info_data_t` up to `phys_footprint` (rev1 layout) plus
    /// room for the later revisions.
    #[repr(C)]
    struct TaskVmInfo {
        virtual_size: u64,
        region_count: i32,
        page_size: i32,
        resident_size: u64,
        resident_size_peak: u64,
        device: u64,
        device_peak: u64,
        internal: u64,
        internal_peak: u64,
        external: u64,
        external_peak: u64,
        reusable: u64,
        reusable_peak: u64,
        purgeable_volatile_pmap: u64,
        purgeable_volatile_resident: u64,
        purgeable_volatile_virtual: u64,
        compressed: u64,
        compressed_peak: u64,
        compressed_lifetime: u64,
        phys_footprint: u64,
        rest: [u64; 32],
    }
    const TASK_VM_INFO: i32 = 22;
    /// natural_t words up to and including `phys_footprint`
    /// (`TASK_VM_INFO_REV1_COUNT`); an older kernel answers fewer.
    const REV1_WORDS: u32 = ((std::mem::offset_of!(TaskVmInfo, phys_footprint) + 8) / 4) as u32;
    unsafe extern "C" {
        static mach_task_self_: u32;
        fn task_info(task: u32, flavor: i32, info: *mut TaskVmInfo, count: *mut u32) -> i32;
    }

    pub fn footprint() -> Option<u64> {
        // SAFETY: plain C structure, zero is a valid value of every field
        let mut i: TaskVmInfo = unsafe { std::mem::zeroed() };
        let mut c = (std::mem::size_of::<TaskVmInfo>() / 4) as u32;
        // SAFETY: `i` is large enough for `c` words; `mach_task_self_` is the
        // port of the own task, valid for the life of the process
        let r = unsafe { task_info(mach_task_self_, TASK_VM_INFO, &mut i, &mut c) };
        (r == 0 && c >= REV1_WORDS).then_some(i.phys_footprint)
    }

    pub fn phys_ram() -> Option<u64> {
        let mut v = 0u64;
        let mut l = std::mem::size_of::<u64>();
        // SAFETY: `hw.memsize` is a 64-bit value, `v`/`l` match it
        let r = unsafe { libc::sysctlbyname(c"hw.memsize".as_ptr(), (&mut v as *mut u64).cast(), &mut l, std::ptr::null_mut(), 0) };
        (r == 0 && v > 0).then_some(v)
    }

    pub fn ram_cap() -> Option<u64> {
        None
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::fs::File;
    use std::os::unix::fs::FileExt;
    use std::sync::OnceLock;

    /// `/proc/self/statm`, opened once and read with `pread`. After a
    /// `fork()` the child shares the descriptor, but `/proc/self` was
    /// resolved at open: the child reopens (see `statm`).
    static STATM: OnceLock<(u32, Option<File>)> = OnceLock::new();

    fn page_size() -> u64 {
        // SAFETY: plain query
        let p = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if p > 0 { p as u64 } else { 4096 }
    }

    pub(super) fn parse_rss(buf: &[u8]) -> Option<u64> {
        let s = std::str::from_utf8(buf).ok()?;
        s.split_ascii_whitespace().nth(1)?.parse::<u64>().ok()
    }

    pub fn footprint() -> Option<u64> {
        let pid = std::process::id();
        let (opened_by, f) = STATM.get_or_init(|| (pid, File::open("/proc/self/statm").ok()));
        let mut buf = [0u8; 128];
        let n = if *opened_by == pid {
            f.as_ref()?.read_at(&mut buf, 0).ok()?
        } else {
            // forked child: `/proc/self` of the parent was opened
            File::open("/proc/self/statm").ok()?.read_at(&mut buf, 0).ok()?
        };
        Some(parse_rss(&buf[..n])? * page_size())
    }

    pub fn phys_ram() -> Option<u64> {
        // SAFETY: plain queries
        let (n, p) = unsafe { (libc::sysconf(libc::_SC_PHYS_PAGES), libc::sysconf(libc::_SC_PAGESIZE)) };
        (n > 0 && p > 0).then(|| n as u64 * p as u64)
    }

    /// cgroup v2 limit of the process (`memory.max`), if set.
    pub fn ram_cap() -> Option<u64> {
        let cg = std::fs::read_to_string("/proc/self/cgroup").ok()?;
        let path = cg.lines().find_map(|l| l.strip_prefix("0::"))?;
        let max = std::fs::read_to_string(format!("/sys/fs/cgroup{}/memory.max", path.trim_end())).ok()?;
        max.trim().parse::<u64>().ok()
    }

}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    pub fn footprint() -> Option<u64> {
        // SAFETY: plain C structure; the size is passed in `cb`
        let mut c: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
        c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        // SAFETY: the pseudo handle of the own process needs no closing
        let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), (&mut c as *mut PROCESS_MEMORY_COUNTERS_EX).cast::<PROCESS_MEMORY_COUNTERS>(), c.cb) };
        (ok != 0).then_some(c.PrivateUsage as u64)
    }

    pub fn phys_ram() -> Option<u64> {
        // SAFETY: plain C structure; `dwLength` set as required
        let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        // SAFETY: valid pointer to an initialised structure
        let ok = unsafe { GlobalMemoryStatusEx(&mut m) };
        (ok != 0 && m.ullTotalPhys > 0).then_some(m.ullTotalPhys)
    }

    pub fn ram_cap() -> Option<u64> {
        None
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
mod imp {
    pub fn footprint() -> Option<u64> {
        None
    }
    pub fn phys_ram() -> Option<u64> {
        None
    }
    pub fn ram_cap() -> Option<u64> {
        None
    }
}

/// Current footprint of the process in bytes (`None` if the platform
/// does not answer).
pub fn footprint() -> Option<u64> {
    imp::footprint()
}

/// Physical RAM in bytes.
pub fn phys_ram() -> Option<u64> {
    imp::phys_ram()
}

/// Additional bound of the RAM side (Linux cgroup v2 `memory.max`).
pub fn ram_cap() -> Option<u64> {
    imp::ram_cap()
}

#[cfg(all(test, nhydb_internal_tests))]
#[path = "footprint_tests.rs"]
mod tests;
