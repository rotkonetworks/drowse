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
}

/// Probe outcome: an unavailable source is a value, not an error.
pub enum Probe {
    Ready(Box<dyn Collector>),
    Unavailable { name: &'static str, status: drowse_api::SourceStatus },
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

    /// Sample every collector once. `now_ns` is CLOCK_BOOTTIME.
    /// Allocation-free after construction.
    pub fn tick(&mut self, now_ns: u64) -> io::Result<()> {
        std::mem::swap(&mut self.prev, &mut self.curr);
        self.prev_ns = self.curr_ns;
        self.curr_ns = now_ns;
        for (i, c) in self.collectors.iter_mut().enumerate() {
            let start = self.offsets[i];
            let end = start + c.fields().len();
            c.sample(&mut self.curr[start..end])?;
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
