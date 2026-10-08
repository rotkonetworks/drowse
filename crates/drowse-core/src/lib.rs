//! Sampling engine and service stack. Mechanism only — no policy, no I/O
//! besides what a [`Collector`] does behind its own fds.

pub mod service;

use std::collections::BTreeMap;
use std::io;

// ---------------------------------------------------------------------------
// Collector contract
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Monotonic counter; the engine deltas it. A backwards value invalidates
    /// the tick (hotplug / re-registration), it is never clamped to zero.
    Counter,
    /// Instantaneous value; passed through as sampled.
    Gauge,
}

/// Latency class of a collector's `sample()` call. Governs which thread reads
/// it: `Fast` collectors run on the ppoll sampler every tick; `Slow` ones run
/// on the daemon's slow lane so their stall never blocks the fast tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Latency {
    /// Cheap, memory-backed sysfs (cpuidle, amdgpu busy%, RAPL energy_uj).
    Fast,
    /// High-latency or hardware-waking (EC battery gauges, SMU mailbox): the
    /// read stalls the calling thread and must run on the slow lane.
    Slow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Micros,
    MicroJoules,
    MicroWatts,
    Count,
    Percent,
    Bool,
}

#[derive(Debug, Clone)]
pub struct Field {
    /// Stable dotted name, e.g. "cpu3.C3.time_us". Interned at probe.
    pub name: String,
    pub kind: Kind,
    pub unit: Unit,
}

/// A collector opens everything it needs at construction ("probe") and then
/// only ever `pread`s on kept fds. `sample` must not allocate or open.
pub trait Collector: Send {
    fn name(&self) -> &'static str;
    /// Field layout; fixed for the collector's lifetime.
    fn fields(&self) -> &[Field];
    /// Write one value per field into `out` (`out.len() == fields().len()`).
    fn sample(&mut self, out: &mut [u64]) -> io::Result<()>;
    /// Sample every Nth engine tick (default: every tick). A collector whose
    /// read is expensive or wakes hardware (EC-backed battery attributes,
    /// anything firmware-mediated) declares a sparser cadence; on skipped
    /// ticks its previous values carry forward (gauges persist, counter
    /// deltas read 0).
    fn cadence(&self) -> u32 {
        1
    }
    /// Latency class. `Slow` collectors are moved to the daemon's slow lane so
    /// their stalling `sample()` never blocks the fast sampler thread — the
    /// real battery-safety property. Defaults to `Fast`.
    fn latency(&self) -> Latency {
        Latency::Fast
    }
}

/// Probe outcome: an unavailable source is a value, not an error.
pub enum Probe {
    Ready(Box<dyn Collector>),
    Unavailable { name: &'static str, status: drowse_api::SourceStatus },
}

// ---------------------------------------------------------------------------
// Attribution seam (M2): a long-running in-kernel counter, drained on a slow
// cadence. NOT a Collector — it is not pread-per-tick sampling. The daemon
// owns rate conversion, ranking, and id assignment; the implementation (eBPF,
// in drowse-ebpf) owns kernel attribution and symbol resolution.
// ---------------------------------------------------------------------------

/// One drained attribution bucket: counts accumulated since the last drain.
pub struct RawOffender {
    pub kind: drowse_api::OffenderKind,
    /// Stable identity within kind (pid as text, callback symbol, irq line).
    pub key: String,
    /// Display name: comm, symbol, or irq handler.
    pub name: String,
    pub pid: Option<u32>,
    /// Deltas since the last drain.
    pub wakeups: u64,
    pub idle_breaks: u64,
    pub pkg_breaks: u64,
}

/// Everything drained in one pass.
pub struct AttributionDelta {
    pub offenders: Vec<RawOffender>,
    /// Drowse's own wakeups over the interval, kept honest.
    pub observer_wakeups: u64,
    /// Nanoseconds in this interval during which ALL cpus were simultaneously
    /// idle — the honest package-sleep residency. 0 when the implementation
    /// doesn't track it (the daemon then falls back to os-timed-mean SE).
    pub all_cores_idle_ns: u64,
}

/// A running wakeup-attribution subsystem. `drain` returns the counts since the
/// previous call; the daemon divides by elapsed time for rates. Draining is
/// the only userspace/kernel crossing — no per-event traffic.
pub trait Attributor: Send {
    fn drain(&mut self) -> io::Result<AttributionDelta>;
}

/// Probe outcome for attribution (needs CAP_BPF + a BTF kernel).
pub enum AttributorProbe {
    Ready(Box<dyn Attributor>),
    Unavailable(drowse_api::SourceStatus),
}

// ---------------------------------------------------------------------------
// Policy seam (M3): the act layer. A PolicyEngine plans, applies, and reverts
// reversible system changes transactionally. Implementation (sysfs/cgroup/dbus
// knobs, the journal, verify-after-apply) lives in drowse-policy; the daemon
// owns it and exposes policy.plan/apply/revert. Kept minimal and object-safe.
// ---------------------------------------------------------------------------

/// AC vs battery — the trigger context a plan is computed for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerState {
    Battery,
    Ac,
}

/// The act subsystem. All methods are fallible; a failed apply must leave the
/// system restorable via the journal (replayed by `revert` or a restore unit).
pub trait PolicyEngine: Send {
    /// What would apply in this power state; no mutation.
    fn plan(&self, state: PowerState) -> io::Result<drowse_api::Plan>;
    /// Apply (or, with `dry_run`, only report) the plan, returning a txn.
    fn apply(&mut self, state: PowerState, dry_run: bool) -> io::Result<drowse_api::ApplyResult>;
    /// Undo a transaction by id.
    fn revert(&mut self, txn_id: &str) -> io::Result<drowse_api::RevertResult>;
}

/// Probe outcome for the policy engine (needs privilege to apply; may be
/// plan-only unprivileged).
pub enum PolicyProbe {
    Ready(Box<dyn PolicyEngine>),
    Unavailable(drowse_api::SourceStatus),
}

// ---------------------------------------------------------------------------
// Engine: owns buffers, ticks collectors, serves deltas.
// ---------------------------------------------------------------------------

pub struct Engine {
    collectors: Vec<Box<dyn Collector>>,
    /// Slot offset of each collector into the flat sample buffers.
    offsets: Vec<usize>,
    prev: Box<[u64]>,
    curr: Box<[u64]>,
    prev_ns: u64,
    curr_ns: u64,
    ticks: u64,
    /// Sources that failed probe, reported verbatim in scorecards.
    pub unavailable: BTreeMap<String, drowse_api::SourceStatus>,
}

impl Engine {
    pub fn new(probes: Vec<Probe>) -> Engine {
        let mut collectors = Vec::new();
        let mut unavailable = BTreeMap::new();
        for p in probes {
            match p {
                Probe::Ready(c) => collectors.push(c),
                Probe::Unavailable { name, status } => {
                    unavailable.insert(name.to_string(), status);
                }
            }
        }
        let mut offsets = Vec::with_capacity(collectors.len());
        let mut total = 0usize;
        for c in &collectors {
            offsets.push(total);
            total += c.fields().len();
        }
        Engine {
            collectors,
            offsets,
            prev: vec![0; total].into_boxed_slice(),
            curr: vec![0; total].into_boxed_slice(),
            prev_ns: 0,
            curr_ns: 0,
            ticks: 0,
            unavailable,
        }
    }

    /// Sample every due collector once. `now_ns` is CLOCK_BOOTTIME.
    /// Allocation-free after construction. A collector whose `cadence` is N
    /// is read on every Nth tick (and always on the first); in between, its
    /// previous values carry forward.
    pub fn tick(&mut self, now_ns: u64) -> io::Result<()> {
        std::mem::swap(&mut self.prev, &mut self.curr);
        self.prev_ns = self.curr_ns;
        self.curr_ns = now_ns;
        for (i, c) in self.collectors.iter_mut().enumerate() {
            let start = self.offsets[i];
            let end = start + c.fields().len();
            let cadence = u64::from(c.cadence().max(1));
            if self.ticks.is_multiple_of(cadence) {
                c.sample(&mut self.curr[start..end])?;
            } else {
                self.curr[start..end].copy_from_slice(&self.prev[start..end]);
            }
        }
        self.ticks += 1;
        Ok(())
    }

    /// Deltas are meaningful once two ticks exist.
    pub fn ready(&self) -> bool {
        self.ticks >= 2
    }

    pub fn ticks(&self) -> u64 {
        self.ticks
    }

    /// Wall time between the last two ticks, in nanoseconds.
    pub fn elapsed_ns(&self) -> u64 {
        self.curr_ns.saturating_sub(self.prev_ns)
    }

    /// Iterate (collector name, field, value) for the latest tick, where a
    /// Counter yields its delta (None if it went backwards) and a Gauge its
    /// current value.
    pub fn values(&self) -> impl Iterator<Item = Sample<'_>> {
        self.collectors.iter().enumerate().flat_map(move |(ci, c)| {
            let base = self.offsets[ci];
            c.fields().iter().enumerate().map(move |(fi, f)| {
                let idx = base + fi;
                let value = match f.kind {
                    Kind::Gauge => Some(self.curr[idx]),
                    Kind::Counter => self.curr[idx].checked_sub(self.prev[idx]),
                };
                Sample { collector: c.name(), field: f, value }
            })
        })
    }
}

pub struct Sample<'a> {
    pub collector: &'static str,
    pub field: &'a Field,
    /// Counter delta or gauge value; None = invalid this tick.
    pub value: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        fields: Vec<Field>,
        n: u64,
    }
    impl Collector for Fake {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn fields(&self) -> &[Field] {
            &self.fields
        }
        fn sample(&mut self, out: &mut [u64]) -> io::Result<()> {
            self.n += 10;
            out[0] = self.n; // counter
            out[1] = 42; // gauge
            Ok(())
        }
    }

    struct Slow {
        fields: Vec<Field>,
        samples: u64,
    }
    impl Collector for Slow {
        fn name(&self) -> &'static str {
            "slow"
        }
        fn fields(&self) -> &[Field] {
            &self.fields
        }
        fn cadence(&self) -> u32 {
            3
        }
        fn sample(&mut self, out: &mut [u64]) -> io::Result<()> {
            self.samples += 1;
            out[0] = self.samples * 100; // gauge changes each real sample
            Ok(())
        }
    }

    #[test]
    fn cadence_skips_and_carries_forward() {
        let fields = vec![Field { name: "g".into(), kind: Kind::Gauge, unit: Unit::Count }];
        let mut e = Engine::new(vec![Probe::Ready(Box::new(Slow { fields, samples: 0 }))]);
        for t in 0..7u64 {
            e.tick(t * 1_000).unwrap();
        }
        // Sampled on ticks 0, 3, 6 -> 3 real reads; value carried in between.
        let v = e.values().next().unwrap().value;
        assert_eq!(v, Some(300));
    }

    #[test]
    fn deltas_and_gauges() {
        let fields = vec![
            Field { name: "c".into(), kind: Kind::Counter, unit: Unit::Count },
            Field { name: "g".into(), kind: Kind::Gauge, unit: Unit::Count },
        ];
        let mut e = Engine::new(vec![Probe::Ready(Box::new(Fake { fields, n: 0 }))]);
        e.tick(1_000).unwrap();
        assert!(!e.ready());
        e.tick(3_000).unwrap();
        assert!(e.ready());
        assert_eq!(e.elapsed_ns(), 2_000);
        let vals: Vec<_> = e.values().collect();
        assert_eq!(vals[0].value, Some(10)); // counter delta
        assert_eq!(vals[1].value, Some(42)); // gauge
    }
}
