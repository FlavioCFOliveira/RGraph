use std::fs;
use std::path::Path;

/// Information about the NUMA topology of the machine.
#[derive(Debug, Clone)]
pub struct NumaTopology {
    /// Number of NUMA nodes detected.
    pub node_count: usize,
    /// For each node, the list of CPUs that are local to it.
    pub cpus_per_node: Vec<Vec<usize>>,
}

impl NumaTopology {
    /// Detect NUMA topology by parsing `/sys/devices/system/node`.
    ///
    /// If the sysfs directory is missing or contains only a single node,
    /// returns a single-node topology (non-NUMA mode).
    pub fn detect() -> Self {
        let node_dir = Path::new("/sys/devices/system/node");
        if !node_dir.exists() {
            return Self::single_node();
        }

        let mut nodes = Vec::new();
        let mut cpus_per_node: Vec<Vec<usize>> = Vec::new();

        if let Ok(entries) = fs::read_dir(node_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if let Some(id_str) = name_str.strip_prefix("node") {
                    if let Ok(node_id) = id_str.parse::<usize>() {
                        let cpulist_path = entry.path().join("cpulist");
                        let cpus = if cpulist_path.exists() {
                            parse_cpulist(&fs::read_to_string(&cpulist_path).unwrap_or_default())
                        } else {
                            Vec::new()
                        };
                        nodes.push(node_id);
                        cpus_per_node.push(cpus);
                    }
                }
            }
        }

        nodes.sort_unstable();

        if nodes.len() <= 1 {
            return Self::single_node();
        }

        // Re-order cpus_per_node to match sorted node ids.
        let mut ordered = vec![Vec::new(); nodes.len()];
        for (idx, &node_id) in nodes.iter().enumerate() {
            ordered[idx] = cpus_per_node[node_id].clone();
        }

        Self {
            node_count: nodes.len(),
            cpus_per_node: ordered,
        }
    }

    fn single_node() -> Self {
        Self {
            node_count: 1,
            cpus_per_node: vec![Vec::new()],
        }
    }

    /// Returns true if the machine has more than one NUMA node.
    pub fn is_numa(&self) -> bool {
        self.node_count > 1
    }

    /// Map a CPU index to its NUMA node.
    pub fn node_for_cpu(&self, cpu: usize) -> usize {
        for (node_id, cpus) in self.cpus_per_node.iter().enumerate() {
            if cpus.contains(&cpu) {
                return node_id;
            }
        }
        0
    }
}

/// Parse a Linux cpulist string (e.g. "0-3,5,7-9") into a sorted Vec of CPU numbers.
fn parse_cpulist(s: &str) -> Vec<usize> {
    let mut cpus = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some(dash) = part.find('-') {
            let start = part[..dash].parse::<usize>().unwrap_or(0);
            let end = part[dash + 1..].parse::<usize>().unwrap_or(start);
            for c in start..=end {
                cpus.push(c);
            }
        } else if let Ok(c) = part.parse::<usize>() {
            cpus.push(c);
        }
    }
    cpus.sort_unstable();
    cpus.dedup();
    cpus
}

/// Allocate `size` bytes aligned to `alignment` and bound to `node_id`.
///
/// On Linux with NUMA support this uses `posix_memalign` + `mbind(MPOL_BIND)`.
/// On non-Linux or when `mbind` is unavailable, returns `None` so the caller
/// can fall back to the standard aligned allocator.
pub fn alloc_numa_aligned(size: usize, alignment: usize, node_id: usize) -> Option<*mut u8> {
    #[cfg(target_os = "linux")]
    {
        if let Some(ptr) = alloc_posix_memalign(size, alignment) {
            if mbind_to_node(ptr, size, node_id) {
                return Some(ptr);
            }
            // mbind failed — free with libc::free and fall through.
            unsafe { libc::free(ptr as *mut libc::c_void) };
        }
    }
    let _ = (size, alignment, node_id);
    None
}

/// Linux kernel constants for memory policy (not exposed by all libc
/// versions, so we define them here).
#[cfg(target_os = "linux")]
mod linux_mpol {
    pub const MPOL_DEFAULT: libc::c_int = 0;
    pub const MPOL_PREFERRED: libc::c_int = 1;
    pub const MPOL_BIND: libc::c_int = 2;
    pub const MPOL_INTERLEAVE: libc::c_int = 3;
    pub const MPOL_LOCAL: libc::c_int = 4;

    pub const MPOL_MF_STRICT: libc::c_uint = 1 << 0;
    pub const MPOL_MF_MOVE: libc::c_uint = 1 << 1;
}

#[cfg(target_os = "linux")]
fn alloc_posix_memalign(size: usize, alignment: usize) -> Option<*mut u8> {
    let mut ptr: *mut libc::c_void = std::ptr::null_mut();
    let res = unsafe { libc::posix_memalign(&mut ptr, alignment, size) };
    if res != 0 {
        return None;
    }
    Some(ptr as *mut u8)
}

#[cfg(target_os = "linux")]
fn mbind_to_node(ptr: *mut u8, size: usize, node_id: usize) -> bool {
    use linux_mpol::*;
    let mut nodemask: libc::c_ulong = 0;
    if node_id < (std::mem::size_of::<libc::c_ulong>() * 8) {
        nodemask = 1 << node_id;
    }
    let maxnode = (node_id + 2).min(libc::c_int::MAX as usize) as libc::c_ulong;
    let mode = MPOL_BIND;
    let flags = MPOL_MF_STRICT | MPOL_MF_MOVE;
    let res = unsafe {
        libc::syscall(
            libc::SYS_mbind,
            ptr as *mut libc::c_void,
            size,
            mode,
            &nodemask as *const libc::c_ulong,
            maxnode,
            flags,
        )
    };
    res == 0
}

fn align_up(addr: usize, align: usize) -> usize {
    (addr + align - 1) & !(align - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_node_topology_is_not_numa() {
        let topo = NumaTopology::single_node();
        assert!(!topo.is_numa());
        assert_eq!(topo.node_count, 1);
    }

    #[test]
    fn parse_cpulist_simple() {
        assert_eq!(parse_cpulist("0-3"), vec![0, 1, 2, 3]);
    }

    #[test]
    fn parse_cpulist_mixed() {
        assert_eq!(parse_cpulist("0-1,3,5-7"), vec![0, 1, 3, 5, 6, 7]);
    }

    #[test]
    fn fallback_allocation_returns_none() {
        // alloc_numa_aligned only returns a pointer on Linux when mbind
        // succeeds; on non-Linux it returns None.
        let ptr = alloc_numa_aligned(4096, 4096, 0);
        // On this build platform (Linux aarch64) it may succeed or fail
        // depending on NUMA availability.  We just verify it does not panic.
        if let Some(p) = ptr {
            // Write a byte to verify the mapping is writable.
            unsafe { p.write_volatile(0xAB) };
            assert_eq!(unsafe { p.read_volatile() }, 0xAB);
            unsafe { libc::free(p as *mut libc::c_void) };
        }
    }
}
