//! Minimal flattened-device-tree (FDT) reader — Phase 16.
//!
//! Real hardware passes the platform's device tree to the kernel in x0 at
//! boot (the Linux boot convention, which QEMU follows for both the `virt`
//! and `sbsa-ref` machines).  We do not need the full tree: just enough to
//! discover the DRAM window (base + size) and the CPU count, which the
//! EL3 monitor needs to answer PSCI CPU_ON sensibly.  Everything else in
//! the Tanix world is fixed at compile time per machine (`machine.rs`).
//!
//! Layout (flattened.device.tree spec): a big-endian header, then the
//! structure block of tagged tokens:
//!
//!   FDT_BEGIN_NODE (1) — node name follows, NUL-padded to 4 bytes
//!   FDT_PROP      (3) — u32 len, u32 nameoff (into the strings block),
//!                       then `len` bytes of property value
//!   FDT_END_NODE   (2)
//!   FDT_NOP        (4)
//!   FDT_END        (9)
//!
//! #address-cells / #size-cells default to 2/2 (both QEMU machines) and
//! are read from the root node's properties.

#![allow(dead_code)]

const FDT_MAGIC: u32 = 0xD00D_FEED;
const FDT_BEGIN_NODE: u32 = 0x1;
const FDT_END_NODE: u32 = 0x2;
const FDT_PROP: u32 = 0x3;
const FDT_NOP: u32 = 0x4;
const FDT_END: u32 = 0x9;

/// A discovered memory window.
#[derive(Clone, Copy, Debug)]
pub struct MemoryRegion {
    pub base: usize,
    pub size: usize,
}

fn rd_be32(p: *const u8) -> u32 {
    unsafe {
        let a = core::ptr::read_volatile(p) as u32;
        let b = core::ptr::read_volatile(p.add(1)) as u32;
        let c = core::ptr::read_volatile(p.add(2)) as u32;
        let d = core::ptr::read_volatile(p.add(3)) as u32;
        (a << 24) | (b << 16) | (c << 8) | d
    }
}

fn rd_be64(p: *const u8) -> u64 {
    (rd_be32(p) as u64) << 32 | rd_be32(unsafe { p.add(4) }) as u64
}

/// Returns the first `/memory@...` node's base + size, or None when the
/// pointer is not a valid FDT (or has no memory node).
pub fn dram_region(dtb: usize) -> Option<MemoryRegion> {
    let base = dtb as *const u8;
    if rd_be32(base) != FDT_MAGIC {
        return None;
    }

    // Header: magic, totalsize, off_dt_struct, off_dt_strings, off_mem_rsvmap,
    // version, last_comp_version, boot_cpuid_phys, size_dt_strings,
    // size_dt_struct.
    let struct_off = rd_be32(unsafe { base.add(8) }) as usize;
    let strings_off = rd_be32(unsafe { base.add(12) }) as usize;

    // Root node's #address-cells / #size-cells (default 2/2 per the spec's
    // "default empty case" — both QEMU machines set 2/2 explicitly).
    let mut addr_cells = 2usize;
    let mut size_cells = 2usize;
    let mut node_name: Option<&[u8]> = None;
    let mut depth = 0usize;
    let mut in_memory_node = false;

    let mut p = struct_off;
    loop {
        let token = rd_be32(unsafe { base.add(p) });
        p += 4;
        match token {
            FDT_BEGIN_NODE => {
                // Node name: bytes until NUL, padded to 4.
                let name_start = p;
                let mut n = 0;
                while unsafe { core::ptr::read_volatile(base.add(p + n)) } != 0 {
                    n += 1;
                }
                let name = unsafe { core::slice::from_raw_parts(base.add(name_start), n) };
                p = (p + n + 4) & !3; // NUL + padding
                if depth == 1 {
                    node_name = Some(name);
                    in_memory_node = name.starts_with(b"memory");
                } else {
                    in_memory_node = false;
                }
                depth += 1;
            }
            FDT_PROP => {
                let len = rd_be32(unsafe { base.add(p) }) as usize;
                let nameoff = rd_be32(unsafe { base.add(p + 4) }) as usize;
                p += 8;
                let val = unsafe { base.add(p) };
                let name = unsafe {
                    let mut n = 0;
                    while core::ptr::read_volatile(base.add(strings_off + nameoff + n)) != 0 {
                        n += 1;
                    }
                    core::slice::from_raw_parts(base.add(strings_off + nameoff), n)
                };
                match (depth, node_name) {
                    (1, Some(nm)) if nm == b"memory" || nm.starts_with(b"memory@") => {
                        if name == b"reg" && len as usize >= 4 * (addr_cells + size_cells) {
                            let mut off = 0;
                            let mut addr = 0u64;
                            for _ in 0..addr_cells {
                                addr = (addr << 32) | rd_be32(unsafe { val.add(off) }) as u64;
                                off += 4;
                            }
                            let mut size = 0u64;
                            for _ in 0..size_cells {
                                size = (size << 32) | rd_be32(unsafe { val.add(off) }) as u64;
                                off += 4;
                            }
                            if size != 0 {
                                return Some(MemoryRegion {
                                    base: addr as usize,
                                    size: size as usize,
                                });
                            }
                        }
                    }
                    (0, _) => match name {
                        b"#address-cells" if len >= 4 => addr_cells = rd_be32(val) as usize,
                        b"#size-cells" if len >= 4 => size_cells = rd_be32(val) as usize,
                        _ => {}
                    },
                    _ => {}
                }
                p = (p + len + 3) & !3;
            }
            FDT_END_NODE => {
                depth -= 1;
                node_name = None;
            }
            FDT_NOP => {}
            FDT_END => break,
            _ => break, // unknown token — give up
        }
    }
    None
}

/// Maximum number of secondary CPUs we can name in the monitor.
pub const MAX_DT_CPUS: usize = 8;

/// Count the `/cpus/cpu@N` nodes; returns at least 1.
pub fn cpu_count(dtb: usize) -> usize {
    let base = dtb as *const u8;
    if rd_be32(base) != FDT_MAGIC {
        return 1;
    }
    let struct_off = rd_be32(unsafe { base.add(8) }) as usize;

    // Depth counts BEGIN_NODEs: root = 1, its children = 2, grandchildren
    // = 3.  `/cpus` sits at depth 2, `cpu@N` at depth 3.
    let mut depth = 0usize;
    let mut in_cpus = false;
    let mut count = 1usize;
    let mut p = struct_off;
    loop {
        let token = rd_be32(unsafe { base.add(p) });
        p += 4;
        match token {
            FDT_BEGIN_NODE => {
                let mut n = 0;
                while unsafe { core::ptr::read_volatile(base.add(p + n)) } != 0 {
                    n += 1;
                }
                let name = unsafe { core::slice::from_raw_parts(base.add(p), n) };
                depth += 1;
                if depth == 2 && name == b"cpus" {
                    in_cpus = true;
                } else if in_cpus && depth == 3 && name.starts_with(b"cpu@") {
                    count += 1;
                }
                p = (p + n + 4) & !3;
            }
            FDT_END_NODE => {
                depth = depth.saturating_sub(1);
                if depth < 2 {
                    in_cpus = false;
                }
            }
            FDT_PROP => {
                let len = rd_be32(unsafe { base.add(p) }) as usize;
                p += 8;
                p = (p + len + 3) & !3;
            }
            FDT_NOP => {}
            FDT_END => break,
            _ => break,
        }
    }
    count.min(MAX_DT_CPUS)
}
