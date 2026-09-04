//! Per-CPU cpuidle residency counters.
//!
//! Fields, per CPU and advertised state (names read from the `name` attribute
//! at probe; on the reference machine: POLL, C1, C2, C3):
//!   `cpu{N}.{state}.time_us`  Counter, Micros — OS-timed residency
//!   `cpu{N}.{state}.usage`    Counter, Count  — entry count
//!
//! These are kernel-timestamped request intervals, not hardware-attested
//! residency (the SMU can demote an IOPORT C3 request below CC6); the engine
//! layer is expected to treat them as an upper bound.

use crate::util;
use drowse_core::{Collector, Field, Kind, Probe, Unit};
use rustix::fd::OwnedFd;
use std::io;
use std::path::Path;

const NAME: &str = "cpuidle";

pub fn probe(root: &Path) -> Probe {
    match CpuIdle::new(root) {
        Ok(c) => Probe::Ready(Box::new(c)),
        Err(e) => util::unavailable(NAME, "sys/devices/system/cpu/*/cpuidle", &e),
    }
}

struct CpuIdle {
    fields: Vec<Field>,
    /// One fd per field, same order (time, usage, time, usage, ...).
    files: Vec<OwnedFd>,
    buf: [u8; 64],
}

impl CpuIdle {
    fn new(root: &Path) -> io::Result<CpuIdle> {
        let base = root.join("sys/devices/system/cpu");
        // Enumerate cpuN directories, sorted numerically (read_dir order is
        // arbitrary and field names must be stable across probes).
        let mut cpus: Vec<u32> = Vec::new();
        for entry in std::fs::read_dir(&base)? {
            let entry = entry?;
            let name = entry.file_name();
            if let Some(n) = name
                .to_str()
                .and_then(|s| s.strip_prefix("cpu"))
                .and_then(|s| s.parse::<u32>().ok())
            {
                cpus.push(n);
            }
        }
        cpus.sort_unstable();

        let mut fields = Vec::new();
        let mut files = Vec::new();
        for n in cpus {
            // States are numbered contiguously from state0.
            for s in 0u32.. {
                let state_dir = base.join(format!("cpu{n}/cpuidle/state{s}"));
                if !state_dir.is_dir() {
                    break;
                }
                let state = std::fs::read_to_string(state_dir.join("name"))?;
                let state = state.trim();
                let dir = util::open_dir(&state_dir)?;
                files.push(util::open_at(&dir, "time")?);
                fields.push(Field {
                    name: format!("cpu{n}.{state}.time_us"),
                    kind: Kind::Counter,
                    unit: Unit::Micros,
                });
                files.push(util::open_at(&dir, "usage")?);
                fields.push(Field {
                    name: format!("cpu{n}.{state}.usage"),
                    kind: Kind::Counter,
                    unit: Unit::Count,
                });
            }
        }
        if fields.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no cpu*/cpuidle/state* entries",
            ));
        }
        Ok(CpuIdle { fields, files, buf: [0; 64] })
    }
}

impl Collector for CpuIdle {
    fn name(&self) -> &'static str {
        NAME
    }

    fn fields(&self) -> &[Field] {
        &self.fields
    }

    fn sample(&mut self, out: &mut [u64]) -> io::Result<()> {
        for (i, fd) in self.files.iter().enumerate() {
            out[i] = util::read_u64_at(fd, &mut self.buf)?;
        }
        Ok(())
    }
}
