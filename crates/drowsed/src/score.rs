//! Scorecard assembly: pure functions from recorded tick data to the wire
//! [`Scorecard`]. No I/O, no clocks — everything is passed in, so the whole
//! module is testable against fake engines.

use drowse_api::{
    Battery, CpuIdle, Grade, Meta, Score, Scorecard, SourceState, SourceStatus, SCHEMA,
};
use drowse_core::{Engine, Kind, Unit};
use std::collections::BTreeMap;

/// Host facts captured once at startup.
#[derive(Debug, Clone)]
pub struct SysInfo {
    pub hostname: String,
    pub kernel: String,
}

/// Owned copy of the engine's field layout, captured once after probe.
/// Index-aligned with [`TickRecord::values`].
#[derive(Clone)]
pub struct FieldInfo {
    pub collector: &'static str,
    pub name: String,
    pub kind: Kind,
    pub unit: Unit,
}

pub fn layout_of(engine: &Engine) -> Vec<FieldInfo> {
    engine
        .values()
        .map(|s| FieldInfo {
            collector: s.collector,
            name: s.field.name.clone(),
            kind: s.field.kind,
            unit: s.field.unit,
        })
        .collect()
}

/// One completed tick: counter deltas / gauge values, index-aligned with the
/// layout. `None` = field invalid this tick (counter went backwards).
pub struct TickRecord {
    pub unix_ms: u64,
    pub elapsed_ns: u64,
    pub values: Vec<Option<u64>>,
}

pub fn record(engine: &Engine, unix_ms: u64) -> TickRecord {
    TickRecord {
        unix_ms,
        elapsed_ns: engine.elapsed_ns(),
        values: engine.values().map(|s| s.value).collect(),
    }
}

/// Aggregate `records` (chronological, non-empty) into a scorecard.
///
/// Counters are summed over the ticks where they were valid and rated against
/// the wall time of those same ticks; gauges take their most recent valid
/// value. A field with no valid tick in the window is omitted (and its
/// collector reported Degraded), never emitted as zero.
pub fn assemble(
    layout: &[FieldInfo],
    records: &[&TickRecord],
    unavailable: &BTreeMap<String, SourceStatus>,
    sysinfo: &SysInfo,
) -> Scorecard {
    let n = layout.len();
    let total_ns: u64 = records.iter().map(|r| r.elapsed_ns).sum();

    let mut sum = vec![0u64; n];
    let mut valid_ns = vec![0u64; n];
    let mut valid_ticks = vec![0u32; n];
    let mut last_gauge: Vec<Option<u64>> = vec![None; n];
    for r in records {
        for i in 0..n {
            if let Some(v) = r.values.get(i).copied().flatten() {
                sum[i] += v;
                valid_ns[i] += r.elapsed_ns;
                valid_ticks[i] += 1;
                last_gauge[i] = Some(v);
            }
        }
    }

    // ---- cpuidle: "cpuN.<STATE>.time_us" counters -------------------------
    #[derive(Default)]
    struct CpuAcc {
        states: BTreeMap<String, f64>,
        order: Vec<String>, // layout order; last = deepest advertised state
    }
    let mut cpus: BTreeMap<u32, CpuAcc> = BTreeMap::new();
    for (i, f) in layout.iter().enumerate() {
        if f.collector != "cpuidle" || f.kind != Kind::Counter {
            continue;
        }
        let parts: Vec<&str> = f.name.splitn(3, '.').collect();
        if parts.len() != 3 || parts[2] != "time_us" {
            continue;
        }
        let Some(cpu) = parts[0].strip_prefix("cpu").and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let acc = cpus.entry(cpu).or_default();
        acc.order.push(parts[1].to_string());
        if valid_ns[i] > 0 {
            // sum is µs; valid_ns is ns.
            let pct = (sum[i] as f64 * 1_000.0) / valid_ns[i] as f64 * 100.0;
            acc.states.insert(parts[1].to_string(), pct.clamp(0.0, 100.0));
        }
    }

    // SE (M1): mean deepest-state residency across CPUs, basis "os-timed-mean".
    let mut deepest_name = String::from("unknown");
    let mut se_sum = 0.0;
    let mut se_n = 0u32;
    for acc in cpus.values() {
        if let Some(deep) = acc.order.last() {
            if deepest_name == "unknown" {
                deepest_name = deep.clone();
            }
            if let Some(pct) = acc.states.get(deep) {
                se_sum += pct;
                se_n += 1;
            }
        }
    }
    let sleep_efficiency = (se_n > 0).then(|| se_sum / se_n as f64);
    let grade = sleep_efficiency.map(Grade::from_se);

    // ---- battery gauges ---------------------------------------------------
    let mut power_uw: Option<u64> = None;
    let mut discharging: Option<bool> = None;
    let mut capacity: Option<f64> = None;
    let mut energy: Option<(Unit, u64)> = None;
    for (i, f) in layout.iter().enumerate() {
        if f.collector != "battery" || f.kind != Kind::Gauge {
            continue;
        }
        let Some(v) = last_gauge[i] else { continue };
        match f.unit {
            Unit::MicroWatts if f.name.contains("power") => power_uw = Some(v),
            Unit::Bool if f.name.contains("discharg") => discharging = Some(v != 0),
            Unit::Percent if f.name.contains("capacity") => capacity = Some(v as f64),
            _ if f.name.contains("energy") => energy = Some((f.unit, v)),
            _ => {}
        }
    }
    let battery = power_uw.map(|uw| {
        let power_w = uw as f64 / 1e6;
        Battery {
            power_w,
            discharging: discharging.unwrap_or(power_w > 0.05),
            capacity_pct: capacity,
        }
    });
    let idle_draw_w = battery.as_ref().filter(|b| b.discharging).map(|b| b.power_w);
    let runtime_est_min = match (idle_draw_w, energy, power_uw) {
        (Some(_), Some((unit, e)), Some(uw)) if uw > 0 => {
            let minutes = match unit {
                // µJ / µW = seconds
                Unit::MicroJoules => e as f64 / uw as f64 / 60.0,
                // anything else: assume power_supply µWh; µWh / µW = hours
                _ => e as f64 / uw as f64 * 60.0,
            };
            Some(minutes.round().max(0.0) as u64)
        }
        _ => None,
    };

    // ---- RAPL package counters: "package-N.energy_uj" ---------------------
    let mut pkg_w = 0.0;
    let mut pkg_any = false;
    for (i, f) in layout.iter().enumerate() {
        if f.collector == "rapl"
            && f.kind == Kind::Counter
            && f.unit == Unit::MicroJoules
            && f.name.starts_with("package")
            && f.name.ends_with(".energy_uj")
            && f.name.matches('.').count() == 1 // package level, not subdomain
            && valid_ns[i] > 0
        {
            // µJ / ns = kW; ×1000 → W
            pkg_w += sum[i] as f64 / valid_ns[i] as f64 * 1_000.0;
            pkg_any = true;
        }
    }

    // ---- sources ----------------------------------------------------------
    let mut sources = unavailable.clone();
    let mut collector_totals: BTreeMap<&'static str, (u32, u32)> = BTreeMap::new();
    for (i, f) in layout.iter().enumerate() {
        let e = collector_totals.entry(f.collector).or_insert((0, 0));
        e.0 += 1;
        if valid_ticks[i] == 0 {
            e.1 += 1;
        }
    }
    let mut any_degraded = false;
    for (name, (total, invalid)) in collector_totals {
        let status = if invalid == 0 {
            SourceStatus::ok()
        } else {
            any_degraded = true;
            SourceStatus::err(
                SourceState::Degraded,
                format!("{invalid}/{total} fields had no valid sample in window"),
            )
        };
        sources.insert(name.to_string(), status);
    }

    Scorecard {
        schema: SCHEMA,
        kind: "scorecard".into(),
        meta: Meta {
            hostname: sysinfo.hostname.clone(),
            kernel: sysinfo.kernel.clone(),
            sampled_at_unix_ms: records.last().map(|r| r.unix_ms).unwrap_or(0),
            window_s: total_ns as f64 / 1e9,
            ticks: records.len() as u32,
            degraded: any_degraded || !unavailable.is_empty(),
        },
        sources,
        score: Score {
            sleep_efficiency,
            grade,
            deepest_state: deepest_name,
            basis: "os-timed-mean".into(),
            idle_draw_w,
            runtime_est_min,
        },
        cpu: cpus
            .into_iter()
            .map(|(cpu, acc)| CpuIdle { cpu, states: acc.states })
            .collect(),
        battery,
        package_power_w: pkg_any.then_some(pkg_w),
    }
}

// ---------------------------------------------------------------------------
// Attribution assembly: the M2 payload of `offenders.get`. Pure, like
// `assemble` above — RawOffenders + a window in, wire `Offenders` out, so it
// unit-tests without eBPF.
// ---------------------------------------------------------------------------

use drowse_api::{Offender, OffenderKind, Offenders};
use drowse_core::RawOffender;

/// Id prefix per kind: "proc"/"timer"/"irq"/"softirq"/"workqueue".
fn kind_slug(k: OffenderKind) -> &'static str {
    match k {
        OffenderKind::Process => "proc",
        OffenderKind::Timer => "timer",
        OffenderKind::Irq => "irq",
        OffenderKind::Softirq => "softirq",
        OffenderKind::Workqueue => "workqueue",
    }
}

/// counts ÷ window; 0 when the window is empty (never divide by zero).
fn rate(count: u64, window_s: f64) -> f64 {
    if window_s > 0.0 { count as f64 / window_s } else { 0.0 }
}

/// Human rate: whole numbers at a glance, a decimal only when sub-10/s.
fn fmt_rate(r: f64) -> String {
    if r >= 10.0 {
        format!("{r:.0}x/s")
    } else {
        format!("{r:.1}x/s")
    }
}

/// Assemble the ranked offender table. `window_ns` is the wall time the
/// `raw` counts accumulated over; `source` is the attribution subsystem's own
/// availability (ok when eBPF loaded, else the probe's typed reason). When the
/// subsystem is unavailable `raw` is empty and the result is a valid, empty
/// table — never an error.
#[allow(clippy::too_many_arguments)]
/// A rough, honestly-labeled battery cost for an offender, from its package-
/// sleep-break rate. Model: each break costs ~40us of lost deep idle at a
/// ~1.5W light-vs-deep delta; against a ~4W idle draw on a ~40Wh battery
/// (~10h = 600min base), cost_min ≈ pkg_breaks_per_s × 0.009. The low/high
/// span a 20–80us per-break assumption. Returns None below a noise floor.
fn battery_cost(pkg_breaks_per_s: f64) -> Option<drowse_api::Estimate> {
    if pkg_breaks_per_s < 5.0 {
        return None;
    }
    Some(drowse_api::Estimate {
        mid: pkg_breaks_per_s * 0.009,
        low: pkg_breaks_per_s * 0.0045,
        high: pkg_breaks_per_s * 0.018,
        basis: "rough".into(),
    })
}

pub fn offenders(
    raw: &[RawOffender],
    observer_wakeups: u64,
    window_ns: u64,
    sampled_at_unix_ms: u64,
    source: &SourceStatus,
    sysinfo: &SysInfo,
    limit: usize,
) -> Offenders {
    let window_s = window_ns as f64 / 1e9;
    let total_wakeups: u64 = raw.iter().map(|o| o.wakeups).sum();

    let mut rows: Vec<Offender> = raw
        .iter()
        .map(|o| {
            let id = match (o.kind, o.pid) {
                (OffenderKind::Process, Some(pid)) => format!("proc:{pid}:{}", o.name),
                _ => format!("{}:{}", kind_slug(o.kind), o.key),
            };
            let w = rate(o.wakeups, window_s);
            let p = rate(o.pkg_breaks, window_s);
            let summary = match (o.kind, o.pid) {
                (OffenderKind::Process, Some(pid)) => format!(
                    "{} (pid {pid}) wakes the CPU {}, breaks package sleep {}",
                    o.name,
                    fmt_rate(w),
                    fmt_rate(p)
                ),
                _ => format!(
                    "{} wakes the CPU {}, breaks package sleep {}",
                    o.name,
                    fmt_rate(w),
                    fmt_rate(p)
                ),
            };
            Offender {
                id,
                kind: o.kind,
                summary,
                wakeups_per_s: w,
                idle_breaks_per_s: rate(o.idle_breaks, window_s),
                pkg_breaks_per_s: p,
                pid: o.pid,
                share: if total_wakeups > 0 {
                    o.wakeups as f64 / total_wakeups as f64
                } else {
                    0.0
                },
                // Filled by the measure->fix integration (gap #2).
                battery_cost_min: battery_cost(p),
                // Linked to fixes by the offenders.get handler (which has the
                // policy plan); the pure assembler can't see it.
                action_ids: Vec::new(),
            }
        })
        .collect();

    // Rank by battery cost: package-sleep breaks first, then raw wakeups.
    // (hrtimer_wakeup dominates raw wakeups; pkg_breaks is what costs battery.)
    rows.sort_by(|a, b| {
        b.pkg_breaks_per_s
            .total_cmp(&a.pkg_breaks_per_s)
            .then(b.wakeups_per_s.total_cmp(&a.wakeups_per_s))
    });
    rows.truncate(limit);

    Offenders {
        schema: SCHEMA,
        kind: "offenders".into(),
        meta: Meta {
            hostname: sysinfo.hostname.clone(),
            kernel: sysinfo.kernel.clone(),
            sampled_at_unix_ms,
            window_s,
            ticks: 1,
            degraded: source.state != SourceState::Ok,
        },
        source: source.clone(),
        offenders: rows,
        observer_wakeups_per_s: Some(rate(observer_wakeups, window_s)),
        window_ms_mean: None,
    }
}
