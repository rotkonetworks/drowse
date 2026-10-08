//! The drowse wire contract: every object an agent sees, nothing else.
//!
//! This crate is the single source of truth for the JSON surface. It has no
//! I/O and depends on nothing but serde. Schema evolution is additive-only;
//! bump [`SCHEMA`] on breaking change.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SCHEMA: u32 = 1;

// ---------------------------------------------------------------------------
// Errors: a closed enum, mapped 1:1 to stable CLI exit codes.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Permission,
    NotPresent,
    SessionUnknown,
    Busy,
    Kernel,
    DaemonUnreachable,
    Usage,
    Internal,
}

impl ErrorCode {
    /// Stable CLI exit codes. Tested in CI; agents branch on `$?`.
    pub fn exit_code(self) -> i32 {
        match self {
            ErrorCode::Usage => 2,
            ErrorCode::Permission => 10,
            ErrorCode::NotPresent => 11,
            ErrorCode::DaemonUnreachable => 12,
            ErrorCode::SessionUnknown => 14,
            ErrorCode::Busy => 15,
            ErrorCode::Kernel => 16,
            ErrorCode::Internal => 1,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorData {
    pub code: ErrorCode,
    pub detail: String,
    pub retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

// ---------------------------------------------------------------------------
// Sources: typed degradation, never a crash.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceState {
    Ok,
    Degraded,
    PermissionDenied,
    NotPresent,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceStatus {
    pub state: SourceState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl SourceStatus {
    pub fn ok() -> Self {
        Self { state: SourceState::Ok, detail: None }
    }
    pub fn err(state: SourceState, detail: impl Into<String>) -> Self {
        Self { state, detail: Some(detail.into()) }
    }
}

// ---------------------------------------------------------------------------
// Scorecard: the M1 payload of `score.get`.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scorecard {
    pub schema: u32,
    pub kind: String, // "scorecard"
    pub meta: Meta,
    pub sources: BTreeMap<String, SourceStatus>,
    pub score: Score,
    /// Per-CPU idle-state residency over the window, percent of wall time.
    pub cpu: Vec<CpuIdle>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub battery: Option<Battery>,
    /// RAPL package power over the window, watts. Null when unreadable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package_power_w: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub hostname: String,
    pub kernel: String,
    pub sampled_at_unix_ms: u64,
    pub window_s: f64,
    pub ticks: u32,
    pub degraded: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Score {
    /// M1 approximation: mean residency of the deepest advertised idle state
    /// across all CPUs over the window. OS-timed upper bound, not hardware
    /// truth — see `basis`. Null when cpuidle is unreadable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sleep_efficiency: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grade: Option<Grade>,
    /// Name of the deepest state used for SE (e.g. "C3" slot naming CC6).
    pub deepest_state: String,
    /// How SE was derived. M1: "os-timed-mean"; M2+: "window-overlap".
    pub basis: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_draw_w: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_est_min: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Grade {
    A,
    B,
    C,
    D,
    E,
    F,
}

impl Grade {
    /// Fixed thresholds so humans and CI gates share one vocabulary.
    pub fn from_se(se: f64) -> Grade {
        match se {
            s if s >= 90.0 => Grade::A,
            s if s >= 75.0 => Grade::B,
            s if s >= 60.0 => Grade::C,
            s if s >= 50.0 => Grade::D,
            s if s >= 40.0 => Grade::E,
            _ => Grade::F,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuIdle {
    pub cpu: u32,
    /// state name -> residency percent of the window.
    pub states: BTreeMap<String, f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Battery {
    pub power_w: f64,
    pub discharging: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capacity_pct: Option<f64>,
}

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 framing (newline-delimited over the unix socket / stdio).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String, // "2.0"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<serde_json::Value>,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64, // JSON-RPC numeric code (-32601 method not found, etc.)
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<ErrorData>,
}

impl RpcResponse {
    pub fn ok(id: Option<serde_json::Value>, result: serde_json::Value) -> Self {
        Self { jsonrpc: "2.0".into(), id, result: Some(result), error: None }
    }
    pub fn err(id: Option<serde_json::Value>, code: i64, message: impl Into<String>, data: Option<ErrorData>) -> Self {
        Self { jsonrpc: "2.0".into(), id, result: None, error: Some(RpcError { code, message: message.into(), data }) }
    }
}

// ---------------------------------------------------------------------------
// Attribution: the M2 payload of `offenders.get`.
// ---------------------------------------------------------------------------

/// What woke the CPU. Attribution resolves the blame chain to one of these at
/// the moment of `sched_waking` (or a timer/irq firing with no resulting task).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OffenderKind {
    /// A userspace task (process/thread) woke the CPU.
    Process,
    /// A timer callback fired (hrtimer or timer); `name` is the callback symbol.
    Timer,
    /// A hardware IRQ; `name` is the IRQ line / handler.
    Irq,
    /// A softirq (net rx/tx, rcu, sched); `name` is the vector.
    Softirq,
    /// A workqueue item; `name` is the work function symbol.
    Workqueue,
}

/// One row of the wakeup-attribution table, aggregated over the window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Offender {
    /// Stable id an agent can reference across calls, e.g. "proc:1663:chromium"
    /// or "timer:tick_sched_timer".
    pub id: String,
    pub kind: OffenderKind,
    /// Human one-liner: "wakes the CPU 312x/s (timer storm, 3 threads)".
    pub summary: String,
    /// Wakeups per second this offender caused over the window.
    pub wakeups_per_s: f64,
    /// Of those, the rate that interrupted an already-idle CPU.
    pub idle_breaks_per_s: f64,
    /// Of those, the rate that broke a window where ALL cpus were idle — the
    /// package-sleep breaks, the ones that actually cost battery.
    pub pkg_breaks_per_s: f64,
    /// pid/tgid for Process kind; None otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Share of total attributed wakeups, 0..1.
    pub share: f64,
}

/// Result of `offenders.get`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Offenders {
    pub schema: u32,
    pub kind: String, // "offenders"
    pub meta: Meta,
    /// Availability of the attribution subsystem itself (needs CAP_BPF).
    pub source: SourceStatus,
    /// Ranked most-costly first (by pkg_breaks_per_s, then wakeups_per_s).
    pub offenders: Vec<Offender>,
    /// Drowse's own wakeups, reported honestly as its own row's rate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observer_wakeups_per_s: Option<f64>,
    /// Package-idle window stats over the measurement: windows/s and the
    /// mean/p95 window length in milliseconds (null until M2 histogram lands).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_ms_mean: Option<f64>,
}

/// Default socket path for the unprivileged user instance.
pub fn user_socket_path() -> std::path::PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    dir.join("drowsed.sock")
}

/// Socket path of the privileged system instance.
pub const SYSTEM_SOCKET: &str = "/run/drowsed/sock";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grades_are_fixed() {
        assert_eq!(Grade::from_se(90.0), Grade::A);
        assert_eq!(Grade::from_se(89.9), Grade::B);
        assert_eq!(Grade::from_se(39.9), Grade::F);
    }

    #[test]
    fn exit_codes_are_stable() {
        // These are API: changing them breaks agents. Update SCHEMA if you must.
        assert_eq!(ErrorCode::Usage.exit_code(), 2);
        assert_eq!(ErrorCode::Permission.exit_code(), 10);
        assert_eq!(ErrorCode::NotPresent.exit_code(), 11);
        assert_eq!(ErrorCode::DaemonUnreachable.exit_code(), 12);
    }
}
