//! eBPF wakeup attribution.
//!
//! Design (see doc §6): an in-kernel per-CPU context stack pushed/popped by
//! timer/irq/softirq/workqueue tracepoints; at `sched:sched_waking` one count
//! lands in an LRU per-CPU hash keyed {kind, id}, tagged with whether it broke
//! an idle CPU and whether ALL cpus were idle (a package-sleep break). Nothing
//! crosses to userspace per-event; [`Attributor::drain`] reads the maps in
//! batch on a slow cadence. Binding: libbpf-rs CO-RE (decision register).
//!
//! STATUS: stub. `probe` reports the subsystem unavailable until the eBPF
//! programs and loader land, so the daemon builds and `offenders.get` returns
//! an honest "not implemented yet" rather than a fake table.

use drowse_api::{SourceState, SourceStatus};
use drowse_core::AttributorProbe;

/// Probe the attribution subsystem. Requires CAP_BPF and a BTF kernel.
pub fn probe() -> AttributorProbe {
    AttributorProbe::Unavailable(SourceStatus::err(
        SourceState::NotPresent,
        "eBPF attribution not implemented yet (M2)",
    ))
}
