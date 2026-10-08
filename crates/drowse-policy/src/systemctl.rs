//! The systemd seam for elastic-service caps.
//!
//! We talk to systemd through the `systemctl` CLI rather than adding a D-Bus
//! client (`zbus`) dependency — a deliberate trade for a small first cut. The
//! operations map 1:1 onto the D-Bus calls we would otherwise make:
//!   - [`SystemCtl::show`]    ->  `org.freedesktop.DBus.Properties.Get`
//!   - [`SystemCtl::set_property`] (runtime) -> `Manager.SetUnitProperties(runtime=true)`
//!   - [`SystemCtl::exists`]  ->  a `LoadState` lookup
//!
//! `set_property(.., runtime=true, ..)` writes a `/run` drop-in that systemd
//! drops on reboot — the same transient guarantee our journal gives for sysfs.
//!
//! The trait exists so tests inject [`tests::FakeSystemCtl`] and never fork a
//! real `systemctl`.

use std::io;
use std::process::Command;

/// Minimal systemd control surface used by the cap engine.
pub trait SystemCtl: Send {
    /// Read the named properties of `unit`; returns `(name, value)` pairs.
    fn show(&self, unit: &str, props: &[&str]) -> io::Result<Vec<(String, String)>>;
    /// Set properties on `unit`. With `runtime`, the change is transient
    /// (gone at reboot). An empty value clears a property (reverts the cap).
    fn set_property(&self, unit: &str, runtime: bool, props: &[(String, String)]) -> io::Result<()>;
    /// Whether the unit is known to systemd (loaded / has a unit file).
    fn exists(&self, unit: &str) -> bool;
}

/// Production implementation: shells out to `systemctl`.
#[derive(Debug, Clone, Default)]
pub struct RealSystemCtl {
    _priv: (),
}

impl RealSystemCtl {
    pub fn new() -> Self {
        RealSystemCtl { _priv: () }
    }
}

impl SystemCtl for RealSystemCtl {
    fn show(&self, unit: &str, props: &[&str]) -> io::Result<Vec<(String, String)>> {
        let mut cmd = Command::new("systemctl");
        cmd.arg("show");
        for p in props {
            cmd.arg("-p").arg(p);
        }
        cmd.arg(unit);
        let out = cmd.output()?;
        if !out.status.success() {
            return Err(io::Error::other(format!(
                "systemctl show {unit} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        Ok(text
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect())
    }

    fn set_property(&self, unit: &str, runtime: bool, props: &[(String, String)]) -> io::Result<()> {
        let mut cmd = Command::new("systemctl");
        cmd.arg("set-property");
        if runtime {
            cmd.arg("--runtime");
        }
        cmd.arg(unit);
        for (k, v) in props {
            cmd.arg(format!("{k}={v}"));
        }
        let out = cmd.output()?;
        if out.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        // Map polkit/privilege refusals onto PermissionDenied so the engine
        // can land them in `failed` with the right note.
        let kind = if stderr.contains("Access denied")
            || stderr.contains("Interactive authentication required")
            || stderr.contains("not authorized")
        {
            io::ErrorKind::PermissionDenied
        } else {
            io::ErrorKind::Other
        };
        Err(io::Error::new(kind, format!("systemctl set-property {unit}: {}", stderr.trim())))
    }

    fn exists(&self, unit: &str) -> bool {
        Command::new("systemctl")
            .args(["show", "-p", "LoadState", unit])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| {
                let s = String::from_utf8_lossy(&o.stdout);
                let state = s.trim().strip_prefix("LoadState=").unwrap_or("");
                state == "loaded"
            })
            .unwrap_or(false)
    }
}
