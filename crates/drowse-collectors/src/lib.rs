//! sysfs collectors. Every collector takes a root path (normally `/`) so the
//! whole crate is testable against fixture directory trees with no hardware.
//!
//! Discipline (see design doc §8): probe opens and keeps every fd; `sample`
//! only ever `pread`s at offset 0 on kept fds — no open, no allocation.
//!
//! M1 scope notes:
//! - amdgpu reads only `gpu_busy_percent`. `gpu_metrics`, `pp_dpm_*` and the
//!   hwmon sensors are deliberately excluded: each read is an SMU mailbox
//!   round-trip that wakes the SMU — the tool must not perturb the package
//!   idle it measures.
//! - RAPL `energy_uj` is root-only on stock sysfs; unprivileged probes report
//!   `Probe::Unavailable` with `PermissionDenied`, which is a value, not a bug.

mod amdgpu;
mod battery;
mod cpuidle;
mod rapl;
mod util;

use drowse_core::Probe;
use std::path::Path;

/// Probe every known collector under `root` (`/` in production).
///
/// Never fails: an unavailable source becomes `Probe::Unavailable` with a
/// typed reason, and the caller reports it verbatim.
///
/// Order is fixed and part of the crate's de-facto contract with tests:
/// cpuidle, battery, rapl, amdgpu.
pub fn probe_all(root: &Path) -> Vec<Probe> {
    vec![
        cpuidle::probe(root),
        battery::probe(root),
        rapl::probe(root),
        amdgpu::probe(root),
    ]
}
