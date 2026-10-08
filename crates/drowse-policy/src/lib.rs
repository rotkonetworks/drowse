//! The act layer (M3).
//!
//! A [`drowse_core::PolicyEngine`] that plans, applies, and reverts reversible
//! system changes: tiered sysfs knobs (audio `power_save`, amd-pstate EPP +
//! `platform_profile`, USB autosuspend, PCI runtime PM, SATA link PM, and the
//! opt-in ASPM policy) plus elastic-service cgroup caps driven through systemd.
//!
//! Discipline:
//! - **Write-ahead journal.** Before any mutation, the whole transaction is
//!   written to `<root>/run/drowse/txn-<id>.json` with each knob's prior value.
//!   A crash, a `revert`, or `Drop` restores the machine from that file.
//! - **Default-safe.** Only T0/T1 knobs apply without opt-in; T2 (ASPM, cgroup
//!   freeze) needs `opt_in_t2`; T3 (NVMe APST and other boot-param items) is
//!   report-only and never written.
//! - **Rooted I/O.** Every path is resolved under a `root` (`/` in production,
//!   a tempdir in tests), exactly like the collectors, so nothing here touches
//!   real sysfs in a unit test.
//! - **Verify-after-apply.** Each real write is read back; risky knobs (ASPM)
//!   also watch the PCIe AER correctable-error counters. A regression triggers
//!   an immediate auto-revert of that one action into `failed`.
//!
//! Config: a small `key = value` file (a TOML-compatible subset parsed by hand
//! to avoid a toml dependency) at `<root>/etc/drowse/policy.toml`:
//! ```text
//! # units the engine may cap on battery
//! syncthing.service = elastic
//! # opt in to T2 knobs (ASPM, cgroup freeze)
//! opt_in_t2 = true
//! ```

use drowse_api::{
    ApplyResult, Estimate, Plan, PolicyAction, RevertResult, Risk, SourceStatus, Tier, SCHEMA,
};
use drowse_core::{PolicyEngine, PolicyProbe, PowerState};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

mod systemctl;
pub use systemctl::{RealSystemCtl, SystemCtl};

#[cfg(test)]
mod tests;

/// Elastic-cap targets. Deliberately gentle for a first cut; a later revision
/// can make these per-unit in the config.
const CAP_CPU_QUOTA: &str = "10%";
const CAP_CPU_WEIGHT: &str = "20";
const CAP_IO_WEIGHT: &str = "20";

/// The three resource-control properties the cap sets, in `systemctl show`
/// spelling (what we read back for `from`) paired with `set-property` spelling.
const CAP_PROPS: &[(&str, &str)] = &[
    ("CPUQuotaPerSecUSec", "CPUQuota"),
    ("CPUWeight", "CPUWeight"),
    ("IOWeight", "IOWeight"),
];

// ---------------------------------------------------------------------------
// Probe
// ---------------------------------------------------------------------------

/// Probe the policy engine. Always `Ready`: `plan` works unprivileged, and
/// `apply` reports per-action `PermissionDenied` rather than refusing wholesale
/// (requirement 5). Production uses `/` and the real `systemctl`.
pub fn probe() -> PolicyProbe {
    PolicyProbe::Ready(Box::new(Engine::new(
        PathBuf::from("/"),
        RealSystemCtl::new(),
    )))
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Unit elasticity class declared in config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// Background work that may be throttled on battery (CPUQuota/weights).
    Elastic,
    /// Frozen on battery (SIGSTOP via cgroup). T2 — opt-in only.
    Freeze,
}

#[derive(Debug, Clone, Default)]
struct Config {
    /// unit name -> class, e.g. `syncthing.service -> Elastic`.
    units: BTreeMap<String, Class>,
    /// Enables T2 knobs (ASPM, freeze). Off by default.
    opt_in_t2: bool,
}

impl Config {
    /// Parse the `key = value` config. Unknown keys and malformed lines are
    /// skipped (logged by the caller, not here). Missing file -> empty config.
    fn load(path: &Path) -> Config {
        let mut cfg = Config::default();
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => return cfg,
        };
        for raw in text.lines() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() || line.starts_with('[') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let (k, v) = (k.trim(), v.trim());
            match (k, v) {
                ("opt_in_t2", v) => cfg.opt_in_t2 = matches!(v, "true" | "1" | "yes"),
                (unit, "elastic") => {
                    cfg.units.insert(unit.to_string(), Class::Elastic);
                }
                (unit, "freeze") => {
                    cfg.units.insert(unit.to_string(), Class::Freeze);
                }
                _ => {}
            }
        }
        cfg
    }
}

// ---------------------------------------------------------------------------
// Internal action model: richer than the wire `PolicyAction` — it also knows
// how to apply, verify, and revert itself.
// ---------------------------------------------------------------------------

/// How an action mutates the system.
#[derive(Debug, Clone)]
enum Act {
    /// Write one value to one or more sysfs paths (EPP fans out over all CPUs).
    Sysfs { paths: Vec<PathBuf> },
    /// Cap a systemd unit via `set-property --runtime`. `props` are the
    /// `set-property` spellings to write on battery; revert clears them.
    Cgroup { unit: String },
    /// Report-only recommendation (T3 boot-param knobs). Never written.
    Report,
}

/// How to confirm an applied knob actually took and did no harm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verify {
    /// Re-read the first path; must equal `to`.
    ReadBack,
    /// Re-read; a vanished device (NotFound) is a regression (USB disconnect).
    Presence,
    /// Read-back plus a PCIe AER correctable-error burst check.
    AspmAer,
    /// Nothing to verify (cgroup caps are confirmed by set-property's exit).
    None,
}

/// A planned change, before it becomes a wire `PolicyAction`.
struct Planned {
    id: String,
    kind: &'static str, // "sysfs" | "cgroup" | "report"
    summary: String,
    tier: Tier,
    risk: Risk,
    reversible: bool,
    from: Option<String>,
    to: Option<String>,
    est: Option<Estimate>,
    note: Option<String>,
    act: Act,
    verify: Verify,
}

impl Planned {
    fn to_wire(&self) -> PolicyAction {
        PolicyAction {
            id: self.id.clone(),
            kind: self.kind.to_string(),
            summary: self.summary.clone(),
            tier: self.tier,
            risk: self.risk,
            reversible: self.reversible,
            from: self.from.clone(),
            to: self.to.clone(),
            est_gain_min: self.est.clone(),
            note: self.note.clone(),
        }
    }
}

/// Where a planned action belongs in the plan.
enum Slot {
    /// Would apply in this state.
    Action(Planned),
    /// Excluded by tier/opt-in/quirk/already-set, with a reason in `note`.
    Excluded(Planned),
}

// ---------------------------------------------------------------------------
// Journal
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalEntry {
    id: String,
    kind: String, // "sysfs" | "cgroup"
    /// sysfs: the path(s) to restore. Empty for cgroup.
    #[serde(default)]
    paths: Vec<String>,
    /// cgroup: the unit whose runtime caps to clear. Empty for sysfs.
    #[serde(default)]
    unit: String,
    /// cgroup: set-property names written (cleared on revert).
    #[serde(default)]
    set_props: Vec<String>,
    /// Prior value (sysfs) or prior-props JSON (cgroup), for display + restore.
    from: String,
    /// Value written, for display.
    to: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Journal {
    txn_id: String,
    state: String,
    created_unix_ms: u64,
    reverted: bool,
    entries: Vec<JournalEntry>,
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

/// The act subsystem. Generic over [`SystemCtl`] so tests inject a fake and
/// never shell out.
pub struct Engine<S: SystemCtl> {
    root: PathBuf,
    config: Config,
    systemctl: S,
    seq: u64,
    /// Applied-but-not-reverted txn ids, restored on `Drop`.
    live: Vec<String>,
}

impl Engine<RealSystemCtl> {
    /// Production constructor: real sysfs under `root`, real `systemctl`.
    pub fn new(root: PathBuf, systemctl: RealSystemCtl) -> Self {
        Self::with_systemctl(root, systemctl)
    }
}

impl<S: SystemCtl> Engine<S> {
    /// Test/DI constructor: any [`SystemCtl`], config read from `<root>`.
    pub fn with_systemctl(root: PathBuf, systemctl: S) -> Self {
        let config = Config::load(&root.join("etc/drowse/policy.toml"));
        Engine { root, config, systemctl, seq: 0, live: Vec::new() }
    }

    fn run_dir(&self) -> PathBuf {
        self.root.join("run/drowse")
    }

    fn journal_path(&self, txn_id: &str) -> PathBuf {
        self.run_dir().join(format!("txn-{txn_id}.json"))
    }

    // -- planning ----------------------------------------------------------

    /// Build every candidate action for `state`, each tagged Action/Excluded.
    fn candidates(&self, state: PowerState) -> Vec<Slot> {
        let mut out = Vec::new();
        self.knob_audio(state, &mut out);
        self.knob_epp(state, &mut out);
        self.knob_platform_profile(state, &mut out);
        self.knob_usb(state, &mut out);
        self.knob_pci_rpm(state, &mut out);
        self.knob_sata(state, &mut out);
        self.knob_aspm(state, &mut out);
        self.knob_caps(state, &mut out);
        self.knob_nvme_apst(&mut out);
        out
    }

    // -- T0: audio codec power_save ----------------------------------------

    fn knob_audio(&self, state: PowerState, out: &mut Vec<Slot>) {
        let path = self.root.join("sys/module/snd_hda_intel/parameters/power_save");
        let Some(cur) = read_trim(&path) else { return };
        let to = match state {
            PowerState::Battery => "10",
            PowerState::Ac => "0",
        };
        let p = Planned {
            id: "knob:audio-powersave".into(),
            kind: "sysfs",
            summary: format!("HDA codec power_save {cur} -> {to}s on {}", label(state)),
            tier: Tier::T0,
            risk: Risk::Low,
            reversible: true,
            from: Some(cur.clone()),
            to: Some(to.into()),
            est: Some(est(2.0, 1.0, 5.0, "rough")),
            note: None,
            act: Act::Sysfs { paths: vec![path] },
            verify: Verify::ReadBack,
        };
        push(out, p, cur == to, None);
    }

    // -- T0: amd-pstate EPP (all CPUs, one action) -------------------------

    fn knob_epp(&self, state: PowerState, out: &mut Vec<Slot>) {
        let status = self.root.join("sys/devices/system/cpu/amd_pstate/status");
        let active = read_trim(&status).as_deref() == Some("active");
        let cpu_base = self.root.join("sys/devices/system/cpu");
        let mut paths = Vec::new();
        for n in 0u32.. {
            let p = cpu_base.join(format!("cpu{n}/cpufreq/energy_performance_preference"));
            if p.exists() {
                paths.push(p);
            } else {
                break;
            }
        }
        if paths.is_empty() {
            return;
        }
        let cur = read_trim(&paths[0]).unwrap_or_default();
        let to = match state {
            PowerState::Battery => "power",
            PowerState::Ac => "balance_performance",
        };
        let p = Planned {
            id: "knob:epp".into(),
            kind: "sysfs",
            summary: format!(
                "amd-pstate EPP {cur} -> {to} on {} ({} CPUs)",
                label(state),
                paths.len()
            ),
            tier: Tier::T0,
            risk: Risk::Low,
            reversible: true,
            from: Some(cur.clone()),
            to: Some(to.into()),
            est: Some(est(12.0, 5.0, 25.0, "modeled")),
            note: None,
            act: Act::Sysfs { paths },
            verify: Verify::ReadBack,
        };
        let skip = if !active {
            Some("amd_pstate not in active mode; EPP has no effect".to_string())
        } else {
            None
        };
        push(out, p, cur == to, skip);
    }

    // -- T0: ACPI platform_profile -----------------------------------------

    fn knob_platform_profile(&self, state: PowerState, out: &mut Vec<Slot>) {
        let path = self.root.join("sys/firmware/acpi/platform_profile");
        let Some(cur) = read_trim(&path) else { return };
        let choices = read_trim(&self.root.join("sys/firmware/acpi/platform_profile_choices"))
            .unwrap_or_default();
        let want = match state {
            PowerState::Battery => "low-power",
            PowerState::Ac => "balanced",
        };
        // Fall back to a supported choice if the ideal one is absent.
        let to = if choices.split_whitespace().any(|c| c == want) {
            want.to_string()
        } else if state == PowerState::Battery
            && choices.split_whitespace().any(|c| c == "quiet")
        {
            "quiet".to_string()
        } else {
            choices.split_whitespace().next().unwrap_or(want).to_string()
        };
        let p = Planned {
            id: "knob:platform-profile".into(),
            kind: "sysfs",
            summary: format!("platform_profile {cur} -> {to} on {}", label(state)),
            tier: Tier::T0,
            risk: Risk::Low,
            reversible: true,
            from: Some(cur.clone()),
            to: Some(to.clone()),
            est: Some(est(8.0, 3.0, 20.0, "modeled")),
            note: None,
            act: Act::Sysfs { paths: vec![path] },
            verify: Verify::ReadBack,
        };
        push(out, p, cur == to, None);
    }

    // -- T1: USB autosuspend (per device, with exclusion lists) ------------

    fn knob_usb(&self, state: PowerState, out: &mut Vec<Slot>) {
        let base = self.root.join("sys/bus/usb/devices");
        let Ok(rd) = std::fs::read_dir(&base) else { return };
        let to = match state {
            PowerState::Battery => "auto",
            PowerState::Ac => "on",
        };
        let mut devs: Vec<String> = rd
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            // Only real devices (busnum-portpath), not interfaces (contain ':').
            .filter(|n| !n.contains(':') && n.chars().next().is_some_and(|c| c.is_ascii_digit()))
            .collect();
        devs.sort();
        for dev in devs {
            let ddir = base.join(&dev);
            let ctrl = ddir.join("power/control");
            let Some(cur) = read_trim(&ctrl) else { continue };
            let id = format!("knob:usb-autosuspend:{dev}");
            // Root hubs: not a power win, skip quietly.
            if read_trim(&ddir.join("idVendor")).as_deref() == Some("1d6b") {
                continue;
            }
            if let Some(reason) = usb_exclude_reason(&ddir) {
                let mut p = usb_action(id, &dev, &cur, to, ctrl.clone());
                p.note = Some(reason);
                out.push(Slot::Excluded(p));
                continue;
            }
            let p = usb_action(id, &dev, &cur, to, ctrl);
            push(out, p, cur == to, None);
        }
    }

    // -- T1: PCI runtime PM (per device, driver denylist) ------------------

    fn knob_pci_rpm(&self, state: PowerState, out: &mut Vec<Slot>) {
        let base = self.root.join("sys/bus/pci/devices");
        let Ok(rd) = std::fs::read_dir(&base) else { return };
        let to = match state {
            PowerState::Battery => "auto",
            PowerState::Ac => "on",
        };
        let mut devs: Vec<String> =
            rd.flatten().filter_map(|e| e.file_name().into_string().ok()).collect();
        devs.sort();
        for dev in devs {
            let ddir = base.join(&dev);
            let ctrl = ddir.join("power/control");
            let Some(cur) = read_trim(&ctrl) else { continue };
            let driver = driver_of(&ddir);
            let id = format!("knob:pci-rpm:{dev}");
            let mk = |note: Option<String>| Planned {
                id: id.clone(),
                kind: "sysfs",
                summary: format!(
                    "PCI {dev} ({}) runtime PM {cur} -> {to} on {}",
                    driver.as_deref().unwrap_or("no-driver"),
                    label(state)
                ),
                tier: Tier::T1,
                risk: Risk::Low,
                reversible: true,
                from: Some(cur.clone()),
                to: Some(to.into()),
                est: Some(est(3.0, 1.0, 10.0, "rough")),
                note,
                act: Act::Sysfs { paths: vec![ctrl.clone()] },
                verify: Verify::Presence,
            };
            if let Some(d) = &driver
                && PCI_RPM_DENYLIST.contains(&d.as_str())
            {
                out.push(Slot::Excluded(mk(Some(format!(
                    "driver '{d}' on runtime-PM denylist (hybrid-GPU / USB-host safety)"
                )))));
                continue;
            }
            push(out, mk(None), cur == to, None);
        }
    }

    // -- T1: SATA link power management ------------------------------------

    fn knob_sata(&self, state: PowerState, out: &mut Vec<Slot>) {
        let base = self.root.join("sys/class/scsi_host");
        let Ok(rd) = std::fs::read_dir(&base) else { return };
        let to = match state {
            PowerState::Battery => "med_power_with_dipm",
            PowerState::Ac => "max_performance",
        };
        let mut hosts: Vec<String> =
            rd.flatten().filter_map(|e| e.file_name().into_string().ok()).collect();
        hosts.sort();
        for host in hosts {
            let path = base.join(&host).join("link_power_management_policy");
            let Some(cur) = read_trim(&path) else { continue };
            let p = Planned {
                id: format!("knob:sata-alpm:{host}"),
                kind: "sysfs",
                summary: format!("{host} link PM {cur} -> {to} on {}", label(state)),
                tier: Tier::T1,
                risk: Risk::Low,
                reversible: true,
                from: Some(cur.clone()),
                to: Some(to.into()),
                est: Some(est(4.0, 2.0, 8.0, "rough")),
                note: None,
                act: Act::Sysfs { paths: vec![path] },
                verify: Verify::ReadBack,
            };
            push(out, p, cur == to, None);
        }
    }

    // -- T2: PCIe ASPM policy (opt-in) -------------------------------------

    fn knob_aspm(&self, state: PowerState, out: &mut Vec<Slot>) {
        let path = self.root.join("sys/module/pcie_aspm/parameters/policy");
        let Some(raw) = read_trim(&path) else { return };
        let cur = aspm_current(&raw).unwrap_or(&raw).to_string();
        let to = match state {
            PowerState::Battery => "powersave",
            PowerState::Ac => "default",
        };
        let p = Planned {
            id: "knob:pcie-aspm".into(),
            kind: "sysfs",
            summary: format!("PCIe ASPM policy {cur} -> {to} on {}", label(state)),
            tier: Tier::T2,
            risk: Risk::Medium,
            reversible: true,
            from: Some(cur.clone()),
            to: Some(to.into()),
            est: Some(est(7.0, 2.0, 20.0, "rough")),
            note: None,
            act: Act::Sysfs { paths: vec![path] },
            verify: Verify::AspmAer,
        };
        if cur == to {
            push(out, p, true, None);
        } else if self.config.opt_in_t2 {
            out.push(Slot::Action(p));
        } else {
            let mut p = p;
            p.note = Some("T2: opt in with opt_in_t2=true (ASPM powersave breaks some NICs)".into());
            out.push(Slot::Excluded(p));
        }
    }

    // -- Elastic-service caps (the novel part) -----------------------------

    fn knob_caps(&self, state: PowerState, out: &mut Vec<Slot>) {
        for (unit, class) in &self.config.units {
            // Freeze is T2; only cap-class handled as a settable action here.
            let tier = match class {
                Class::Elastic => Tier::T1,
                Class::Freeze => Tier::T2,
            };
            if !self.systemctl.exists(unit) {
                out.push(Slot::Excluded(Planned {
                    id: format!("cap:{unit}"),
                    kind: "cgroup",
                    summary: format!("cap {unit} on battery"),
                    tier,
                    risk: Risk::Low,
                    reversible: true,
                    from: None,
                    to: None,
                    est: None,
                    note: Some("unit not loaded".into()),
                    act: Act::Cgroup { unit: unit.clone() },
                    verify: Verify::None,
                }));
                continue;
            }
            let show_keys: Vec<&str> = CAP_PROPS.iter().map(|(k, _)| *k).collect();
            let from_map = self.systemctl.show(unit, &show_keys).unwrap_or_default();
            let from = serde_json::to_string(&from_map).unwrap_or_default();
            // On AC the cap is lifted (revert semantics); only battery caps.
            let (summary, to, is_action) = match (state, class) {
                (PowerState::Battery, Class::Elastic) => (
                    format!(
                        "cap {unit}: CPUQuota={CAP_CPU_QUOTA} CPUWeight={CAP_CPU_WEIGHT} IOWeight={CAP_IO_WEIGHT}"
                    ),
                    format!(
                        "CPUQuota={CAP_CPU_QUOTA},CPUWeight={CAP_CPU_WEIGHT},IOWeight={CAP_IO_WEIGHT}"
                    ),
                    true,
                ),
                (PowerState::Battery, Class::Freeze) => {
                    (format!("freeze {unit} on battery (SIGSTOP)"), "frozen".into(), true)
                }
                (PowerState::Ac, _) => {
                    (format!("lift cap on {unit} (AC)"), "uncapped".into(), false)
                }
            };
            let p = Planned {
                id: format!("cap:{unit}"),
                kind: "cgroup",
                summary,
                tier,
                risk: Risk::Low,
                reversible: true,
                from: Some(from),
                to: Some(to),
                est: Some(est(6.0, 2.0, 18.0, "measured")),
                note: None,
                act: Act::Cgroup { unit: unit.clone() },
                verify: Verify::None,
            };
            // On AC, the cap is removed by reverting the battery txn, not by a
            // fresh apply; surface it as informational-excluded.
            if !is_action {
                let mut p = p;
                p.note = Some("lifted on AC by reverting the battery transaction".into());
                out.push(Slot::Excluded(p));
                continue;
            }
            // Freeze gated behind opt_in_t2.
            if matches!(class, Class::Freeze) && !self.config.opt_in_t2 {
                let mut p = p;
                p.note = Some("T2: cgroup freeze needs opt_in_t2=true".into());
                out.push(Slot::Excluded(p));
                continue;
            }
            out.push(Slot::Action(p));
        }
    }

    // -- T3: NVMe APST (report-only) ---------------------------------------

    fn knob_nvme_apst(&self, out: &mut Vec<Slot>) {
        let base = self.root.join("sys/class/nvme");
        let Ok(rd) = std::fs::read_dir(&base) else { return };
        let mut ctrls: Vec<String> =
            rd.flatten().filter_map(|e| e.file_name().into_string().ok()).collect();
        ctrls.sort();
        for c in ctrls {
            out.push(Slot::Excluded(Planned {
                id: format!("report:nvme-apst:{c}"),
                kind: "report",
                summary: format!("{c}: deepen NVMe APST via boot param (report-only)"),
                tier: Tier::T3,
                risk: Risk::Medium,
                reversible: false,
                from: None,
                to: Some("nvme_core.default_ps_max_latency_us=5000".into()),
                est: Some(est(5.0, 2.0, 12.0, "rough")),
                note: Some(
                    "T3 boot-param only; verify the drive isn't on the kernel \
                     NO_APST/NO_DEEPEST_PS quirk list before setting"
                        .into(),
                ),
                act: Act::Report,
                verify: Verify::None,
            }));
        }
    }

    // -- apply / verify / revert mechanics ---------------------------------

    fn next_txn_id(&mut self) -> String {
        self.seq += 1;
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        format!("{ms}-{}", self.seq)
    }

    /// Apply one sysfs action's value to all its paths. Returns the first error.
    fn write_sysfs(&self, paths: &[PathBuf], value: &str) -> io::Result<()> {
        for p in paths {
            std::fs::write(p, value.as_bytes())?;
        }
        Ok(())
    }

    /// Confirm a just-applied action; `Ok(())` = good, `Err` = regression.
    fn verify(&self, pl: &Planned, baseline_aer: u64) -> io::Result<()> {
        let to = pl.to.as_deref().unwrap_or("");
        match (&pl.act, pl.verify) {
            (Act::Sysfs { paths }, Verify::ReadBack) => match read_trim(&paths[0]) {
                Some(v) if v == to => Ok(()),
                Some(v) => Err(regression(format!("read-back {v:?} != {to:?}"))),
                None => Err(regression("knob unreadable after write".into())),
            },
            (Act::Sysfs { paths }, Verify::Presence) => {
                if read_trim(&paths[0]).is_some() {
                    Ok(())
                } else {
                    Err(regression("device vanished after autosuspend".into()))
                }
            }
            (Act::Sysfs { paths }, Verify::AspmAer) => {
                if read_trim(&paths[0]).and_then(|r| aspm_current(&r).map(str::to_string))
                    != Some(to.to_string())
                {
                    return Err(regression("ASPM policy did not take".into()));
                }
                let now = self.aer_sum();
                if now > baseline_aer.saturating_add(AER_BURST_THRESHOLD) {
                    Err(regression(format!(
                        "PCIe AER correctable errors burst (+{})",
                        now - baseline_aer
                    )))
                } else {
                    Ok(())
                }
            }
            _ => Ok(()),
        }
    }

    /// Sum of all PCIe `aer_dev_correctable` error counts (regression canary).
    fn aer_sum(&self) -> u64 {
        let base = self.root.join("sys/bus/pci/devices");
        let Ok(rd) = std::fs::read_dir(&base) else { return 0 };
        let mut sum = 0u64;
        for e in rd.flatten() {
            let path = e.path().join("aer_dev_correctable");
            if let Some(txt) = read_trim(&path) {
                // Format is "KEY count" lines; sum the trailing numbers.
                for line in txt.lines() {
                    if let Some(n) = line.split_whitespace().next_back().and_then(|t| t.parse::<u64>().ok())
                    {
                        sum = sum.saturating_add(n);
                    }
                }
            }
        }
        sum
    }

    /// Restore a single journal entry to its prior state (best effort shape;
    /// errors returned so callers can decide whether to ignore them).
    fn restore_entry(&self, e: &JournalEntry) -> io::Result<()> {
        match e.kind.as_str() {
            "sysfs" => {
                for p in &e.paths {
                    std::fs::write(p, e.from.as_bytes())?;
                }
                Ok(())
            }
            "cgroup" => {
                // Clear the runtime overrides we set; the unit falls back to
                // its persistent config (or systemd defaults).
                let clear: Vec<(String, String)> =
                    e.set_props.iter().map(|p| (p.clone(), String::new())).collect();
                self.systemctl.set_property(&e.unit, true, &clear)
            }
            _ => Ok(()),
        }
    }

    /// Read + mark a journal, replaying it in reverse. Shared by `revert` and
    /// `Drop`. Returns the restored actions (empty if nothing to do).
    fn restore_txn(&mut self, txn_id: &str) -> io::Result<Vec<PolicyAction>> {
        let path = self.journal_path(txn_id);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut journal: Journal =
            serde_json::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if journal.reverted {
            return Ok(Vec::new());
        }
        let mut reverted = Vec::new();
        for e in journal.entries.iter().rev() {
            // Best effort: an entry that fails to restore (e.g. perms) is
            // reported but does not abort the rest of the rollback.
            let _ = self.restore_entry(e);
            reverted.push(PolicyAction {
                id: e.id.clone(),
                kind: e.kind.clone(),
                summary: format!("restore {} -> {}", e.to, e.from),
                tier: Tier::T0,
                risk: Risk::Low,
                reversible: true,
                from: Some(e.to.clone()),
                to: Some(e.from.clone()),
                est_gain_min: None,
                note: None,
            });
        }
        journal.reverted = true;
        let _ = std::fs::write(
            &path,
            serde_json::to_vec_pretty(&journal).unwrap_or_default(),
        );
        self.live.retain(|id| id != txn_id);
        Ok(reverted)
    }
}

impl<S: SystemCtl> PolicyEngine for Engine<S> {
    fn plan(&self, state: PowerState) -> io::Result<Plan> {
        let mut actions = Vec::new();
        let mut excluded = Vec::new();
        for slot in self.candidates(state) {
            match slot {
                Slot::Action(p) => actions.push(p.to_wire()),
                Slot::Excluded(p) => excluded.push(p.to_wire()),
            }
        }
        Ok(Plan {
            schema: SCHEMA,
            kind: "plan".into(),
            on_battery: state == PowerState::Battery,
            actions,
            excluded,
        })
    }

    fn apply(&mut self, state: PowerState, dry_run: bool) -> io::Result<ApplyResult> {
        let planned: Vec<Planned> = self
            .candidates(state)
            .into_iter()
            .filter_map(|s| match s {
                Slot::Action(p) => Some(p),
                Slot::Excluded(_) => None,
            })
            .collect();

        if dry_run {
            return Ok(ApplyResult {
                schema: SCHEMA,
                kind: "apply".into(),
                txn_id: "dry-run".into(),
                dry_run: true,
                applied: planned.iter().map(Planned::to_wire).collect(),
                failed: Vec::new(),
            });
        }

        let txn_id = self.next_txn_id();
        // Write-ahead: build + persist the full journal BEFORE any mutation.
        let mut entries = Vec::new();
        for p in &planned {
            match &p.act {
                Act::Sysfs { paths } => entries.push(JournalEntry {
                    id: p.id.clone(),
                    kind: "sysfs".into(),
                    paths: paths.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
                    unit: String::new(),
                    set_props: Vec::new(),
                    from: p.from.clone().unwrap_or_default(),
                    to: p.to.clone().unwrap_or_default(),
                }),
                Act::Cgroup { unit } => entries.push(JournalEntry {
                    id: p.id.clone(),
                    kind: "cgroup".into(),
                    paths: Vec::new(),
                    unit: unit.clone(),
                    set_props: CAP_PROPS.iter().map(|(_, set)| (*set).to_string()).collect(),
                    from: p.from.clone().unwrap_or_default(),
                    to: p.to.clone().unwrap_or_default(),
                }),
                Act::Report => {}
            }
        }
        let journal = Journal {
            txn_id: txn_id.clone(),
            state: label(state).into(),
            created_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            reverted: false,
            entries: entries.clone(),
        };
        std::fs::create_dir_all(self.run_dir())?;
        std::fs::write(self.journal_path(&txn_id), serde_json::to_vec_pretty(&journal)?)?;
        self.live.push(txn_id.clone());

        // Mutate, verifying each. AER baseline captured once for ASPM.
        let baseline_aer = if planned.iter().any(|p| p.verify == Verify::AspmAer) {
            self.aer_sum()
        } else {
            0
        };
        let mut applied = Vec::new();
        let mut failed = Vec::new();
        for (p, entry) in planned.iter().zip(entries.iter()) {
            let write_res = match &p.act {
                Act::Sysfs { paths } => self.write_sysfs(paths, p.to.as_deref().unwrap_or("")),
                Act::Cgroup { unit } => {
                    let props: Vec<(String, String)> = match p.to.as_deref() {
                        Some(v) => v
                            .split(',')
                            .filter_map(|kv| kv.split_once('='))
                            .map(|(k, v)| (k.to_string(), v.to_string()))
                            .collect(),
                        None => Vec::new(),
                    };
                    self.systemctl.set_property(unit, true, &props)
                }
                Act::Report => Ok(()),
            };
            match write_res {
                Err(e) => {
                    let mut w = p.to_wire();
                    w.note = Some(format!("{}: {e}", err_label(&e)));
                    failed.push(w);
                }
                Ok(()) => match self.verify(p, baseline_aer) {
                    Ok(()) => applied.push(p.to_wire()),
                    Err(e) => {
                        // Regression: roll back this one knob immediately.
                        let _ = self.restore_entry(entry);
                        let mut w = p.to_wire();
                        w.note = Some(format!("auto-reverted: {e}"));
                        failed.push(w);
                    }
                },
            }
        }

        Ok(ApplyResult {
            schema: SCHEMA,
            kind: "apply".into(),
            txn_id,
            dry_run: false,
            applied,
            failed,
        })
    }

    fn revert(&mut self, txn_id: &str) -> io::Result<RevertResult> {
        let reverted = self.restore_txn(txn_id)?;
        Ok(RevertResult {
            schema: SCHEMA,
            kind: "revert".into(),
            txn_id: txn_id.to_string(),
            already_reverted: reverted.is_empty(),
            reverted,
        })
    }
}

impl<S: SystemCtl> Drop for Engine<S> {
    /// Restore-on-exit: any applied transaction not explicitly reverted is
    /// replayed in reverse from its journal. Belt-and-braces with `revert`.
    fn drop(&mut self) {
        for id in std::mem::take(&mut self.live) {
            let _ = self.restore_txn(&id);
        }
    }
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

/// PCI drivers excluded from runtime-PM auto (TLP's default denylist): prevents
/// waking a hybrid dGPU and known-fragile USB host autosuspend.
const PCI_RPM_DENYLIST: &[&str] = &["mei_me", "nouveau", "radeon", "amdgpu", "nvidia", "xhci_hcd"];

/// AER correctable-error count increase that flags an ASPM regression.
const AER_BURST_THRESHOLD: u64 = 5;

fn label(state: PowerState) -> &'static str {
    match state {
        PowerState::Battery => "battery",
        PowerState::Ac => "ac",
    }
}

fn est(mid: f64, low: f64, high: f64, basis: &str) -> Estimate {
    Estimate { mid, low, high, basis: basis.into() }
}

/// Read a sysfs attribute as a trimmed String; `None` on any error.
fn read_trim(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// Extract the bracketed current token from an ASPM policy line like
/// `default performance [powersave] powersupersave`.
fn aspm_current(raw: &str) -> Option<&str> {
    let start = raw.find('[')? + 1;
    let end = raw[start..].find(']')? + start;
    Some(&raw[start..end])
}

/// Basename of a PCI device's bound driver, via the `driver` symlink.
fn driver_of(ddir: &Path) -> Option<String> {
    std::fs::read_link(ddir.join("driver"))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
}

/// USB autosuspend exclusion: input/HID, audio, and Bluetooth devices add wake
/// latency or drop out when suspended. Returns the reason to exclude, if any.
fn usb_exclude_reason(ddir: &Path) -> Option<String> {
    // Interface subdirs are named like `1-3:1.0`; each has bInterfaceClass.
    let rd = std::fs::read_dir(ddir).ok()?;
    for e in rd.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if !name.contains(':') {
            continue;
        }
        if let Some(class) = read_trim(&e.path().join("bInterfaceClass")) {
            match class.as_str() {
                "03" => return Some("HID/input device: autosuspend adds wake latency".into()),
                "01" => return Some("audio device: autosuspend glitches".into()),
                "e0" => return Some("wireless/Bluetooth: excluded like TLP btusb".into()),
                _ => {}
            }
        }
    }
    None
}

fn usb_action(id: String, dev: &str, cur: &str, to: &str, ctrl: PathBuf) -> Planned {
    Planned {
        id,
        kind: "sysfs",
        summary: format!("USB {dev} autosuspend {cur} -> {to}"),
        tier: Tier::T1,
        risk: Risk::Low,
        reversible: true,
        from: Some(cur.to_string()),
        to: Some(to.to_string()),
        est: Some(est(1.5, 0.5, 4.0, "rough")),
        note: None,
        act: Act::Sysfs { paths: vec![ctrl] },
        verify: Verify::Presence,
    }
}

/// Push a candidate as an Action, or Excluded if already at target / tier-skip.
fn push(out: &mut Vec<Slot>, mut p: Planned, already: bool, skip: Option<String>) {
    if let Some(reason) = skip {
        p.note = Some(reason);
        out.push(Slot::Excluded(p));
    } else if already {
        p.note = Some("already at target".into());
        out.push(Slot::Excluded(p));
    } else {
        out.push(Slot::Action(p));
    }
}

fn regression(msg: String) -> io::Error {
    io::Error::other(msg)
}

fn err_label(e: &io::Error) -> &'static str {
    match e.kind() {
        io::ErrorKind::PermissionDenied => "permission denied",
        io::ErrorKind::NotFound => "not present",
        _ => "write failed",
    }
}

/// Reports the policy source as available (plan always works). Kept for the
/// daemon's `system.check` symmetry with the collectors.
pub fn source_status() -> SourceStatus {
    SourceStatus::ok()
}
