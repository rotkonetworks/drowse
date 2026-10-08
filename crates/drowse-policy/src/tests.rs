//! Unit tests. Every test writes under a tempdir root; none touches real
//! sysfs or forks a real `systemctl`. The one live test is `#[ignore]`d,
//! guarded by sudo, and reverts what it applies.

use super::*;
use crate::systemctl::SystemCtl;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A throwaway directory tree that cleans itself up.
struct TmpRoot {
    path: PathBuf,
}

impl TmpRoot {
    fn new() -> TmpRoot {
        static N: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        let n = N.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("drowse-policy-test-{pid}-{n}"));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        TmpRoot { path }
    }

    fn write(&self, rel: &str, contents: &str) {
        let p = self.path.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, contents).unwrap();
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.path.join(rel)).unwrap().trim().to_string()
    }
}

impl Drop for TmpRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Lay down a minimal Ryzen-mobile-ish sysfs: audio, EPP (2 CPUs),
/// platform_profile, one suspendable + one HID USB device, one PCI device,
/// one denylisted PCI device, and an nvme controller.
fn fixture(root: &TmpRoot) {
    root.write("sys/module/snd_hda_intel/parameters/power_save", "0\n");
    root.write("sys/devices/system/cpu/amd_pstate/status", "active\n");
    root.write("sys/devices/system/cpu/cpu0/cpufreq/energy_performance_preference", "performance\n");
    root.write("sys/devices/system/cpu/cpu1/cpufreq/energy_performance_preference", "performance\n");
    root.write("sys/firmware/acpi/platform_profile", "balanced\n");
    root.write("sys/firmware/acpi/platform_profile_choices", "low-power balanced performance\n");

    // Suspendable USB device (vendor storage, class 08).
    root.write("sys/bus/usb/devices/1-1/idVendor", "0781\n");
    root.write("sys/bus/usb/devices/1-1/power/control", "on\n");
    root.write("sys/bus/usb/devices/1-1/1-1:1.0/bInterfaceClass", "08\n");
    // HID device (class 03) -> must be excluded.
    root.write("sys/bus/usb/devices/1-2/idVendor", "046d\n");
    root.write("sys/bus/usb/devices/1-2/power/control", "on\n");
    root.write("sys/bus/usb/devices/1-2/1-2:1.0/bInterfaceClass", "03\n");
    // Root hub -> skipped quietly.
    root.write("sys/bus/usb/devices/usb1/idVendor", "1d6b\n");
    root.write("sys/bus/usb/devices/usb1/power/control", "auto\n");

    // PCI device with runtime PM available and a benign driver.
    root.write("sys/bus/pci/devices/0000:01:00.0/power/control", "on\n");
    symlink_driver(root, "0000:01:00.0", "nvme");
    // PCI device on the denylist (amdgpu).
    root.write("sys/bus/pci/devices/0000:c1:00.0/power/control", "on\n");
    symlink_driver(root, "0000:c1:00.0", "amdgpu");

    root.write("sys/class/nvme/nvme0/model", "FixtureSSD\n");
}

fn symlink_driver(root: &TmpRoot, dev: &str, driver: &str) {
    let ddir = root.path.join(format!("sys/bus/pci/devices/{dev}"));
    std::fs::create_dir_all(&ddir).unwrap();
    let drv = root.path.join(format!("sys/bus/pci/drivers/{driver}"));
    std::fs::create_dir_all(&drv).unwrap();
    let _ = std::os::unix::fs::symlink(&drv, ddir.join("driver"));
}

// ---------------------------------------------------------------------------
// Fake systemctl: an in-memory unit property store.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakeSystemCtl {
    /// unit -> (prop -> value). Present keys = loaded units.
    units: RefCell<BTreeMap<String, BTreeMap<String, String>>>,
    /// Count of set_property calls, for assertions.
    sets: AtomicU64,
}

impl FakeSystemCtl {
    fn with_unit(unit: &str) -> FakeSystemCtl {
        let f = FakeSystemCtl::default();
        let mut defaults = BTreeMap::new();
        defaults.insert("CPUQuotaPerSecUSec".to_string(), "infinity".to_string());
        defaults.insert("CPUWeight".to_string(), "[not set]".to_string());
        defaults.insert("IOWeight".to_string(), "[not set]".to_string());
        f.units.borrow_mut().insert(unit.to_string(), defaults);
        f
    }
}

// RefCell isn't Sync, but the engine only uses the fake from one thread in
// tests; SystemCtl requires Send, which RefCell satisfies.
impl SystemCtl for FakeSystemCtl {
    fn show(&self, unit: &str, props: &[&str]) -> io::Result<Vec<(String, String)>> {
        let units = self.units.borrow();
        let u = units.get(unit).ok_or_else(|| io::Error::other("no unit"))?;
        Ok(props.iter().map(|p| (p.to_string(), u.get(*p).cloned().unwrap_or_default())).collect())
    }

    fn set_property(&self, unit: &str, _runtime: bool, props: &[(String, String)]) -> io::Result<()> {
        self.sets.fetch_add(1, Ordering::Relaxed);
        let mut units = self.units.borrow_mut();
        let u = units
            .get_mut(unit)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unit not loaded"))?;
        for (k, v) in props {
            // set-property uses CPUQuota; show uses CPUQuotaPerSecUSec. Mirror
            // the write under the show-name so later reads reflect it.
            let show_key = match k.as_str() {
                "CPUQuota" => "CPUQuotaPerSecUSec",
                other => other,
            };
            if v.is_empty() {
                // Empty clears the runtime override -> back to default.
                let def = if show_key == "CPUQuotaPerSecUSec" { "infinity" } else { "[not set]" };
                u.insert(show_key.to_string(), def.to_string());
            } else {
                u.insert(show_key.to_string(), v.clone());
            }
        }
        Ok(())
    }

    fn exists(&self, unit: &str) -> bool {
        self.units.borrow().contains_key(unit)
    }
}

fn engine(root: &TmpRoot, sc: FakeSystemCtl) -> Engine<FakeSystemCtl> {
    Engine::with_systemctl(root.path.clone(), sc)
}

// ---------------------------------------------------------------------------
// Plan
// ---------------------------------------------------------------------------

#[test]
fn plan_battery_lists_knobs_with_tiers() {
    let root = TmpRoot::new();
    fixture(&root);
    let eng = engine(&root, FakeSystemCtl::default());
    let plan = eng.plan(PowerState::Battery).unwrap();

    assert!(plan.on_battery);
    let ids: Vec<&str> = plan.actions.iter().map(|a| a.id.as_str()).collect();
    // Audio/EPP/platform-profile all differ from target -> actionable.
    assert!(ids.contains(&"knob:audio-powersave"));
    assert!(ids.contains(&"knob:epp"));
    assert!(ids.contains(&"knob:platform-profile"));
    // Suspendable USB device is actionable.
    assert!(ids.contains(&"knob:usb-autosuspend:1-1"));
    // Benign PCI device is actionable.
    assert!(ids.contains(&"knob:pci-rpm:0000:01:00.0"));

    // Tiers are set correctly.
    let epp = plan.actions.iter().find(|a| a.id == "knob:epp").unwrap();
    assert_eq!(epp.tier, Tier::T0);
    assert_eq!(epp.to.as_deref(), Some("power"));
}

#[test]
fn plan_excludes_hid_denylist_t2_and_t3() {
    let root = TmpRoot::new();
    fixture(&root);
    let eng = engine(&root, FakeSystemCtl::default());
    let plan = eng.plan(PowerState::Battery).unwrap();

    let ex: BTreeMap<&str, &str> = plan
        .excluded
        .iter()
        .map(|a| (a.id.as_str(), a.note.as_deref().unwrap_or("")))
        .collect();

    // HID USB excluded.
    assert!(ex.get("knob:usb-autosuspend:1-2").unwrap().contains("HID"));
    // amdgpu PCI device excluded by driver denylist.
    assert!(ex.get("knob:pci-rpm:0000:c1:00.0").unwrap().contains("denylist"));
    // ASPM is absent from the fixture (no pcie_aspm module) so not present; add
    // it and re-plan to confirm T2 gating.
    assert!(ex.contains_key("report:nvme-apst:nvme0"));
    let nvme = plan.excluded.iter().find(|a| a.id == "report:nvme-apst:nvme0").unwrap();
    assert_eq!(nvme.tier, Tier::T3);
    assert_eq!(nvme.kind, "report");
}

#[test]
fn aspm_t2_gated_then_opted_in() {
    let root = TmpRoot::new();
    fixture(&root);
    root.write(
        "sys/module/pcie_aspm/parameters/policy",
        "default performance [default] powersave powersupersave\n",
    );
    // Default config: ASPM excluded, T2.
    let plan = engine(&root, FakeSystemCtl::default()).plan(PowerState::Battery).unwrap();
    let aspm = plan.excluded.iter().find(|a| a.id == "knob:pcie-aspm").unwrap();
    assert_eq!(aspm.tier, Tier::T2);
    assert!(aspm.note.as_deref().unwrap().contains("opt_in_t2"));

    // Opt in -> becomes an action.
    root.write("etc/drowse/policy.toml", "opt_in_t2 = true\n");
    let plan = engine(&root, FakeSystemCtl::default()).plan(PowerState::Battery).unwrap();
    assert!(plan.actions.iter().any(|a| a.id == "knob:pcie-aspm"));
}

#[test]
fn already_at_target_is_excluded_not_an_action() {
    let root = TmpRoot::new();
    fixture(&root);
    // Put audio already at the battery target.
    root.write("sys/module/snd_hda_intel/parameters/power_save", "10\n");
    let plan = engine(&root, FakeSystemCtl::default()).plan(PowerState::Battery).unwrap();
    assert!(!plan.actions.iter().any(|a| a.id == "knob:audio-powersave"));
    let ex = plan.excluded.iter().find(|a| a.id == "knob:audio-powersave").unwrap();
    assert_eq!(ex.note.as_deref(), Some("already at target"));
}

#[test]
fn epp_excluded_when_pstate_not_active() {
    let root = TmpRoot::new();
    fixture(&root);
    root.write("sys/devices/system/cpu/amd_pstate/status", "passive\n");
    let plan = engine(&root, FakeSystemCtl::default()).plan(PowerState::Battery).unwrap();
    let ex = plan.excluded.iter().find(|a| a.id == "knob:epp").unwrap();
    assert!(ex.note.as_deref().unwrap().contains("active mode"));
}

// ---------------------------------------------------------------------------
// Apply / verify / revert
// ---------------------------------------------------------------------------

#[test]
fn dry_run_applies_nothing() {
    let root = TmpRoot::new();
    fixture(&root);
    let mut eng = engine(&root, FakeSystemCtl::default());
    let res = eng.apply(PowerState::Battery, true).unwrap();
    assert!(res.dry_run);
    assert_eq!(res.txn_id, "dry-run");
    assert!(!res.applied.is_empty());
    // Nothing changed on disk.
    assert_eq!(root.read("sys/module/snd_hda_intel/parameters/power_save"), "0");
    // No journal written.
    assert!(!root.path.join("run/drowse").exists());
}

#[test]
fn apply_writes_journals_verifies_and_revert_restores() {
    let root = TmpRoot::new();
    fixture(&root);
    let mut eng = engine(&root, FakeSystemCtl::default());

    let res = eng.apply(PowerState::Battery, false).unwrap();
    assert!(!res.dry_run);
    assert_ne!(res.txn_id, "dry-run");
    // Real sysfs knobs took.
    assert_eq!(root.read("sys/module/snd_hda_intel/parameters/power_save"), "10");
    assert_eq!(
        root.read("sys/devices/system/cpu/cpu0/cpufreq/energy_performance_preference"),
        "power"
    );
    assert_eq!(
        root.read("sys/devices/system/cpu/cpu1/cpufreq/energy_performance_preference"),
        "power"
    );
    assert_eq!(root.read("sys/firmware/acpi/platform_profile"), "low-power");
    assert_eq!(root.read("sys/bus/usb/devices/1-1/power/control"), "auto");
    assert!(res.applied.iter().any(|a| a.id == "knob:audio-powersave"));
    assert!(res.failed.is_empty());

    // Journal exists.
    let jpath = root.path.join(format!("run/drowse/txn-{}.json", res.txn_id));
    assert!(jpath.exists());

    // Revert restores every prior value.
    let rev = eng.revert(&res.txn_id).unwrap();
    assert!(!rev.already_reverted);
    assert_eq!(root.read("sys/module/snd_hda_intel/parameters/power_save"), "0");
    assert_eq!(
        root.read("sys/devices/system/cpu/cpu0/cpufreq/energy_performance_preference"),
        "performance"
    );
    assert_eq!(root.read("sys/firmware/acpi/platform_profile"), "balanced");
    assert_eq!(root.read("sys/bus/usb/devices/1-1/power/control"), "on");

    // Second revert is a no-op.
    let rev2 = eng.revert(&res.txn_id).unwrap();
    assert!(rev2.already_reverted);
    assert!(rev2.reverted.is_empty());
}

#[test]
fn revert_unknown_txn_is_already_reverted() {
    let root = TmpRoot::new();
    fixture(&root);
    let mut eng = engine(&root, FakeSystemCtl::default());
    let rev = eng.revert("does-not-exist").unwrap();
    assert!(rev.already_reverted);
    assert!(rev.reverted.is_empty());
}

#[test]
fn drop_restores_unreverted_transaction() {
    let root = TmpRoot::new();
    fixture(&root);
    let txn_id;
    {
        let mut eng = engine(&root, FakeSystemCtl::default());
        let res = eng.apply(PowerState::Battery, false).unwrap();
        txn_id = res.txn_id;
        assert_eq!(root.read("sys/module/snd_hda_intel/parameters/power_save"), "10");
        // drop without revert
    }
    // Drop replayed the journal in reverse.
    assert_eq!(root.read("sys/module/snd_hda_intel/parameters/power_save"), "0");
    // Journal marked reverted.
    let jtext = std::fs::read_to_string(root.path.join(format!("run/drowse/txn-{txn_id}.json")))
        .unwrap();
    assert!(jtext.contains("\"reverted\": true"));
}

#[test]
fn verify_catches_regression_and_auto_reverts() {
    let root = TmpRoot::new();
    fixture(&root);
    // Point the audio knob at /dev/null: the write SUCCEEDS but the read-back
    // comes back empty (!= "10"), so verify-after-apply flags a regression and
    // auto-reverts just this action into `failed`.
    let p = root.path.join("sys/module/snd_hda_intel/parameters/power_save");
    std::fs::remove_file(&p).unwrap();
    std::os::unix::fs::symlink("/dev/null", &p).unwrap();

    let mut eng = engine(&root, FakeSystemCtl::default());
    let res = eng.apply(PowerState::Battery, false).unwrap();

    let audio = res.failed.iter().find(|a| a.id == "knob:audio-powersave").unwrap();
    assert!(audio.note.as_deref().unwrap().contains("auto-reverted"));
    // Unrelated knobs still applied fine.
    assert!(res.applied.iter().any(|a| a.id == "knob:epp"));
}

#[test]
fn write_failure_lands_in_failed_with_note() {
    let root = TmpRoot::new();
    fixture(&root);
    // Read-only audio file -> owner gets EACCES on write (tests run unprivileged).
    use std::os::unix::fs::PermissionsExt;
    let p = root.path.join("sys/module/snd_hda_intel/parameters/power_save");
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o444)).unwrap();

    let mut eng = engine(&root, FakeSystemCtl::default());
    let res = eng.apply(PowerState::Battery, false).unwrap();
    let audio = res.failed.iter().find(|a| a.id == "knob:audio-powersave").unwrap();
    assert!(audio.note.as_deref().unwrap().contains("permission denied"));
}

// ---------------------------------------------------------------------------
// Elastic-service caps (the novel part)
// ---------------------------------------------------------------------------

#[test]
fn elastic_cap_plans_applies_and_reverts() {
    let root = TmpRoot::new();
    fixture(&root);
    root.write("etc/drowse/policy.toml", "syncthing.service = elastic\n");
    let sc = FakeSystemCtl::with_unit("syncthing.service");
    let mut eng = engine(&root, sc);

    // Planned on battery.
    let plan = eng.plan(PowerState::Battery).unwrap();
    let cap = plan.actions.iter().find(|a| a.id == "cap:syncthing.service").unwrap();
    assert_eq!(cap.kind, "cgroup");
    assert_eq!(cap.tier, Tier::T1);
    assert!(cap.to.as_deref().unwrap().contains("CPUQuota=10%"));

    // Apply sets the properties.
    let res = eng.apply(PowerState::Battery, false).unwrap();
    assert!(res.applied.iter().any(|a| a.id == "cap:syncthing.service"));

    // Revert clears them (fake restores defaults).
    let rev = eng.revert(&res.txn_id).unwrap();
    assert!(rev.reverted.iter().any(|a| a.id == "cap:syncthing.service"));
}

#[test]
fn cap_excluded_when_unit_absent() {
    let root = TmpRoot::new();
    fixture(&root);
    root.write("etc/drowse/policy.toml", "ghost.service = elastic\n");
    let eng = engine(&root, FakeSystemCtl::default()); // no units loaded
    let plan = eng.plan(PowerState::Battery).unwrap();
    let ex = plan.excluded.iter().find(|a| a.id == "cap:ghost.service").unwrap();
    assert_eq!(ex.note.as_deref(), Some("unit not loaded"));
}

#[test]
fn freeze_class_gated_behind_opt_in() {
    let root = TmpRoot::new();
    fixture(&root);
    root.write("etc/drowse/policy.toml", "syncthing.service = freeze\n");
    let plan = engine(&root, FakeSystemCtl::with_unit("syncthing.service"))
        .plan(PowerState::Battery)
        .unwrap();
    let ex = plan.excluded.iter().find(|a| a.id == "cap:syncthing.service").unwrap();
    assert_eq!(ex.tier, Tier::T2);
    assert!(ex.note.as_deref().unwrap().contains("opt_in_t2"));
}

// ---------------------------------------------------------------------------
// Live test: ONE real, reversible apply+revert under sudo. Ignored by default.
// Run with:  sudo -E cargo test -p drowse-policy -- --ignored live_audio
// It applies the audio power_save knob against real sysfs and restores it.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "mutates real sysfs; run manually under sudo"]
fn live_audio_apply_revert() {
    let path = "/sys/module/snd_hda_intel/parameters/power_save";
    let before = std::fs::read_to_string(path).expect("no HDA power_save on this host").trim().to_string();

    let mut eng = Engine::new(PathBuf::from("/"), RealSystemCtl::new());
    // Plan battery; find the audio action (skip if already at target).
    let plan = eng.plan(PowerState::Battery).unwrap();
    if !plan.actions.iter().any(|a| a.id == "knob:audio-powersave") {
        eprintln!("audio already at battery target ({before}); nothing to do");
        return;
    }

    let res = eng.apply(PowerState::Battery, false).unwrap();
    let applied_audio = res.applied.iter().any(|a| a.id == "knob:audio-powersave");
    // Always revert, even if the assertion below would fail.
    let rev = eng.revert(&res.txn_id).unwrap();

    let after = std::fs::read_to_string(path).unwrap().trim().to_string();
    assert_eq!(after, before, "machine must be restored to its original state");
    assert!(applied_audio, "audio knob should have applied under sudo");
    assert!(!rev.already_reverted);
}
