//! Unit tests: handlers driven through the real router against a fake
//! engine, plus the timer arm/disarm policy. No sockets, no clocks.

use crate::score::SysInfo;
use crate::state::State;
use crate::{rpc, timer_plan};
use drowse_api::{RpcRequest, RpcResponse, Scorecard};
use drowse_core::service::drive;
use drowse_core::{Collector, Engine, Field, Kind, Probe, Unit};
use serde_json::{json, Value};
use std::io;
use std::sync::{Arc, Mutex};

/// Scripted collector: returns pre-baked absolute samples tick by tick.
struct Fake {
    name: &'static str,
    fields: Vec<Field>,
    script: Vec<Vec<u64>>,
    i: usize,
}

impl Collector for Fake {
    fn name(&self) -> &'static str {
        self.name
    }
    fn fields(&self) -> &[Field] {
        &self.fields
    }
    fn sample(&mut self, out: &mut [u64]) -> io::Result<()> {
        let row = &self.script[self.i.min(self.script.len() - 1)];
        out.copy_from_slice(row);
        self.i += 1;
        Ok(())
    }
}

fn f(name: &str, kind: Kind, unit: Unit) -> Field {
    Field { name: name.into(), kind, unit }
}

/// cpuidle (2 CPUs x POLL/C3), battery gauges, one RAPL package counter.
/// Tick deltas span 2 s: C3 +1.9e6 us -> 95% residency on both CPUs.
fn fake_engine() -> Engine {
    let cpuidle = Fake {
        name: "cpuidle",
        fields: vec![
            f("cpu0.POLL.time_us", Kind::Counter, Unit::Micros),
            f("cpu0.C3.time_us", Kind::Counter, Unit::Micros),
            f("cpu1.POLL.time_us", Kind::Counter, Unit::Micros),
            f("cpu1.C3.time_us", Kind::Counter, Unit::Micros),
        ],
        script: vec![
            vec![0, 100_000, 0, 200_000],
            vec![10_000, 2_000_000, 20_000, 2_100_000],
        ],
        i: 0,
    };
    let battery = Fake {
        name: "battery",
        fields: vec![
            f("power_now_uw", Kind::Gauge, Unit::MicroWatts),
            f("discharging", Kind::Gauge, Unit::Bool),
            f("capacity_pct", Kind::Gauge, Unit::Percent),
            f("energy_now_uj", Kind::Gauge, Unit::MicroJoules),
        ],
        // 5 W draw, discharging, 80%, 10 Wh left -> 120 min runtime.
        script: vec![vec![5_000_000, 1, 80, 36_000_000_000]],
        i: 0,
    };
    let rapl = Fake {
        name: "rapl",
        fields: vec![f("package-0.energy_uj", Kind::Counter, Unit::MicroJoules)],
        // +4e6 uJ over 2 s -> 2.0 W.
        script: vec![vec![0], vec![4_000_000]],
        i: 0,
    };
    Engine::new(vec![
        Probe::Ready(Box::new(cpuidle)),
        Probe::Ready(Box::new(battery)),
        Probe::Ready(Box::new(rapl)),
    ])
}

fn test_state() -> Arc<Mutex<State>> {
    let sysinfo = SysInfo { hostname: "testhost".into(), kernel: "6.0-test".into() };
    Arc::new(Mutex::new(State::new(fake_engine(), sysinfo)))
}

fn req(method: &str, params: Value) -> RpcRequest {
    RpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!(1)),
        method: method.into(),
        params: Some(params),
    }
}

fn result(resp: RpcResponse) -> Value {
    assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
    resp.result.expect("missing result")
}

#[test]
fn session_start_is_idempotent() {
    let st = test_state();
    let mut svc = rpc::router(st.clone());

    let a = result(drive(&mut svc, req("session.start", json!({"session_id": "x", "interval_ms": 1000}))));
    assert_eq!(a["session_id"], "x");
    assert_eq!(a["created"], json!(true));

    // Same id again: same session back, not an error, not a new session.
    let b = result(drive(&mut svc, req("session.start", json!({"session_id": "x", "interval_ms": 3000}))));
    assert_eq!(b["created"], json!(false));
    assert_eq!(b["interval_ms"], json!(1000), "existing session returned unchanged");
    assert_eq!(st.lock().unwrap().sessions.len(), 1);

    let list = result(drive(&mut svc, req("session.list", json!({}))));
    assert_eq!(list["sessions"].as_array().unwrap().len(), 1);
}

#[test]
fn session_stop_is_idempotent() {
    let st = test_state();
    let mut svc = rpc::router(st.clone());
    result(drive(&mut svc, req("session.start", json!({"session_id": "x"}))));

    let a = result(drive(&mut svc, req("session.stop", json!({"session_id": "x"}))));
    assert_eq!(a["already_stopped"], json!(false));
    let b = result(drive(&mut svc, req("session.stop", json!({"session_id": "x"}))));
    assert_eq!(b["already_stopped"], json!(true));
    // Never-existed id also succeeds.
    let c = result(drive(&mut svc, req("session.stop", json!({"session_id": "ghost"}))));
    assert_eq!(c["already_stopped"], json!(true));
}

#[test]
fn interval_below_minimum_is_usage_error() {
    let st = test_state();
    let mut svc = rpc::router(st);
    let resp = drive(&mut svc, req("session.start", json!({"interval_ms": 100})));
    let err = resp.error.expect("expected error");
    assert_eq!(err.code, -32602);
    assert_eq!(err.data.unwrap().code, drowse_api::ErrorCode::Usage);
}

#[test]
fn score_get_shape_and_round_trip() {
    let st = test_state();
    let mut svc = rpc::router(st.clone());
    result(drive(&mut svc, req("session.start", json!({"session_id": "s", "interval_ms": 1000}))));

    // Drive the sampler by hand: prime tick + one delta tick 2 s later.
    {
        let mut s = st.lock().unwrap();
        s.tick_now(1_000_000_000, 111).unwrap();
        s.tick_now(3_000_000_000, 222).unwrap();
    }

    let v = result(drive(&mut svc, req("score.get", json!({"window_s": 10.0}))));

    // Wire shape survives a round-trip through the api types.
    let sc: Scorecard = serde_json::from_value(v.clone()).expect("scorecard round-trip");
    assert_eq!(sc.schema, drowse_api::SCHEMA);
    assert_eq!(sc.kind, "scorecard");
    assert_eq!(sc.meta.ticks, 1);
    assert!((sc.meta.window_s - 2.0).abs() < 1e-9);
    assert_eq!(sc.meta.hostname, "testhost");
    assert!(!sc.meta.degraded);

    // SE: C3 is deepest; 1.9e6 us over 2 s on both CPUs = 95% -> grade A.
    let se = sc.score.sleep_efficiency.unwrap();
    assert!((se - 95.0).abs() < 1e-6, "se = {se}");
    assert_eq!(sc.score.grade, Some(drowse_api::Grade::A));
    assert_eq!(sc.score.deepest_state, "C3");
    assert_eq!(sc.score.basis, "os-timed-mean");

    // Battery: 5 W discharging, 10 Wh -> 120 min estimate.
    let b = sc.battery.as_ref().unwrap();
    assert!((b.power_w - 5.0).abs() < 1e-9);
    assert!(b.discharging);
    assert_eq!(sc.score.idle_draw_w, Some(5.0));
    assert_eq!(sc.score.runtime_est_min, Some(120));

    // RAPL: 4e6 uJ over 2 s = 2 W.
    assert!((sc.package_power_w.unwrap() - 2.0).abs() < 1e-9);

    // Per-CPU residency map.
    assert_eq!(sc.cpu.len(), 2);
    assert!((sc.cpu[0].states["C3"] - 95.0).abs() < 1e-6);
    assert!((sc.cpu[1].states["C3"] - 95.0).abs() < 1e-6);

    // All three sources reported ok.
    assert_eq!(v["sources"]["cpuidle"]["state"], "ok");
    assert_eq!(v["sources"]["battery"]["state"], "ok");
    assert_eq!(v["sources"]["rapl"]["state"], "ok");
}

#[test]
fn score_get_warmup_is_busy_and_retryable() {
    let st = test_state();
    let mut svc = rpc::router(st);
    result(drive(&mut svc, req("session.start", json!({"session_id": "s"}))));
    // No ticks yet -> busy, retryable, never a fake zero scorecard.
    let resp = drive(&mut svc, req("score.get", json!({})));
    let data = resp.error.expect("busy").data.unwrap();
    assert_eq!(data.code, drowse_api::ErrorCode::Busy);
    assert!(data.retryable);
}

// ---------------------------------------------------------------------------
// Timer policy: the battery promise. Sessions -> 0 MUST disarm the timer.
// ---------------------------------------------------------------------------

#[test]
fn timer_disarms_when_last_session_stops() {
    let st = test_state();
    let mut svc = rpc::router(st.clone());

    assert_eq!(st.lock().unwrap().min_interval_ms(), None);
    assert_eq!(timer_plan(None, 123), None, "idle daemon arms no timer");

    result(drive(&mut svc, req("session.start", json!({"session_id": "a", "interval_ms": 2000}))));
    result(drive(&mut svc, req("session.start", json!({"session_id": "b", "interval_ms": 500}))));
    // N sessions share one sampler at the minimum interval.
    assert_eq!(st.lock().unwrap().min_interval_ms(), Some(500));

    result(drive(&mut svc, req("session.stop", json!({"session_id": "b"}))));
    assert_eq!(st.lock().unwrap().min_interval_ms(), Some(2000));

    result(drive(&mut svc, req("session.stop", json!({"session_id": "a"}))));
    // The transition under test: zero sessions -> timer_plan disarms.
    assert_eq!(st.lock().unwrap().min_interval_ms(), None);
    assert_eq!(timer_plan(st.lock().unwrap().min_interval_ms(), 987_654_321), None);
}

#[test]
fn timer_plan_aligns_absolute_expiry() {
    // 2 s cadence, now = 3.5 s since boot -> first expiry at exactly 4 s.
    let (next, iv) = timer_plan(Some(2000), 3_500_000_000).unwrap();
    assert_eq!(iv, 2_000_000_000);
    assert_eq!(next, 4_000_000_000);
    assert_eq!(next % iv, 0, "expiries land on interval multiples since boot");
    // Already on a boundary: next boundary, not now (no immediate fire).
    let (next, _) = timer_plan(Some(2000), 4_000_000_000).unwrap();
    assert_eq!(next, 6_000_000_000);
}

#[test]
fn subscribe_requires_connection_and_registers() {
    let st = test_state();
    let mut svc = rpc::router(st.clone());

    // No transport context -> internal error, not a phantom subscription.
    let resp = drive(&mut svc, req("subscribe", json!({"kinds": ["tick"]})));
    assert!(resp.error.is_some());

    st.lock().unwrap().current_conn = Some(7);
    let ok = result(drive(&mut svc, req("subscribe", json!({"kinds": ["tick"]}))));
    assert_eq!(ok["subscribed"], json!(true));
    assert!(st.lock().unwrap().subscribers.contains_key(&7));

    let bad = drive(&mut svc, req("subscribe", json!({"kinds": ["offenders"]})));
    assert!(bad.error.is_some(), "unknown kind rejected");
}
