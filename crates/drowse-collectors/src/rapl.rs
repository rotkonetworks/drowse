//! AMD package energy via the powercap RAPL bridge
//! (`/sys/class/powercap/intel-rapl:0`, backed by MSR 0xC001_029B on this
//! family).
//!
//! Fields:
//!   `{zone}.energy_uj`            Counter, MicroJoules — wraps at
//!                                 `max_energy_range_uj` (32-bit counter,
//!                                 ~65.5 kJ: ~3.6 h at 5 W)
//!   `{zone}.max_energy_range_uj`  Gauge, MicroJoules — read once at probe,
//!                                 constant; consumers wrap-correct with it
//!
//! `energy_uj` is 0400 root on stock sysfs: an unprivileged probe must come
//! back `Unavailable { PermissionDenied }`, while a missing zone (non-RAPL
//! hardware) is `NotPresent`. The distinction is load-bearing for the CLI's
//! "run me as root to get power" hint.

use crate::util;
use drowse_core::{Collector, Field, Kind, Probe, Unit};
use rustix::fd::OwnedFd;
use std::io;
use std::path::Path;

const NAME: &str = "rapl";

pub fn probe(root: &Path) -> Probe {
    match Rapl::new(root) {
        Ok(c) => Probe::Ready(Box::new(c)),
        Err(e) => util::unavailable(NAME, "sys/class/powercap/intel-rapl:0/energy_uj", &e),
    }
}

struct Rapl {
    fields: Vec<Field>,
    energy: OwnedFd,
    max_range: u64,
    buf: [u8; 32],
}

impl Rapl {
    fn new(root: &Path) -> io::Result<Rapl> {
        let zone_path = root.join("sys/class/powercap/intel-rapl:0");
        let dir = util::open_dir(&zone_path)?; // ENOENT → NotPresent
        let energy = util::open_at(&dir, "energy_uj")?; // EACCES → PermissionDenied

        let mut buf = [0u8; 32];
        let max_fd = util::open_at(&dir, "max_energy_range_uj")?;
        let max_range = util::read_u64_at(&max_fd, &mut buf)?;

        // Human name of the zone ("package-0"); fall back to the directory.
        let zone = std::fs::read_to_string(zone_path.join("name"))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "intel-rapl:0".to_string());

        let fields = vec![
            Field {
                name: format!("{zone}.energy_uj"),
                kind: Kind::Counter,
                unit: Unit::MicroJoules,
            },
            Field {
                name: format!("{zone}.max_energy_range_uj"),
                kind: Kind::Gauge,
                unit: Unit::MicroJoules,
            },
        ];
        Ok(Rapl { fields, energy, max_range, buf })
    }
}

impl Collector for Rapl {
    fn name(&self) -> &'static str {
        NAME
    }

    fn fields(&self) -> &[Field] {
        &self.fields
    }

    fn sample(&mut self, out: &mut [u64]) -> io::Result<()> {
        out[0] = util::read_u64_at(&self.energy, &mut self.buf)?;
        out[1] = self.max_range;
        Ok(())
    }
}
