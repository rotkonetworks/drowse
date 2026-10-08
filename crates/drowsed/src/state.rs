//! Daemon state: engine + sessions + tick history + subscribers. All state
//! transitions live here as plain methods so the RPC handlers stay thin and
//! unit-testable without a socket.

use crate::score::{self, FieldInfo, SysInfo, TickRecord};
use drowse_api::{Offenders, Scorecard, SourceStatus};
use drowse_core::{AttributionDelta, Engine, PolicyEngine, RawOffender, Unit};
use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

/// Keep at most this many completed ticks (~20 min at 2 s cadence).
const HISTORY_CAP: usize = 720;

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub interval_ms: u64,
    pub started_unix_ms: u64,
}

pub struct State {
    /// Fast lane: cheap collectors (cpuidle, amdgpu, rapl) sampled on the
    /// ppoll thread every tick. High-latency collectors (battery) live in the
    /// slow lane and never touch this engine.
    pub engine: Engine,
    /// fast layout ++ slow layout; the axis `make_scorecard` assembles over.
    pub combined_layout: Vec<FieldInfo>,
    pub history: VecDeque<TickRecord>,
    pub sessions: BTreeMap<String, SessionInfo>,
    /// Connection ids subscribed to per-tick notifications.
    pub subscribers: BTreeMap<u64, ()>,
    /// Connection currently being served; set by the transport around each
    /// dispatch so `subscribe` knows who called.
    pub current_conn: Option<u64>,
    next_id: u64,
    pub sysinfo: SysInfo,

    // -- slow lane: latest values published by the slow-lane thread ----------
    /// Field layout of the slow collectors (fixed after probe).
    pub slow_layout: Vec<FieldInfo>,
    /// Latest slow values, index-aligned with `slow_layout`. `None` per field
    /// until the slow lane has produced a valid sample for it.
    pub slow_values: Vec<Option<u64>>,
    /// Wall-clock ms of the last slow publish; 0 = never. Drives staleness.
    pub slow_unix_ms: u64,
    /// The slow lane's cadence; staleness threshold is 3x this.
    pub slow_cadence_ms: u64,

    // -- attribution lane: last window drained by the eBPF attributor ---------
    /// Availability of the attribution subsystem (ok when eBPF loaded, else the
    /// probe's typed reason — e.g. PermissionDenied unprivileged).
    pub attr_source: SourceStatus,
    /// Last published attribution window; None until the first drain lands.
    pub attr: Option<AttrWindow>,
}

/// The policy engine, held behind its OWN mutex — separate from the sampler
/// `State` — so a slow `apply()` (verify-after-apply reads knobs back, watches
/// AER) never holds the sampler lock and never blocks sampling. `probe()`
/// returns Ready even unprivileged (apply reports per-action PermissionDenied),
/// so `Unavailable` is only a safety net.
pub enum Policy {
    Ready(Box<dyn PolicyEngine>),
    Unavailable(SourceStatus),
}

/// One drained attribution window: per-offender counts and the wall time they
/// accumulated over. `offenders.get` divides counts by the window for rates,
/// so it never blocks on the attributor — it reads this last snapshot.
pub struct AttrWindow {
    pub offenders: Vec<RawOffender>,
    pub observer_wakeups: u64,
    pub window_ns: u64,
    pub unix_ms: u64,
}

impl State {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        engine: Engine,
        slow_layout: Vec<FieldInfo>,
        slow_cadence_ms: u64,
        attr_source: SourceStatus,
        sysinfo: SysInfo,
    ) -> State {
        let mut combined_layout: Vec<FieldInfo> = score::layout_of(&engine);
        combined_layout.extend(slow_layout.iter().cloned());
        let slow_values = vec![None; slow_layout.len()];
        State {
            engine,
            combined_layout,
            history: VecDeque::new(),
            sessions: BTreeMap::new(),
            subscribers: BTreeMap::new(),
            current_conn: None,
            next_id: 0,
            sysinfo,
            slow_layout,
            slow_values,
            slow_unix_ms: 0,
            slow_cadence_ms,
            attr_source,
            attr: None,
        }
    }

    /// Publish a drained attribution window (called by the attribution thread,
    /// off the sampler critical path).
    pub fn publish_attribution(&mut self, delta: AttributionDelta, window_ns: u64, unix_ms: u64) {
        self.attr = Some(AttrWindow {
            offenders: delta.offenders,
            observer_wakeups: delta.observer_wakeups,
            window_ns,
            unix_ms,
        });
    }

    /// Build the ranked offender table from the last published window. Always
    /// a valid `Offenders`: when attribution is unavailable or no window has
    /// been drained yet, the list is empty and `source` explains why.
    pub fn make_offenders(&self, limit: usize) -> Offenders {
        match &self.attr {
            Some(w) => score::offenders(
                &w.offenders,
                w.observer_wakeups,
                w.window_ns,
                w.unix_ms,
                &self.attr_source,
                &self.sysinfo,
                limit,
            ),
            None => score::offenders(&[], 0, 0, unix_ms(), &self.attr_source, &self.sysinfo, limit),
        }
    }

    /// Publish a fresh batch of slow-lane values (called by the slow thread,
    /// off the sampler critical path). `len` must equal `slow_layout.len()`.
    pub fn publish_slow(&mut self, values: Vec<Option<u64>>, unix_ms: u64) {
        debug_assert_eq!(values.len(), self.slow_layout.len());
        self.slow_values = values;
        self.slow_unix_ms = unix_ms;
    }

    /// Slow data is fresh if it was published within 3x the slow cadence of
    /// `now_ms`. A frozen slow lane goes stale and its fields degrade rather
    /// than being reported as current — the anti-freeze guard.
    pub fn slow_fresh(&self, now_ms: u64) -> bool {
        self.slow_unix_ms != 0
            && now_ms.saturating_sub(self.slow_unix_ms)
                <= self.slow_cadence_ms.saturating_mul(3)
    }

    // -- sessions -----------------------------------------------------------

    /// Idempotent: a caller-supplied id that already exists returns the
    /// existing session unchanged, `created = false`.
    pub fn start_session(
        &mut self,
        id: Option<String>,
        interval_ms: u64,
    ) -> (SessionInfo, bool) {
        let id = id.unwrap_or_else(|| {
            self.next_id += 1;
            format!("s{}", self.next_id)
        });
        if let Some(existing) = self.sessions.get(&id) {
            return (existing.clone(), false);
        }
        let info = SessionInfo { id: id.clone(), interval_ms, started_unix_ms: unix_ms() };
        self.sessions.insert(id, info.clone());
        (info, true)
    }

    /// Idempotent: returns `already_stopped = true` for unknown/stopped ids.
    pub fn stop_session(&mut self, id: &str) -> bool {
        self.sessions.remove(id).is_none()
    }

    /// N sessions share one sampler: tick at the minimum requested interval.
    pub fn min_interval_ms(&self) -> Option<u64> {
        self.sessions.values().map(|s| s.interval_ms).min()
    }

    // -- sampling -----------------------------------------------------------

    /// Sample all collectors. Records a delta tick once two samples exist.
    /// Returns true if a record was appended.
    pub fn tick_now(&mut self, now_ns: u64, unix_ms: u64) -> io::Result<bool> {
        self.engine.tick(now_ns)?;
        if !self.engine.ready() {
            return Ok(false);
        }
        self.history.push_back(score::record(&self.engine, unix_ms));
        while self.history.len() > HISTORY_CAP {
            self.history.pop_front();
        }
        Ok(true)
    }

    /// Two inline samples 1 s apart, for `score.get` with no session running.
    /// Blocks the caller (single-threaded daemon: the whole loop) for ~1 s.
    pub fn one_shot(&mut self) -> io::Result<TickRecord> {
        self.engine.tick(boottime_ns())?;
        std::thread::sleep(std::time::Duration::from_secs(1));
        self.engine.tick(boottime_ns())?;
        Ok(score::record(&self.engine, unix_ms()))
    }

    /// Trailing records covering `window_s`, chronological, possibly empty.
    pub fn window_records(&self, window_s: f64) -> Vec<&TickRecord> {
        let window_ns = (window_s.max(0.0) * 1e9) as u64;
        let mut acc = 0u64;
        let mut out: Vec<&TickRecord> = Vec::new();
        for r in self.history.iter().rev() {
            if acc >= window_ns && !out.is_empty() {
                break;
            }
            acc += r.elapsed_ns;
            out.push(r);
        }
        out.reverse();
        out
    }

    /// Assemble a scorecard from fast-lane `records`, merging the slow lane's
    /// latest published values. The slow snapshot (battery gauges) is appended
    /// to the most recent record only — gauges are read from their last valid
    /// tick, so one carrier record suffices and the others stay short (slow
    /// columns then read as absent for them, which assemble handles). When the
    /// slow lane is stale, the snapshot is withheld and battery degrades.
    pub fn make_scorecard(&self, records: &[&TickRecord]) -> Scorecard {
        let now_ms = records.last().map(|r| r.unix_ms).unwrap_or_else(unix_ms);
        let fresh = self.slow_fresh(now_ms);
        let mut merged: Vec<TickRecord> = records
            .iter()
            .map(|r| TickRecord {
                unix_ms: r.unix_ms,
                elapsed_ns: r.elapsed_ns,
                values: r.values.clone(),
            })
            .collect();
        if fresh && let Some(last) = merged.last_mut() {
            last.values.extend_from_slice(&self.slow_values);
        }
        let refs: Vec<&TickRecord> = merged.iter().collect();
        score::assemble(&self.combined_layout, &refs, &self.engine.unavailable, &self.sysinfo)
    }

    /// Params for the per-tick "tick" notification, from the latest record.
    pub fn make_tick_params(&self) -> Option<serde_json::Value> {
        let last = self.history.back()?;
        let sc = self.make_scorecard(&[last]);
        Some(serde_json::json!({ "scorecard": sc }))
    }

    /// Source availability for `system.check`: ready collectors + probe
    /// failures, no sampling required.
    pub fn sources_now(&self) -> BTreeMap<String, SourceStatus> {
        let mut out = self.engine.unavailable.clone();
        for f in &self.combined_layout {
            out.entry(f.collector.to_string()).or_insert_with(SourceStatus::ok);
        }
        out
    }

    /// Last known battery discharge state from the slow lane's published gauge,
    /// used only as the fallback for power-state detection when AC `online` is
    /// unreadable. `None` when battery telemetry is absent or not yet sampled.
    pub fn battery_discharging(&self) -> Option<bool> {
        for (i, f) in self.slow_layout.iter().enumerate() {
            if f.collector == "battery" && f.unit == Unit::Bool && f.name.contains("discharg") {
                return self.slow_values.get(i).copied().flatten().map(|v| v != 0);
            }
        }
        None
    }
}

pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// CLOCK_BOOTTIME in nanoseconds: monotonic across suspend, so deltas over a
/// sleep are rated against real wall time.
pub fn boottime_ns() -> u64 {
    let ts = rustix::time::clock_gettime(rustix::time::ClockId::Boottime);
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}
