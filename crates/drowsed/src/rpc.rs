//! The verb surface: plain functions lifted into the drowse-core service
//! stack. Handlers only transform state; arming/disarming the timer is the
//! transport loop's job (it re-reads `min_interval_ms` after every dispatch).

use crate::state::{Policy, PolicyHandle, State};
use drowse_api::{ErrorCode, ErrorData, PowerStatus, RpcRequest, RpcResponse, SourceState, SourceStatus, SCHEMA};
use drowse_core::service::{drive, handler, Layer, Router, RpcFuture, Service};
use drowse_core::PowerState;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::future::ready;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

pub const DEFAULT_OFFENDER_LIMIT: u64 = 20;
pub const MIN_INTERVAL_MS: u64 = 500;
pub const DEFAULT_INTERVAL_MS: u64 = 2000;
pub const DEFAULT_WINDOW_S: f64 = 10.0;

// ---------------------------------------------------------------------------
// Logging filter, in the paper's style: wraps any inner service.
// ---------------------------------------------------------------------------

pub struct LogLayer;

impl<S> Layer<S> for LogLayer {
    type Service = Log<S>;
    fn layer(&self, inner: S) -> Log<S> {
        Log(inner)
    }
}

pub struct Log<S>(pub S);

impl<S> Service<RpcRequest> for Log<S>
where
    S: Service<RpcRequest, Response = RpcResponse, Error = Infallible, Future = RpcFuture>,
{
    type Response = RpcResponse;
    type Error = Infallible;
    type Future = RpcFuture;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, req: RpcRequest) -> RpcFuture {
        let method = req.method.clone();
        let t = Instant::now();
        let resp = drive(&mut self.0, req);
        let outcome = if resp.error.is_some() { "err" } else { "ok" };
        eprintln!("drowsed: {method} {outcome} {}us", t.elapsed().as_micros());
        ready(Ok(resp))
    }
}

// ---------------------------------------------------------------------------
// Param helpers (manual extraction; params are tiny and serde derive would
// pull the serde dep into this crate for no gain).
// ---------------------------------------------------------------------------

fn p_u64(params: &Option<Value>, key: &str) -> Option<u64> {
    params.as_ref()?.get(key)?.as_u64()
}

fn p_f64(params: &Option<Value>, key: &str) -> Option<f64> {
    params.as_ref()?.get(key)?.as_f64()
}

fn p_str(params: &Option<Value>, key: &str) -> Option<String> {
    Some(params.as_ref()?.get(key)?.as_str()?.to_string())
}

fn p_bool(params: &Option<Value>, key: &str) -> Option<bool> {
    params.as_ref()?.get(key)?.as_bool()
}

fn usage(id: Option<Value>, detail: impl Into<String>) -> RpcResponse {
    let detail = detail.into();
    RpcResponse::err(
        id,
        -32602,
        detail.clone(),
        Some(ErrorData { code: ErrorCode::Usage, detail, retryable: false, hint: None }),
    )
}

fn busy(id: Option<Value>, detail: impl Into<String>, hint: &str) -> RpcResponse {
    let detail = detail.into();
    RpcResponse::err(
        id,
        -32000,
        detail.clone(),
        Some(ErrorData {
            code: ErrorCode::Busy,
            detail,
            retryable: true,
            hint: Some(hint.to_string()),
        }),
    )
}

fn internal(id: Option<Value>, detail: impl Into<String>) -> RpcResponse {
    let detail = detail.into();
    RpcResponse::err(
        id,
        -32603,
        detail.clone(),
        Some(ErrorData { code: ErrorCode::Internal, detail, retryable: false, hint: None }),
    )
}

/// The policy engine's own `Unavailable` status mapped to an RPC error. Only a
/// safety net: `drowse_policy::probe()` is always Ready.
fn policy_unavailable(id: Option<Value>, status: &SourceStatus) -> RpcResponse {
    let code = match status.state {
        SourceState::PermissionDenied => ErrorCode::Permission,
        SourceState::NotPresent => ErrorCode::NotPresent,
        _ => ErrorCode::Internal,
    };
    let detail = status.detail.clone().unwrap_or_else(|| "policy engine unavailable".into());
    RpcResponse::err(
        id,
        -32000,
        detail.clone(),
        Some(ErrorData { code, detail, retryable: false, hint: None }),
    )
}

// ---------------------------------------------------------------------------
// Power-state detection for policy. Reads AC `online` directly from sysfs (no
// sampler-lock involvement); falls back to the battery gauge via a brief State
// lock, released before any policy call. Default Battery when unknown — the
// safer context (a plan computed for battery is conservative).
// ---------------------------------------------------------------------------

fn read_ac_online() -> Option<bool> {
    let base = std::path::Path::new("/sys/class/power_supply");
    let mut fallback: Option<bool> = None;
    for entry in std::fs::read_dir(base).ok()?.flatten() {
        let dir = entry.path();
        let is_mains = std::fs::read_to_string(dir.join("type"))
            .map(|t| t.trim() == "Mains")
            .unwrap_or(false);
        if let Ok(s) = std::fs::read_to_string(dir.join("online")) {
            let online = s.trim() == "1";
            if is_mains {
                return Some(online); // a Mains supply is the authoritative AC state
            }
            fallback.get_or_insert(online);
        }
    }
    fallback
}

fn detect_power_state(explicit_on_battery: Option<bool>, st: &Arc<Mutex<State>>) -> PowerState {
    if let Some(on_batt) = explicit_on_battery {
        return if on_batt { PowerState::Battery } else { PowerState::Ac };
    }
    if let Some(ac_online) = read_ac_online() {
        return if ac_online { PowerState::Ac } else { PowerState::Battery };
    }
    if let Some(discharging) = st.lock().unwrap().battery_discharging() {
        return if discharging { PowerState::Battery } else { PowerState::Ac };
    }
    PowerState::Battery
}

// ---------------------------------------------------------------------------
// The router.
// ---------------------------------------------------------------------------

/// Bare verb router (no logging) — used directly by tests. The policy engine
/// rides its OWN `Arc<Mutex<Policy>>`, separate from the sampler `State`, so a
/// slow `apply()` never holds the sampler lock.
pub fn router(st: Arc<Mutex<State>>, policy: Arc<Mutex<PolicyHandle>>) -> Router {
    let s = st.clone();
    let system_check = handler(move |q: RpcRequest| {
        let st = s.lock().unwrap();
        RpcResponse::ok(
            q.id,
            json!({ "schema": SCHEMA, "kind": "system_check", "sources": st.sources_now() }),
        )
    });

    let s = st.clone();
    let session_start = handler(move |q: RpcRequest| {
        let interval_ms = p_u64(&q.params, "interval_ms").unwrap_or(DEFAULT_INTERVAL_MS);
        if interval_ms < MIN_INTERVAL_MS {
            return usage(q.id, format!("interval_ms must be >= {MIN_INTERVAL_MS}"));
        }
        let sid = p_str(&q.params, "session_id");
        let (info, created) = s.lock().unwrap().start_session(sid, interval_ms);
        RpcResponse::ok(
            q.id,
            json!({
                "session_id": info.id,
                "interval_ms": info.interval_ms,
                "started_unix_ms": info.started_unix_ms,
                "created": created,
            }),
        )
    });

    let s = st.clone();
    let session_stop = handler(move |q: RpcRequest| {
        let Some(sid) = p_str(&q.params, "session_id") else {
            return usage(q.id, "session_id is required");
        };
        let already_stopped = s.lock().unwrap().stop_session(&sid);
        RpcResponse::ok(
            q.id,
            json!({ "session_id": sid, "stopped": true, "already_stopped": already_stopped }),
        )
    });

    let s = st.clone();
    let session_list = handler(move |q: RpcRequest| {
        let st = s.lock().unwrap();
        let sessions: Vec<Value> = st
            .sessions
            .values()
            .map(|i| {
                json!({
                    "session_id": i.id,
                    "interval_ms": i.interval_ms,
                    "started_unix_ms": i.started_unix_ms,
                })
            })
            .collect();
        RpcResponse::ok(q.id, json!({ "sessions": sessions }))
    });

    let s = st.clone();
    let score_get = handler(move |q: RpcRequest| {
        let window_s = p_f64(&q.params, "window_s").unwrap_or(DEFAULT_WINDOW_S);
        if window_s.is_nan() || window_s <= 0.0 {
            return usage(q.id, "window_s must be > 0");
        }
        let mut st = s.lock().unwrap();
        if st.sessions.is_empty() {
            // One-shot: two inline samples 1 s apart.
            match st.one_shot() {
                Ok(rec) => {
                    let sc = st.make_scorecard(&[&rec]);
                    RpcResponse::ok(q.id, serde_json::to_value(sc).unwrap())
                }
                Err(e) => internal(q.id, format!("one-shot sampling failed: {e}")),
            }
        } else {
            let records = st.window_records(window_s);
            if records.is_empty() {
                return busy(
                    q.id,
                    "sampling warmup: no completed tick yet",
                    "retry after one interval",
                );
            }
            let sc = st.make_scorecard(&records);
            RpcResponse::ok(q.id, serde_json::to_value(sc).unwrap())
        }
    });

    let s = st.clone();
    let subscribe = handler(move |q: RpcRequest| {
        let kinds: Vec<String> = match q.params.as_ref().and_then(|p| p.get("kinds")) {
            None => vec!["tick".to_string()],
            Some(v) => match v.as_array() {
                Some(a) => a.iter().filter_map(|k| k.as_str().map(String::from)).collect(),
                None => return usage(q.id, "kinds must be an array of strings"),
            },
        };
        if kinds.is_empty() || kinds.iter().any(|k| k != "tick") {
            return usage(q.id, "only kind \"tick\" is supported");
        }
        let mut st = s.lock().unwrap();
        let Some(conn) = st.current_conn else {
            return internal(q.id, "subscribe requires a socket connection");
        };
        st.subscribers.insert(conn, ());
        RpcResponse::ok(q.id, json!({ "subscribed": true, "kinds": kinds }))
    });

    let s = st.clone();
    let offenders_get = handler(move |q: RpcRequest| {
        let limit = p_u64(&q.params, "limit").unwrap_or(DEFAULT_OFFENDER_LIMIT) as usize;
        // `window_s` is accepted for API symmetry; the effective window is the
        // last drain's, reported truthfully in meta.window_s.
        let off = s.lock().unwrap().make_offenders(limit);
        RpcResponse::ok(q.id, serde_json::to_value(off).unwrap())
    });

    // -- policy verbs: the act layer. Each locks ONLY the policy mutex for the
    //    engine call; power-state detection takes a brief, separate State lock
    //    (released first), so a slow apply never blocks the sampler. --------
    let s = st.clone();
    let p = policy.clone();
    let policy_plan = handler(move |q: RpcRequest| {
        let state = detect_power_state(p_bool(&q.params, "on_battery"), &s);
        match &p.lock().unwrap().engine {
            Policy::Unavailable(status) => policy_unavailable(q.id, status),
            Policy::Ready(engine) => match engine.plan(state) {
                Ok(plan) => RpcResponse::ok(q.id, serde_json::to_value(plan).unwrap()),
                Err(e) => internal(q.id, format!("policy.plan failed: {e}")),
            },
        }
    });

    let s = st.clone();
    let p = policy.clone();
    let policy_apply = handler(move |q: RpcRequest| {
        // dry_run DEFAULTS TO TRUE: a missing flag must never mutate the host.
        let dry_run = p_bool(&q.params, "dry_run").unwrap_or(true);
        let state = detect_power_state(p_bool(&q.params, "on_battery"), &s);
        let mut g = p.lock().unwrap();
        match &mut g.engine {
            Policy::Unavailable(status) => policy_unavailable(q.id, status),
            Policy::Ready(engine) => match engine.apply(state, dry_run) {
                Ok(res) => {
                    // A real battery apply is now the active power-save plan.
                    if !res.dry_run && state == PowerState::Battery {
                        g.active_txn = Some(res.txn_id.clone());
                    }
                    RpcResponse::ok(q.id, serde_json::to_value(res).unwrap())
                }
                Err(e) => internal(q.id, format!("policy.apply failed: {e}")),
            },
        }
    });

    let p = policy.clone();
    let policy_revert = handler(move |q: RpcRequest| {
        let Some(txn_id) = p_str(&q.params, "txn_id") else {
            return usage(q.id, "txn_id is required");
        };
        let mut g = p.lock().unwrap();
        match &mut g.engine {
            Policy::Unavailable(status) => policy_unavailable(q.id, status),
            Policy::Ready(engine) => match engine.revert(&txn_id) {
                Ok(res) => {
                    if g.active_txn.as_deref() == Some(txn_id.as_str()) {
                        g.active_txn = None;
                    }
                    RpcResponse::ok(q.id, serde_json::to_value(res).unwrap())
                }
                Err(e) => internal(q.id, format!("policy.revert failed: {e}")),
            },
        }
    });

    // -- power.status: compact state a bar renders and a toggle acts on. -----
    let s = st.clone();
    let p = policy.clone();
    let power_status = handler(move |q: RpcRequest| {
        let status = build_power_status(&s, &p);
        RpcResponse::ok(q.id, serde_json::to_value(status).unwrap())
    });

    // -- power.toggle: flip power-save. Applies the battery plan when off,
    //    reverts the active txn when on. This is what a bar's BAT click runs. -
    let s = st.clone();
    let p = policy.clone();
    let power_toggle = handler(move |q: RpcRequest| {
        let active = p.lock().unwrap().active_txn.clone();
        let mut g = p.lock().unwrap();
        let engine = match &mut g.engine {
            Policy::Unavailable(status) => return policy_unavailable(q.id, status),
            Policy::Ready(engine) => engine,
        };
        let outcome = match active {
            // Power-save is on → turn it off.
            Some(txn) => match engine.revert(&txn) {
                Ok(_) => {
                    g.active_txn = None;
                    Ok(false)
                }
                Err(e) => Err(format!("toggle off (revert) failed: {e}")),
            },
            // Power-save is off → turn it on with the battery plan.
            None => match engine.apply(PowerState::Battery, false) {
                Ok(res) => {
                    g.active_txn = Some(res.txn_id.clone());
                    Ok(true)
                }
                Err(e) => Err(format!("toggle on (apply) failed: {e}")),
            },
        };
        drop(g);
        match outcome {
            Ok(_) => RpcResponse::ok(q.id, serde_json::to_value(build_power_status(&s, &p)).unwrap()),
            Err(e) => internal(q.id, e),
        }
    });

    Router::new()
        .route("system.check", system_check)
        .route("session.start", session_start)
        .route("session.stop", session_stop)
        .route("session.list", session_list)
        .route("score.get", score_get)
        .route("offenders.get", offenders_get)
        .route("policy.plan", policy_plan)
        .route("policy.apply", policy_apply)
        .route("policy.revert", policy_revert)
        .route("power.status", power_status)
        .route("power.toggle", power_toggle)
        .route("subscribe", subscribe)
}

/// Assemble a [`PowerStatus`] from the sampler state and the policy handle.
fn build_power_status(st: &Arc<Mutex<State>>, policy: &Arc<Mutex<PolicyHandle>>) -> PowerStatus {
    let on_battery = detect_power_state(None, st) == PowerState::Battery;
    let active_txn_id = policy.lock().unwrap().active_txn.clone();
    let (sleep_efficiency, grade) = {
        let s = st.lock().unwrap();
        s.latest_sleep_efficiency()
    };
    PowerStatus {
        schema: SCHEMA,
        kind: "power_status".into(),
        on_battery,
        power_save_active: active_txn_id.is_some(),
        active_txn_id,
        sleep_efficiency,
        grade,
    }
}

/// The full production stack: router wrapped in the logging filter.
pub fn service(st: Arc<Mutex<State>>, policy: Arc<Mutex<PolicyHandle>>) -> Log<Router> {
    LogLayer.layer(router(st, policy))
}

/// Build an `Unavailable` policy handle. Test-only: production builds the
/// handle from `drowse_policy::probe()`, which is always Ready.
#[cfg(test)]
pub fn unavailable_policy(detail: &str) -> Arc<Mutex<PolicyHandle>> {
    Arc::new(Mutex::new(PolicyHandle::new(Policy::Unavailable(
        SourceStatus::err(SourceState::NotPresent, detail),
    ))))
}
