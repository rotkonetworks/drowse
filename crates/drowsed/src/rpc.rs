//! The verb surface: plain functions lifted into the drowse-core service
//! stack. Handlers only transform state; arming/disarming the timer is the
//! transport loop's job (it re-reads `min_interval_ms` after every dispatch).

use crate::state::State;
use drowse_api::{ErrorCode, ErrorData, RpcRequest, RpcResponse, SCHEMA};
use drowse_core::service::{drive, handler, Layer, Router, RpcFuture, Service};
use serde_json::{json, Value};
use std::convert::Infallible;
use std::future::ready;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

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

// ---------------------------------------------------------------------------
// The router.
// ---------------------------------------------------------------------------

/// Bare verb router (no logging) — used directly by tests.
pub fn router(st: Arc<Mutex<State>>) -> Router {
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
        if !(window_s > 0.0) {
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

    Router::new()
        .route("system.check", system_check)
        .route("session.start", session_start)
        .route("session.stop", session_stop)
        .route("session.list", session_list)
        .route("score.get", score_get)
        .route("subscribe", subscribe)
}

/// The full production stack: router wrapped in the logging filter.
pub fn service(st: Arc<Mutex<State>>) -> Log<Router> {
    LogLayer.layer(router(st))
}
