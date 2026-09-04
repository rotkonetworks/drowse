//! iGPU busyness from the first DRM card exposing `gpu_busy_percent`.
//!
//! Field:
//!   `card{N}.gpu_busy_percent`  Gauge, Percent
//!
//! M1 deliberately samples nothing else from amdgpu: `gpu_metrics`,
//! `pp_dpm_*` and hwmon power sensors each cost an SMU mailbox round-trip
//! and wake the SMU, perturbing the very package idle being measured.
//! `gpu_busy_percent` == 0 sustained is the M1 proxy for "GFX quiesced /
//! GFXOFF-eligible" (Phoenix exposes no GFXOFF residency counters as of
//! mainline 6.16).

use crate::util;
use drowse_core::{Collector, Field, Kind, Probe, Unit};
use rustix::fd::OwnedFd;
use std::io;
use std::path::Path;

const NAME: &str = "amdgpu";

pub fn probe(root: &Path) -> Probe {
    match Amdgpu::new(root) {
        Ok(c) => Probe::Ready(Box::new(c)),
        Err(e) => util::unavailable(NAME, "sys/class/drm/card*/device/gpu_busy_percent", &e),
    }
}

struct Amdgpu {
    fields: Vec<Field>,
    fd: OwnedFd,
    buf: [u8; 16],
}

impl Amdgpu {
    fn new(root: &Path) -> io::Result<Amdgpu> {
        let base = root.join("sys/class/drm");
        let mut cards: Vec<u32> = Vec::new();
        for entry in std::fs::read_dir(&base)? {
            let entry = entry?;
            if let Some(n) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.strip_prefix("card"))
                .and_then(|s| s.parse::<u32>().ok())
            {
                cards.push(n);
            }
        }
        cards.sort_unstable();

        // First card that has the attribute wins; remember the most telling
        // error (anything but ENOENT) for classification if none does.
        let mut hard_err: Option<io::Error> = None;
        for n in cards {
            let dev = base.join(format!("card{n}/device"));
            let Ok(dir) = util::open_dir(&dev) else { continue };
            match util::open_at(&dir, "gpu_busy_percent") {
                Ok(fd) => {
                    let fields = vec![Field {
                        name: format!("card{n}.gpu_busy_percent"),
                        kind: Kind::Gauge,
                        unit: Unit::Percent,
                    }];
                    return Ok(Amdgpu { fields, fd, buf: [0; 16] });
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => hard_err = Some(e),
            }
        }
        Err(hard_err.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "no card with gpu_busy_percent")
        }))
    }
}

impl Collector for Amdgpu {
    fn name(&self) -> &'static str {
        NAME
    }

    fn fields(&self) -> &[Field] {
        &self.fields
    }

    fn sample(&mut self, out: &mut [u64]) -> io::Result<()> {
        out[0] = util::read_u64_at(&self.fd, &mut self.buf)?;
        Ok(())
    }
}
