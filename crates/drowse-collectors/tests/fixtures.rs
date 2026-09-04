//! Fixture-tree tests: every collector is driven against a synthetic sysfs
//! under a tempdir. No external test deps.

use drowse_api::SourceState;
use drowse_core::{Collector, Kind, Probe, Unit};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

// probe_all order (part of the crate contract).
const CPUIDLE: usize = 0;
const BATTERY: usize = 1;
const RAPL: usize = 2;
const AMDGPU: usize = 3;

static SEQ: AtomicU32 = AtomicU32::new(0);

/// Fresh fixture root under the OS tempdir; unique per test.
fn fixture() -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "drowse-collectors-test-{}-{n}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Write `contents` at `root/rel`, creating parents.
fn w(root: &Path, rel: &str, contents: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, contents).unwrap();
}

fn ready(p: Probe) -> Box<dyn Collector> {
    match p {
        Probe::Ready(c) => c,
        Probe::Unavailable { name, status } => {
            panic!("{name} unexpectedly unavailable: {:?}/{:?}", status.state, status.detail)
        }
    }
}

fn unavailable_state(p: &Probe) -> SourceState {
    match p {
        Probe::Ready(_) => panic!("expected Unavailable"),
        Probe::Unavailable { status, .. } => status.state,
    }
}

fn sample(c: &mut dyn Collector) -> Vec<u64> {
    let mut out = vec![0u64; c.fields().len()];
    c.sample(&mut out).unwrap();
    out
}

fn add_cpuidle(root: &Path) {
    for (cpu, mult) in [(0u32, 1u64), (1, 10)] {
        for (s, name, t, u) in [
            (0u32, "POLL", 111u64, 5u64),
            (1, "C1", 2_222, 40),
            (2, "C3", 33_333, 300),
        ] {
            let d = format!("sys/devices/system/cpu/cpu{cpu}/cpuidle/state{s}");
            w(root, &format!("{d}/name"), &format!("{name}\n"));
            w(root, &format!("{d}/time"), &format!("{}\n", t * mult));
            w(root, &format!("{d}/usage"), &format!("{}\n", u * mult));
        }
    }
}

fn add_battery(root: &Path) {
    w(root, "sys/class/power_supply/BATT/type", "Battery\n");
    w(root, "sys/class/power_supply/BATT/status", "Discharging\n");
    w(root, "sys/class/power_supply/BATT/power_now", "4500000\n");
    w(root, "sys/class/power_supply/BATT/capacity", "88\n");
}

fn add_rapl(root: &Path) {
    w(root, "sys/class/powercap/intel-rapl:0/name", "package-0\n");
    w(root, "sys/class/powercap/intel-rapl:0/energy_uj", "5000\n");
    w(
        root,
        "sys/class/powercap/intel-rapl:0/max_energy_range_uj",
        "65532610987\n",
    );
}

fn add_amdgpu(root: &Path) {
    // card0 has no busy file (e.g. simpledrm); card1 is the real GPU.
    fs::create_dir_all(root.join("sys/class/drm/card0/device")).unwrap();
    w(root, "sys/class/drm/card1/device/gpu_busy_percent", "7\n");
}

// ---------------------------------------------------------------------------
// cpuidle
// ---------------------------------------------------------------------------

#[test]
fn cpuidle_fields_and_values() {
    let root = fixture();
    add_cpuidle(&root);
    let mut c = ready(drowse_collectors::probe_all(&root).remove(CPUIDLE));

    let names: Vec<&str> = c.fields().iter().map(|f| f.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "cpu0.POLL.time_us", "cpu0.POLL.usage",
            "cpu0.C1.time_us", "cpu0.C1.usage",
            "cpu0.C3.time_us", "cpu0.C3.usage",
            "cpu1.POLL.time_us", "cpu1.POLL.usage",
            "cpu1.C1.time_us", "cpu1.C1.usage",
            "cpu1.C3.time_us", "cpu1.C3.usage",
        ]
    );
    for f in c.fields() {
        assert_eq!(f.kind, Kind::Counter);
        let want = if f.name.ends_with("time_us") { Unit::Micros } else { Unit::Count };
        assert_eq!(f.unit, want, "{}", f.name);
    }

    let out = sample(c.as_mut());
    assert_eq!(out, [111, 5, 2222, 40, 33333, 300, 1110, 50, 22220, 400, 333330, 3000]);

    // Counters advance: kept-fd pread must see fresh contents.
    w(&root, "sys/devices/system/cpu/cpu0/cpuidle/state0/time", "200\n");
    let out = sample(c.as_mut());
    assert_eq!(out[0], 200);
}

#[test]
fn cpuidle_missing_tree_is_not_present() {
    let root = fixture();
    let probes = drowse_collectors::probe_all(&root);
    assert_eq!(unavailable_state(&probes[CPUIDLE]), SourceState::NotPresent);
}

// ---------------------------------------------------------------------------
// battery
// ---------------------------------------------------------------------------

#[test]
fn battery_direct_power_now() {
    let root = fixture();
    add_battery(&root);
    let mut c = ready(drowse_collectors::probe_all(&root).remove(BATTERY));

    let names: Vec<&str> = c.fields().iter().map(|f| f.name.as_str()).collect();
    assert_eq!(
        names,
        ["BATT.power_now_uw", "BATT.capacity_pct", "BATT.discharging", "BATT.valid"]
    );
    assert!(c.fields().iter().all(|f| f.kind == Kind::Gauge));

    assert_eq!(sample(c.as_mut()), [4_500_000, 88, 1, 1]);

    w(&root, "sys/class/power_supply/BATT/status", "Charging\n");
    assert_eq!(sample(c.as_mut()), [4_500_000, 88, 0, 1]);
}

#[test]
fn battery_computed_from_current_and_voltage() {
    let root = fixture();
    w(&root, "sys/class/power_supply/BATT/type", "Battery\n");
    w(&root, "sys/class/power_supply/BATT/status", "Discharging\n");
    w(&root, "sys/class/power_supply/BATT/current_now", "2000000\n"); // 2 A
    w(&root, "sys/class/power_supply/BATT/voltage_now", "15000000\n"); // 15 V
    w(&root, "sys/class/power_supply/BATT/capacity", "50\n");

    let mut c = ready(drowse_collectors::probe_all(&root).remove(BATTERY));
    assert_eq!(sample(c.as_mut()), [30_000_000, 50, 1, 1]); // 30 W
}

#[test]
fn battery_absent_values_keep_last_good_and_clear_valid() {
    let root = fixture();
    add_battery(&root);
    let mut c = ready(drowse_collectors::probe_all(&root).remove(BATTERY));
    assert_eq!(sample(c.as_mut()), [4_500_000, 88, 1, 1]);

    // EC hiccup: status and power read back empty. Tick must not fail.
    w(&root, "sys/class/power_supply/BATT/status", "");
    w(&root, "sys/class/power_supply/BATT/power_now", "");
    assert_eq!(sample(c.as_mut()), [4_500_000, 88, 1, 0]);
}

#[test]
fn battery_ignores_mains_and_reports_not_present() {
    let root = fixture();
    w(&root, "sys/class/power_supply/ACAD/type", "Mains\n");
    let probes = drowse_collectors::probe_all(&root);
    assert_eq!(unavailable_state(&probes[BATTERY]), SourceState::NotPresent);
}

// ---------------------------------------------------------------------------
// rapl
// ---------------------------------------------------------------------------

#[test]
fn rapl_counter_and_wrap_range_gauge() {
    let root = fixture();
    add_rapl(&root);
    let mut c = ready(drowse_collectors::probe_all(&root).remove(RAPL));

    let f = c.fields();
    assert_eq!(f[0].name, "package-0.energy_uj");
    assert_eq!((f[0].kind, f[0].unit), (Kind::Counter, Unit::MicroJoules));
    assert_eq!(f[1].name, "package-0.max_energy_range_uj");
    assert_eq!((f[1].kind, f[1].unit), (Kind::Gauge, Unit::MicroJoules));

    assert_eq!(sample(c.as_mut()), [5000, 65_532_610_987]);
    w(&root, "sys/class/powercap/intel-rapl:0/energy_uj", "6200\n");
    assert_eq!(sample(c.as_mut()), [6200, 65_532_610_987]);
}

#[test]
fn rapl_missing_zone_is_not_present() {
    let root = fixture();
    let probes = drowse_collectors::probe_all(&root);
    assert_eq!(unavailable_state(&probes[RAPL]), SourceState::NotPresent);
}

#[test]
fn rapl_eacces_classified_permission_denied() {
    use std::os::unix::fs::PermissionsExt;
    if rustix_is_root() {
        eprintln!("skipped: running as root, EACCES cannot be provoked");
        return;
    }
    let root = fixture();
    add_rapl(&root);
    let energy = root.join("sys/class/powercap/intel-rapl:0/energy_uj");
    fs::set_permissions(&energy, fs::Permissions::from_mode(0o000)).unwrap();

    let probes = drowse_collectors::probe_all(&root);
    assert_eq!(unavailable_state(&probes[RAPL]), SourceState::PermissionDenied);

    // Restore so the tempdir can be cleaned by hand later.
    let _ = fs::set_permissions(&energy, fs::Permissions::from_mode(0o644));
}

fn rustix_is_root() -> bool {
    // No rustix dev-dep needed: root can read a 0o000 file, so detect euid 0
    // portably via the effective ability that matters here.
    unsafe_geteuid() == 0
}

fn unsafe_geteuid() -> u32 {
    // std has no geteuid; /proc/self is the no-dependency route.
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                l.strip_prefix("Uid:")
                    .and_then(|r| r.split_whitespace().nth(1))
                    .and_then(|e| e.parse().ok())
            })
        })
        .unwrap_or(u32::MAX)
}

// ---------------------------------------------------------------------------
// amdgpu
// ---------------------------------------------------------------------------

#[test]
fn amdgpu_first_card_with_attribute() {
    let root = fixture();
    add_amdgpu(&root);
    let mut c = ready(drowse_collectors::probe_all(&root).remove(AMDGPU));

    let f = &c.fields()[0];
    assert_eq!(f.name, "card1.gpu_busy_percent");
    assert_eq!((f.kind, f.unit), (Kind::Gauge, Unit::Percent));
    assert_eq!(sample(c.as_mut()), [7]);
}

#[test]
fn amdgpu_missing_is_not_present() {
    let root = fixture();
    fs::create_dir_all(root.join("sys/class/drm/card0/device")).unwrap();
    let probes = drowse_collectors::probe_all(&root);
    assert_eq!(unavailable_state(&probes[AMDGPU]), SourceState::NotPresent);
}

// ---------------------------------------------------------------------------
// whole tree
// ---------------------------------------------------------------------------

#[test]
fn probe_all_full_fixture_all_ready() {
    let root = fixture();
    add_cpuidle(&root);
    add_battery(&root);
    add_rapl(&root);
    add_amdgpu(&root);
    let probes = drowse_collectors::probe_all(&root);
    assert_eq!(probes.len(), 4);
    assert!(probes.iter().all(|p| matches!(p, Probe::Ready(_))));
}

/// Smoke check against the live machine; run explicitly:
/// `cargo test -p drowse-collectors -- --ignored`
/// Expects (per the M1 bring-up contract on the reference GPD Pocket 4):
/// cpuidle, battery, amdgpu Ready; rapl PermissionDenied when unprivileged.
#[test]
#[ignore = "requires the reference machine's real sysfs"]
fn smoke_real_machine() {
    assert_ne!(unsafe_geteuid(), 0, "run the smoke test unprivileged");
    let mut probes = drowse_collectors::probe_all(Path::new("/"));
    assert_eq!(unavailable_state(&probes[RAPL]), SourceState::PermissionDenied);
    for idx in [AMDGPU, BATTERY, CPUIDLE] {
        let mut c = ready(probes.remove(idx));
        let out = sample(c.as_mut());
        eprintln!("{}: {} fields, first = {:?} -> {:?}", c.name(), c.fields().len(),
                  c.fields().first().map(|f| f.name.as_str()), out.first());
        assert!(!out.is_empty());
    }
}
