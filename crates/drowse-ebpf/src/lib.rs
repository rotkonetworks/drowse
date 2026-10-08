//! eBPF wakeup attribution (M2). See design doc §6.
//!
//! An in-kernel per-CPU context stack is pushed/popped by timer/irq/softirq/
//! workqueue raw tracepoints; at `sched:sched_waking` one count lands in an LRU
//! per-CPU hash keyed {kind, id}, tagged with whether it broke an idle CPU and
//! whether ALL cpus were idle (a package-sleep break). Nothing crosses to
//! userspace per event; [`Attributor::drain`] reads the maps on a slow cadence
//! (delete-on-read, so each drain yields the delta since the last) and resolves
//! ids to names. Binding: libbpf-rs CO-RE.
//!
//! The crate always *builds* (the skeleton is compiled from committed C +
//! `vmlinux.h`, needing only clang). It only *loads* at [`probe`] time, behind a
//! root + BTF guard, so a non-root / no-BTF box builds and tests green and the
//! daemon can link this crate unconditionally.

mod skel {
    include!(concat!(env!("OUT_DIR"), "/drowse.skel.rs"));
}

mod symbols;

use std::collections::HashMap;
use std::io;
use std::mem::MaybeUninit;

use drowse_api::{OffenderKind, SourceState, SourceStatus};
use drowse_core::{AttributionDelta, Attributor, AttributorProbe, RawOffender};
use libbpf_rs::skel::{OpenSkel, Skel, SkelBuilder};
use libbpf_rs::{MapCore, MapFlags};

use skel::types;
use skel::{DrowseSkel, DrowseSkelBuilder};
use symbols::KallsymsIndex;

// Kind discriminants, mirrored from the BPF program (and OffenderKind order).
const KIND_PROCESS: u32 = 0;
const KIND_TIMER: u32 = 1;
const KIND_IRQ: u32 = 2;
const KIND_SOFTIRQ: u32 = 3;
const KIND_WORKQUEUE: u32 = 4;

const BTF_VMLINUX: &str = "/sys/kernel/btf/vmlinux";

/// Probe the attribution subsystem. Requires CAP_BPF (root here) and a BTF
/// kernel. An unavailable subsystem is a typed value, never a panic.
pub fn probe() -> AttributorProbe {
    if !std::path::Path::new(BTF_VMLINUX).exists() {
        return AttributorProbe::Unavailable(SourceStatus::err(
            SourceState::NotPresent,
            "no kernel BTF (/sys/kernel/btf/vmlinux); CO-RE attribution needs it",
        ));
    }
    match EbpfAttributor::load() {
        Ok(a) => AttributorProbe::Ready(Box::new(a)),
        Err(LoadError::Permission(detail)) => {
            AttributorProbe::Unavailable(SourceStatus::err(SourceState::PermissionDenied, detail))
        }
        Err(LoadError::NotPresent(detail)) => {
            AttributorProbe::Unavailable(SourceStatus::err(SourceState::NotPresent, detail))
        }
        Err(LoadError::Other(detail)) => {
            AttributorProbe::Unavailable(SourceStatus::err(SourceState::Error, detail))
        }
    }
}

enum LoadError {
    Permission(String),
    NotPresent(String),
    Other(String),
}

impl From<libbpf_rs::Error> for LoadError {
    fn from(e: libbpf_rs::Error) -> Self {
        match e.kind() {
            libbpf_rs::ErrorKind::PermissionDenied => {
                LoadError::Permission(format!("eBPF load denied (need CAP_BPF/root): {e}"))
            }
            libbpf_rs::ErrorKind::Unsupported => {
                LoadError::NotPresent(format!("eBPF attribution unsupported on this kernel: {e}"))
            }
            _ => LoadError::Other(format!("eBPF attribution load failed: {e}")),
        }
    }
}

/// A loaded, attached attribution subsystem.
///
/// Self-referential: `skel` borrows the boxed `OpenObject`. Field order matters
/// — `skel` is declared first so it drops (detaching the programs) before the
/// backing `OpenObject` is freed.
pub struct EbpfAttributor {
    skel: DrowseSkel<'static>,
    // Stable heap address the skeleton borrows; freed after `skel`.
    _obj: Box<MaybeUninit<libbpf_rs::OpenObject>>,
    ksyms: KallsymsIndex,
    irq_names: HashMap<u64, String>,
    /// Observer bucket is a cumulative counter; keep the last sum for deltas.
    prev_observer: u64,
    ncpus: usize,
}

impl EbpfAttributor {
    fn load() -> Result<Self, LoadError> {
        let ncpus = libbpf_rs::num_possible_cpus().map_err(LoadError::from)?;
        let self_tgid = std::process::id();

        let mut obj: Box<MaybeUninit<libbpf_rs::OpenObject>> = Box::new(MaybeUninit::uninit());
        // SAFETY: `obj` is boxed (stable heap address) and, by field order, is
        // dropped only after `skel`. We hand the skeleton a 'static view of it.
        let obj_ref: &'static mut MaybeUninit<libbpf_rs::OpenObject> =
            unsafe { &mut *(obj.as_mut() as *mut MaybeUninit<libbpf_rs::OpenObject>) };

        let mut open = DrowseSkelBuilder::default().open(obj_ref).map_err(LoadError::from)?;
        if let Some(rodata) = open.maps.rodata_data.as_deref_mut() {
            rodata.nr_cpus = ncpus as u32;
            rodata.self_tgid = self_tgid;
        }

        let mut skel = open.load().map_err(LoadError::from)?;
        skel.attach().map_err(LoadError::from)?;

        Ok(EbpfAttributor {
            skel,
            _obj: obj,
            ksyms: KallsymsIndex::load(),
            irq_names: HashMap::new(),
            prev_observer: 0,
            ncpus,
        })
    }

    /// Sum a per-CPU value (`Vec<Vec<u8>>`, one chunk per CPU) into one `oval`.
    fn sum_oval(chunks: &[Vec<u8>]) -> types::oval {
        let mut acc = types::oval::default();
        for chunk in chunks {
            if chunk.len() < std::mem::size_of::<types::oval>() {
                continue;
            }
            // SAFETY: the kernel value is exactly `oval`-sized and repr(C).
            let v: types::oval =
                unsafe { std::ptr::read_unaligned(chunk.as_ptr().cast::<types::oval>()) };
            acc.total += v.total;
            acc.idle_breaks += v.idle_breaks;
            acc.pkg_breaks += v.pkg_breaks;
            if acc.comm[0] == 0 && v.comm[0] != 0 {
                acc.comm = v.comm;
            }
        }
        acc
    }

    fn resolve(&mut self, kind: u32, id: u64, comm: &str) -> Option<(OffenderKind, String, String, Option<u32>)> {
        // Returns (api kind, stable key within kind, display name, pid).
        match kind {
            KIND_PROCESS => {
                let pid = id as u32;
                let name = if comm.is_empty() { format!("pid:{pid}") } else { comm.to_string() };
                Some((OffenderKind::Process, pid.to_string(), name, Some(pid)))
            }
            KIND_TIMER => {
                let name = self.ksyms.resolve(id).unwrap_or_else(|| format!("0x{id:x}"));
                Some((OffenderKind::Timer, name.clone(), name, None))
            }
            KIND_WORKQUEUE => {
                let name = self.ksyms.resolve(id).unwrap_or_else(|| format!("0x{id:x}"));
                Some((OffenderKind::Workqueue, name.clone(), name, None))
            }
            KIND_IRQ => {
                let name = self.irq_name(id);
                Some((OffenderKind::Irq, id.to_string(), name, None))
            }
            KIND_SOFTIRQ => {
                let name = softirq_name(id);
                Some((OffenderKind::Softirq, name.clone(), name, None))
            }
            _ => None,
        }
    }

    fn irq_name(&mut self, irq: u64) -> String {
        if let Some(n) = self.irq_names.get(&irq) {
            return n.clone();
        }
        let path = format!("/sys/kernel/irq/{irq}/actions");
        let name = std::fs::read_to_string(&path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(|s| format!("irq:{irq} ({s})"))
            .unwrap_or_else(|| format!("irq:{irq}"));
        self.irq_names.insert(irq, name.clone());
        name
    }
}

impl Attributor for EbpfAttributor {
    fn drain(&mut self) -> io::Result<AttributionDelta> {
        let to_io = |e: libbpf_rs::Error| io::Error::other(e.to_string());

        // Snapshot keys first, then lookup+delete each (delete-on-read makes the
        // read value itself the delta since the previous drain). Collecting keys
        // up front keeps us off the fragile delete-while-iterating path.
        let offenders_map = &self.skel.maps.offenders;
        let keys: Vec<Vec<u8>> = offenders_map.keys().collect();

        let mut rows: Vec<(u32, u64, types::oval)> = Vec::with_capacity(keys.len());
        for key in &keys {
            let chunks = match offenders_map.lookup_percpu(key, MapFlags::ANY).map_err(to_io)? {
                Some(c) => c,
                None => continue, // evicted between keys() and lookup
            };
            let val = Self::sum_oval(&chunks);
            // Delete-on-read: reset the bucket so the next drain sees only new
            // counts. Ignore NotFound (LRU may have already evicted it).
            let _ = offenders_map.delete(key);
            if val.total == 0 {
                continue;
            }
            if key.len() < std::mem::size_of::<types::okey>() {
                continue;
            }
            // SAFETY: key is exactly `okey`-sized and repr(C).
            let k: types::okey = unsafe { std::ptr::read_unaligned(key.as_ptr().cast::<types::okey>()) };
            rows.push((k.kind, k.id, val));
        }

        let mut offenders = Vec::with_capacity(rows.len());
        for (kind, id, val) in rows {
            let comm = comm_to_string(&val.comm);
            if let Some((api_kind, okey, name, pid)) = self.resolve(kind, id, &comm) {
                offenders.push(RawOffender {
                    kind: api_kind,
                    key: okey,
                    name,
                    pid,
                    wakeups: val.total,
                    idle_breaks: val.idle_breaks,
                    pkg_breaks: val.pkg_breaks,
                });
            }
        }

        // Observer bucket is a cumulative per-CPU counter; delta against last.
        let observer_total = {
            let key = 0u32.to_ne_bytes();
            match self.skel.maps.observer.lookup_percpu(&key, MapFlags::ANY).map_err(to_io)? {
                Some(chunks) => chunks
                    .iter()
                    .filter(|c| c.len() >= 8)
                    .map(|c| u64::from_ne_bytes(c[..8].try_into().unwrap()))
                    .sum(),
                None => 0,
            }
        };
        let observer_wakeups = observer_total.saturating_sub(self.prev_observer);
        self.prev_observer = observer_total;

        let _ = self.ncpus; // kept for future rate/window math
        Ok(AttributionDelta { offenders, observer_wakeups })
    }
}

/// Linux softirq vector names, indexed by `vec_nr` (see `enum`
/// `include/linux/interrupt.h`).
fn softirq_name(vec: u64) -> String {
    const NAMES: [&str; 10] = [
        "HI", "TIMER", "NET_TX", "NET_RX", "BLOCK", "IRQ_POLL", "TASKLET", "SCHED", "HRTIMER", "RCU",
    ];
    match NAMES.get(vec as usize) {
        Some(n) => format!("softirq:{n}"),
        None => format!("softirq:{vec}"),
    }
}

fn comm_to_string(comm: &[i8; 16]) -> String {
    let bytes: &[u8] =
        unsafe { std::slice::from_raw_parts(comm.as_ptr().cast::<u8>(), comm.len()) };
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}
