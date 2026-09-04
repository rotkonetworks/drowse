//! Battery gauges from the first `/sys/class/power_supply/*` whose `type`
//! is `Battery` (reference machine: BATT).
//!
//! Fields (all gauges):
//!   `{supply}.power_now_uw`  MicroWatts — `power_now`, or
//!                            `current_now * voltage_now / 1e6` when the
//!                            driver exposes no `power_now`
//!   `{supply}.capacity_pct`  Percent
//!   `{supply}.discharging`   Bool — `status` == "Discharging"
//!   `{supply}.valid`         Bool — 1 when every read this tick succeeded
//!
//! Battery attributes can vanish mid-run (EC hiccup, unplug on removable
//! packs). A failed read never fails the tick: the last good values are
//! reported and `valid` drops to 0.

use crate::util;
use drowse_core::{Collector, Field, Kind, Latency, Probe, Unit};
use rustix::fd::OwnedFd;
use std::io;
use std::path::Path;

const NAME: &str = "battery";

pub fn probe(root: &Path) -> Probe {
    match Battery::new(root) {
        Ok(c) => Probe::Ready(Box::new(c)),
        Err(e) => util::unavailable(NAME, "sys/class/power_supply/<type=Battery>", &e),
    }
}

enum PowerSrc {
    Direct(OwnedFd),
    Computed { current: OwnedFd, voltage: OwnedFd },
}

struct Battery {
    fields: Vec<Field>,
    status: OwnedFd,
    power: PowerSrc,
    /// Absent on some packs; the field is still exposed and reads 0.
    capacity: Option<OwnedFd>,
    /// Last-good values: [power_uw, capacity_pct, discharging].
    last: [u64; 3],
    buf: [u8; 64],
}

impl Battery {
    fn new(root: &Path) -> io::Result<Battery> {
        let base = root.join("sys/class/power_supply");
        let mut names: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(&base)? {
            let entry = entry?;
            if let Some(s) = entry.file_name().to_str() {
                names.push(s.to_string());
            }
        }
        names.sort_unstable(); // deterministic pick across probes

        for supply in names {
            let dir_path = base.join(&supply);
            let Ok(ty) = std::fs::read_to_string(dir_path.join("type")) else {
                continue;
            };
            if ty.trim() != "Battery" {
                continue;
            }
            let dir = util::open_dir(&dir_path)?;
            let status = util::open_at(&dir, "status")?;
            let power = match util::open_at(&dir, "power_now") {
                Ok(fd) => PowerSrc::Direct(fd),
                Err(e) if e.kind() == io::ErrorKind::NotFound => PowerSrc::Computed {
                    current: util::open_at(&dir, "current_now")?,
                    voltage: util::open_at(&dir, "voltage_now")?,
                },
                Err(e) => return Err(e),
            };
            let capacity = util::open_at(&dir, "capacity").ok();
            let fields = vec![
                Field {
                    name: format!("{supply}.power_now_uw"),
                    kind: Kind::Gauge,
                    unit: Unit::MicroWatts,
                },
                Field {
                    name: format!("{supply}.capacity_pct"),
                    kind: Kind::Gauge,
                    unit: Unit::Percent,
                },
                Field {
                    name: format!("{supply}.discharging"),
                    kind: Kind::Gauge,
                    unit: Unit::Bool,
                },
                Field {
                    name: format!("{supply}.valid"),
                    kind: Kind::Gauge,
                    unit: Unit::Bool,
                },
            ];
            return Ok(Battery { fields, status, power, capacity, last: [0; 3], buf: [0; 64] });
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no power_supply with type Battery",
        ))
    }

    fn read_power(&mut self) -> Option<u64> {
        match &self.power {
            PowerSrc::Direct(fd) => util::read_u64_at(fd, &mut self.buf).ok(),
            PowerSrc::Computed { current, voltage } => {
                let c = util::read_u64_at(current, &mut self.buf).ok()?; // µA
                let v = util::read_u64_at(voltage, &mut self.buf).ok()?; // µV
                c.checked_mul(v).map(|p| p / 1_000_000) // µW
            }
        }
    }
}

impl Collector for Battery {
    fn name(&self) -> &'static str {
        NAME
    }

    fn fields(&self) -> &[Field] {
        &self.fields
    }

    /// Battery attributes are EC/ACPI-backed: one `power_now` pread costs
    /// ~4.5 ms and wakes the embedded controller (measured on the reference
    /// machine — it blew the daemon's wakeup budget when read every tick).
    /// `Slow` moves it to the daemon's slow lane, off the fast sampler thread,
    /// which governs its cadence — so no `cadence()` override is needed here.
    fn latency(&self) -> Latency {
        Latency::Slow
    }

    /// Infallible by design: degradation is data (`valid` = 0), not an error.
    fn sample(&mut self, out: &mut [u64]) -> io::Result<()> {
        let mut valid = true;

        match self.read_power() {
            Some(p) => self.last[0] = p,
            None => valid = false,
        }

        if let Some(fd) = &self.capacity {
            match util::read_u64_at(fd, &mut self.buf) {
                Ok(c) => self.last[1] = c,
                Err(_) => valid = false,
            }
        }

        match util::pread0(&self.status, &mut self.buf) {
            Ok(s) if !s.is_empty() => {
                self.last[2] = u64::from(s.starts_with(b"Discharging"));
            }
            _ => valid = false,
        }

        out[0] = self.last[0];
        out[1] = self.last[1];
        out[2] = self.last[2];
        out[3] = u64::from(valid);
        Ok(())
    }
}
