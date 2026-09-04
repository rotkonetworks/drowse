//! sysfs collectors. Every collector takes a root path (normally `/`) so the
//! whole crate is testable against fixture directory trees with no hardware.
//!
//! Discipline (see design doc §8): probe opens and keeps every fd; `sample`
//! only ever `pread`s at offset 0 on kept fds — no open, no allocation.

use drowse_core::Probe;
use std::path::Path;

/// Probe every known collector under `root` (`/` in production).
///
/// Never fails: an unavailable source becomes `Probe::Unavailable` with a
/// typed reason, and the caller reports it verbatim.
pub fn probe_all(root: &Path) -> Vec<Probe> {
    let _ = root;
    Vec::new() // collectors land here: cpuidle, battery, rapl, amdgpu
}
