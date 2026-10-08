//! Unit tests: handlers driven through the real router against a fake
//! engine, plus the timer arm/disarm policy. No sockets, no clocks.

use crate::score::{FieldInfo, SysInfo};
use crate::state::{Policy, State};
use crate::{rpc, timer_plan};
use drowse_api::{
    ApplyResult, ErrorCode, OffenderKind, Plan, PolicyAction, RevertResult, Risk, RpcRequest,
    RpcResponse, Scorecard, SourceState, SourceStatus, Tier, SCHEMA,
};
use drowse_core::service::{drive, Router};
use drowse_core::{
    AttributionDelta, Collector, Engine, Field, Kind, PolicyEngine, PowerState, Probe, RawOffender,
    Unit,
};
use serde_json::{json, Value};
use std::io;
use std::sync::{Arc, Mutex};

/// Build the verb router with a stub (unavailable) policy engine. The policy
/// verbs are exercised separately in the policy_* tests with a fake engine.
fn router(st: Arc<Mutex<State>>) -> Router {
    rpc::router(st, rpc::unavailable_policy("test: no policy engine"))
}

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
    // Battery lives in the fast fake engine here; the slow lane is exercised
    // separately in the slow_lane_* tests. Empty slow layout => no merge.
    let sysinfo = SysInfo { hostname: "testhost".into(), kernel: "6.0-test".into() };
    Arc::new(Mutex::new(State::new(
        fake_engine(),
        Vec::new(),
        10_000,
        drowse_api::SourceStatus::ok(),
        sysinfo,
    )))
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
    let mut svc = router(st.clone());

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
    let mut svc = router(st.clone());
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
    let mut svc = router(st);
    let resp = drive(&mut svc, req("session.start", json!({"interval_ms": 100})));
    let err = resp.error.expect("expected error");
    assert_eq!(err.code, -32602);
    assert_eq!(err.data.unwrap().code, drowse_api::ErrorCode::Usage);
}

#[test]
fn score_get_shape_and_round_trip() {
    let st = test_state();
    let mut svc = router(st.clone());
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
    let mut svc = router(st);
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
    let mut svc = router(st.clone());

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
    let mut svc = router(st.clone());

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

// ---------------------------------------------------------------------------
// Slow lane: battery merges from the off-thread publish, and a frozen lane
// degrades rather than reporting stale values as current.
// ---------------------------------------------------------------------------

fn cpuidle_only_engine() -> Engine {
    let cpuidle = Fake {
        name: "cpuidle",
        fields: vec![
            f("cpu0.POLL.time_us", Kind::Counter, Unit::Micros),
            f("cpu0.C3.time_us", Kind::Counter, Unit::Micros),
        ],
        script: vec![vec![0, 100_000], vec![10_000, 2_000_000]],
        i: 0,
    };
    Engine::new(vec![Probe::Ready(Box::new(cpuidle))])
}

fn battery_slow_layout() -> Vec<FieldInfo> {
    vec![
        FieldInfo {
            collector: "battery",
            name: "BAT.power_now_uw".into(),
            kind: Kind::Gauge,
            unit: Unit::MicroWatts,
        },
        FieldInfo {
            collector: "battery",
            name: "BAT.discharging".into(),
            kind: Kind::Gauge,
            unit: Unit::Bool,
        },
    ]
}

fn slow_state(cadence_ms: u64) -> State {
    let sysinfo = SysInfo { hostname: "testhost".into(), kernel: "6.0-test".into() };
    State::new(
        cpuidle_only_engine(),
        battery_slow_layout(),
        cadence_ms,
        drowse_api::SourceStatus::ok(),
        sysinfo,
    )
}

#[test]
fn slow_lane_values_merge_into_scorecard() {
    let mut st = slow_state(1000);
    st.tick_now(1_000_000_000, 100).unwrap();
    st.tick_now(3_000_000_000, 200).unwrap(); // last record unix_ms = 200
    // Published just now (age 0) -> fresh.
    st.publish_slow(vec![Some(5_000_000), Some(1)], 200);

    let recs = st.window_records(10.0);
    let sc = st.make_scorecard(&recs);
    let b = sc.battery.expect("battery present when slow lane is fresh");
    assert!((b.power_w - 5.0).abs() < 1e-9);
    assert!(b.discharging);
    assert_eq!(sc.score.idle_draw_w, Some(5.0));
    assert_eq!(sc.sources["battery"].state, drowse_api::SourceState::Ok);
    // cpuidle came from the fast lane in the same card.
    assert!(sc.score.sleep_efficiency.is_some());
}

#[test]
fn slow_lane_staleness_degrades_battery() {
    let mut st = slow_state(1000); // staleness threshold = 3000 ms
    st.tick_now(1_000_000_000, 100).unwrap();
    st.tick_now(3_000_000_000, 100_000).unwrap(); // now_ms = 100000
    st.publish_slow(vec![Some(5_000_000), Some(1)], 100); // age ~100s >> 3s

    let recs = st.window_records(10.0);
    let sc = st.make_scorecard(&recs);
    // A frozen slow lane must not present stale values as current...
    assert!(sc.battery.is_none(), "stale slow lane withholds battery value");
    // ...but battery stays visible as Degraded so the freeze is detectable.
    assert_eq!(sc.sources["battery"].state, drowse_api::SourceState::Degraded);
}

#[test]
fn slow_fresh_window_boundaries() {
    let mut st = slow_state(1000); // threshold 3x = 3000 ms
    assert!(!st.slow_fresh(0), "never-published slow lane is not fresh");
    st.publish_slow(vec![Some(1), Some(0)], 1000);
    assert!(st.slow_fresh(1000)); // age 0
    assert!(st.slow_fresh(4000)); // age 3000 == threshold
    assert!(!st.slow_fresh(4001)); // age 3001 > threshold
}

// ---------------------------------------------------------------------------
// Attribution: offenders.get ranks by battery cost (pkg_breaks), not raw
// wakeups, and degrades to an empty-but-valid table when eBPF is unavailable.
// ---------------------------------------------------------------------------

fn attr_state(source: SourceStatus) -> State {
    let sysinfo = SysInfo { hostname: "testhost".into(), kernel: "6.0-test".into() };
    State::new(cpuidle_only_engine(), battery_slow_layout(), 10_000, source, sysinfo)
}

fn raw(
    kind: OffenderKind,
    key: &str,
    name: &str,
    pid: Option<u32>,
    wakeups: u64,
    idle_breaks: u64,
    pkg_breaks: u64,
) -> RawOffender {
    RawOffender {
        kind,
        key: key.into(),
        name: name.into(),
        pid,
        wakeups,
        idle_breaks,
        pkg_breaks,
    }
}

#[test]
fn offenders_ranked_by_pkg_breaks_not_raw_wakeups() {
    let mut st = attr_state(SourceStatus::ok());
    // hrtimer dominates raw wakeups but breaks no package sleep; syncthing
    // wakes rarely but shatters all-idle windows — it must rank first.
    let timer = raw(OffenderKind::Timer, "hrtimer_wakeup", "hrtimer_wakeup", None, 1000, 800, 0);
    let sync = raw(OffenderKind::Process, "1663", "syncthing", Some(1663), 50, 45, 40);
    st.publish_attribution(
        AttributionDelta { offenders: vec![timer, sync], observer_wakeups: 5, all_cores_idle_ns: 0 },
        10_000_000_000, // 10 s window
        123_456,
    );

    let off = st.make_offenders(20);
    assert_eq!(off.kind, "offenders");
    assert!((off.meta.window_s - 10.0).abs() < 1e-9);
    assert!(!off.meta.degraded);
    assert_eq!(off.source.state, SourceState::Ok);

    // Ranking: syncthing first (pkg_breaks 4/s beats hrtimer's 0), despite
    // hrtimer having 20x the raw wakeups.
    assert_eq!(off.offenders[0].id, "proc:1663:syncthing");
    assert_eq!(off.offenders[0].kind, OffenderKind::Process);
    assert_eq!(off.offenders[0].pid, Some(1663));
    assert!((off.offenders[0].pkg_breaks_per_s - 4.0).abs() < 1e-9);
    assert!((off.offenders[0].wakeups_per_s - 5.0).abs() < 1e-9);
    assert!((off.offenders[0].idle_breaks_per_s - 4.5).abs() < 1e-9);
    assert!((off.offenders[0].share - 50.0 / 1050.0).abs() < 1e-9);
    assert!(off.offenders[0].summary.contains("syncthing"));
    assert!(off.offenders[0].summary.contains("pid 1663"));

    assert_eq!(off.offenders[1].id, "timer:hrtimer_wakeup");
    assert_eq!(off.offenders[1].kind, OffenderKind::Timer);
    assert_eq!(off.offenders[1].pid, None);
    assert!((off.offenders[1].pkg_breaks_per_s - 0.0).abs() < 1e-9);

    // observer row reported honestly: 5 wakeups / 10 s.
    assert!((off.observer_wakeups_per_s.unwrap() - 0.5).abs() < 1e-9);
}

#[test]
fn offenders_limit_truncates_after_ranking() {
    let mut st = attr_state(SourceStatus::ok());
    let a = raw(OffenderKind::Process, "1", "a", Some(1), 10, 0, 1);
    let b = raw(OffenderKind::Process, "2", "b", Some(2), 10, 0, 9); // highest pkg
    let c = raw(OffenderKind::Process, "3", "c", Some(3), 10, 0, 5);
    st.publish_attribution(
        AttributionDelta { offenders: vec![a, b, c], observer_wakeups: 0, all_cores_idle_ns: 0 },
        1_000_000_000,
        1,
    );
    let off = st.make_offenders(1);
    assert_eq!(off.offenders.len(), 1);
    assert_eq!(off.offenders[0].id, "proc:2:b"); // the top pkg-break survives
}

#[test]
fn offenders_unavailable_is_empty_with_reason_not_error() {
    // Unprivileged: eBPF load was denied; offenders.get must still answer.
    let st = Arc::new(Mutex::new(attr_state(SourceStatus::err(
        SourceState::PermissionDenied,
        "attribution needs root (CAP_BPF)",
    ))));
    let mut svc = router(st);
    let resp = drive(&mut svc, req("offenders.get", json!({"limit": 5})));
    let v = result(resp); // result() asserts no RPC error
    assert_eq!(v["kind"], "offenders");
    assert_eq!(v["offenders"].as_array().unwrap().len(), 0);
    assert_eq!(v["source"]["state"], "permission_denied");
    assert_eq!(v["meta"]["degraded"], json!(true));
}

#[test]
fn offenders_get_is_registered_and_defaults_empty() {
    // test_state has attribution ok but no window drained yet.
    let st = test_state();
    let mut svc = router(st);
    let v = result(drive(&mut svc, req("offenders.get", json!({}))));
    assert_eq!(v["kind"], "offenders");
    assert_eq!(v["offenders"].as_array().unwrap().len(), 0);
    assert_eq!(v["source"]["state"], "ok");
    assert!((v["meta"]["window_s"].as_f64().unwrap() - 0.0).abs() < 1e-9);
}

// ---------------------------------------------------------------------------
// Policy verbs: dry-run is the default (safety), plan reflects power state,
// and revert needs a txn id. A fake engine stands in for drowse-policy.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakePolicy;

fn fake_action(id: &str) -> PolicyAction {
    PolicyAction {
        id: id.into(),
        kind: "sysfs".into(),
        summary: "set a knob".into(),
        tier: Tier::T0,
        risk: Risk::Low,
        reversible: true,
        from: Some("on".into()),
        to: Some("auto".into()),
        est_gain_min: None,
        note: None,
    }
}

impl PolicyEngine for FakePolicy {
    fn plan(&self, state: PowerState) -> io::Result<Plan> {
        Ok(Plan {
            schema: SCHEMA,
            kind: "plan".into(),
            on_battery: state == PowerState::Battery,
            actions: vec![fake_action("knob:epp")],
            excluded: vec![],
        })
    }
    fn apply(&mut self, _state: PowerState, dry_run: bool) -> io::Result<ApplyResult> {
        Ok(ApplyResult {
            schema: SCHEMA,
            kind: "apply".into(),
            txn_id: if dry_run { "dry-run".into() } else { "txn-1".into() },
            dry_run,
            applied: vec![fake_action("knob:epp")],
            failed: vec![],
        })
    }
    fn revert(&mut self, txn_id: &str) -> io::Result<RevertResult> {
        Ok(RevertResult {
            schema: SCHEMA,
            kind: "revert".into(),
            txn_id: txn_id.into(),
            already_reverted: txn_id == "gone",
            reverted: vec![fake_action("knob:epp")],
        })
    }
}

fn policy_router() -> Router {
    let pol = Arc::new(Mutex::new(Policy::Ready(Box::new(FakePolicy))));
    rpc::router(test_state(), pol)
}

#[test]
fn policy_apply_defaults_to_dry_run() {
    let mut r = policy_router();
    // No dry_run field at all -> MUST be treated as dry-run (never mutate).
    let v = result(drive(&mut r, req("policy.apply", json!({ "on_battery": true }))));
    assert_eq!(v["kind"], "apply");
    assert_eq!(v["dry_run"], json!(true), "missing dry_run must mean dry-run");
    assert_eq!(v["txn_id"], "dry-run");
}

#[test]
fn policy_apply_false_is_the_only_mutating_path() {
    let mut r = policy_router();
    let v = result(drive(&mut r, req("policy.apply", json!({ "dry_run": false, "on_battery": true }))));
    assert_eq!(v["dry_run"], json!(false));
    assert_eq!(v["txn_id"], "txn-1");
}

#[test]
fn policy_plan_reflects_explicit_power_state() {
    let mut r = policy_router();
    let batt = result(drive(&mut r, req("policy.plan", json!({ "on_battery": true }))));
    assert_eq!(batt["kind"], "plan");
    assert_eq!(batt["on_battery"], json!(true));
    let ac = result(drive(&mut r, req("policy.plan", json!({ "on_battery": false }))));
    assert_eq!(ac["on_battery"], json!(false));
}

#[test]
fn policy_revert_requires_txn_id_then_undoes() {
    let mut r = policy_router();
    let err = drive(&mut r, req("policy.revert", json!({}))).error.expect("usage error");
    assert_eq!(err.data.unwrap().code, ErrorCode::Usage);
    let ok = result(drive(&mut r, req("policy.revert", json!({ "txn_id": "txn-1" }))));
    assert_eq!(ok["txn_id"], "txn-1");
    assert_eq!(ok["already_reverted"], json!(false));
}

#[test]
fn policy_verbs_map_unavailable_engine_to_typed_error() {
    // The stub router helper carries an Unavailable(NotPresent) policy.
    let mut r = router(test_state());
    let err = drive(&mut r, req("policy.plan", json!({ "on_battery": true })))
        .error
        .expect("unavailable -> error");
    assert_eq!(err.data.unwrap().code, ErrorCode::NotPresent);
}
