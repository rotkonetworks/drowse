//! drowse: CLI client. Each subcommand is exactly one RPC plus a formatter;
//! JSON is the default output when stdout is not a tty.
//!
//! Subcommand -> RPC mapping (M1/M2):
//!   drowse check [--gate A..F]        -> score.get     (one-shot inline sample)
//!   drowse score [--window N]         -> score.get     (params: {"window_s": N})
//!   drowse watch                      -> subscribe     (one line per tick)
//!   drowse offenders [--window N]     -> offenders.get (ranked by battery cost)
//!                    [--top K]
//!
//! Exit codes: 0 success; ErrorCode::exit_code() for RPC errors (2 usage,
//! 10 permission, 11 not-present, 12 daemon unreachable, 14 session unknown,
//! 15 busy, 16 kernel, 1 internal); 20 when --gate is not met.

mod mcp;

use drowse_api::{
    ErrorCode, Grade, Offenders, RpcRequest, RpcResponse, Scorecard, SourceState,
};
use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Exit code for a failed `--gate` check. Not an ErrorCode: the RPC succeeded,
/// the machine just isn't healthy enough. Agents branch on it.
const EXIT_GATE_FAILED: i32 = 20;

const USAGE: &str = "drowse - package idle health, agent-first

USAGE: drowse [--json|--table] [--socket PATH] <command>

COMMANDS:
  check [--gate A..F]        one-shot health check (exit 20 if grade below the bar)
  score [--window N]         fetch the current scorecard (window in seconds)
  watch                      stream one score line per tick until Ctrl-C
  offenders [--window N]     who wakes the CPU, ranked by battery cost
            [--top K]        (needs the root daemon for eBPF attribution)
  mcp-serve                  speak MCP over stdio so an AI agent can drive drowse

OUTPUT: human text on a tty, JSON otherwise; --json / --table force either.
";

fn main() {
    std::process::exit(run());
}

// ---------------------------------------------------------------------------
// CLI plumbing
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Output {
    Json,
    Table,
}

struct Cli {
    cmd: Cmd,
    output: Output,
    socket: Option<PathBuf>,
}

enum Cmd {
    Check { gate: Option<Grade> },
    Score { window_s: Option<f64> },
    Watch,
    Offenders { window_s: Option<f64>, top: Option<usize> },
    /// Stdio MCP server: translates MCP tool calls into drowsed verb calls.
    McpServe,
}

struct Fail {
    exit: i32,
    msg: String,
}

impl Fail {
    fn new(exit: i32, msg: impl Into<String>) -> Self {
        Fail { exit, msg: msg.into() }
    }
    fn usage(msg: impl Into<String>) -> Self {
        Fail::new(ErrorCode::Usage.exit_code(), msg)
    }
}

fn run() -> i32 {
    let cli = match parse_args(std::env::args().skip(1)) {
        Ok(cli) => cli,
        Err(f) => {
            eprintln!("drowse: {}", f.msg);
            eprint!("{USAGE}");
            return f.exit;
        }
    };
    // `mcp-serve` does not open a connection up front: it is a long-lived stdio
    // server that opens a short-lived socket per tool call, so the daemon may
    // start (or auto-spawn) after the server is already running.
    if let Cmd::McpServe = cli.cmd {
        return match mcp::serve(cli.socket) {
            Ok(code) => code,
            Err(f) => {
                eprintln!("drowse: {}", f.msg);
                f.exit
            }
        };
    }
    let stream = match connect(cli.socket.as_deref()) {
        Ok(s) => s,
        Err(f) => {
            eprintln!("drowse: {}", f.msg);
            return f.exit;
        }
    };
    let r = match cli.cmd {
        // `check` is the one-shot health measure: score.get without a session
        // performs an inline 2-tick sample, and the scorecard embeds the
        // sources table, so one verb serves both the grade and the reasons.
        Cmd::Check { gate } => cmd_scorecard(stream, "score.get", None, cli.output, gate),
        Cmd::Score { window_s } => {
            let params = window_s.map(|w| serde_json::json!({ "window_s": w }));
            cmd_scorecard(stream, "score.get", params, cli.output, None)
        }
        Cmd::Watch => cmd_watch(stream, cli.output),
        Cmd::Offenders { window_s, top } => cmd_offenders(stream, window_s, top, cli.output),
        // Handled above with an early return, before the up-front connect.
        Cmd::McpServe => unreachable!("mcp-serve dispatched before connect"),
    };
    match r {
        Ok(code) => code,
        Err(f) => {
            eprintln!("drowse: {}", f.msg);
            f.exit
        }
    }
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Cli, Fail> {
    let mut output: Option<Output> = None;
    let mut socket: Option<PathBuf> = None;
    let mut cmd: Option<String> = None;
    let mut gate: Option<Grade> = None;
    let mut window_s: Option<f64> = None;
    let mut top: Option<usize> = None;

    let mut it = args.peekable();
    while let Some(arg) = it.next() {
        // Accept both `--flag value` and `--flag=value`.
        let (flag, mut inline) = match arg.split_once('=') {
            Some((f, v)) => (f.to_string(), Some(v.to_string())),
            None => (arg.clone(), None),
        };
        match flag.as_str() {
            "--json" => output = Some(Output::Json),
            "--table" => output = Some(Output::Table),
            "--socket" => {
                let v = take_value("--socket", inline.take(), &mut it)?;
                socket = Some(PathBuf::from(v));
            }
            "--gate" => {
                let v = take_value("--gate", inline.take(), &mut it)?;
                gate = Some(
                    parse_grade(&v)
                        .ok_or_else(|| Fail::usage(format!("--gate takes A..F, not {v:?}")))?,
                );
            }
            "--window" => {
                let v = take_value("--window", inline.take(), &mut it)?;
                let w = v
                    .parse::<f64>()
                    .ok()
                    .filter(|w| *w > 0.0 && w.is_finite())
                    .ok_or_else(|| Fail::usage(format!("--window takes seconds, not {v:?}")))?;
                window_s = Some(w);
            }
            "--top" => {
                let v = take_value("--top", inline.take(), &mut it)?;
                let k = v
                    .parse::<usize>()
                    .ok()
                    .filter(|k| *k > 0)
                    .ok_or_else(|| Fail::usage(format!("--top takes a count > 0, not {v:?}")))?;
                top = Some(k);
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            f if f.starts_with('-') => return Err(Fail::usage(format!("unknown flag {f}"))),
            _ if cmd.is_none() => cmd = Some(arg),
            other => return Err(Fail::usage(format!("unexpected argument {other:?}"))),
        }
    }

    // `--gate` is check-only; `--top` is offenders-only; `--window` serves
    // score and offenders.
    let cmd = match cmd.as_deref() {
        Some("check") => {
            if window_s.is_some() || top.is_some() {
                return Err(Fail::usage("`check` takes no --window/--top"));
            }
            Cmd::Check { gate }
        }
        Some("score") => {
            if gate.is_some() || top.is_some() {
                return Err(Fail::usage("`score` takes no --gate/--top"));
            }
            Cmd::Score { window_s }
        }
        Some("watch") => {
            if gate.is_some() || window_s.is_some() || top.is_some() {
                return Err(Fail::usage("`watch` takes no --gate/--window/--top"));
            }
            Cmd::Watch
        }
        Some("offenders") => {
            if gate.is_some() {
                return Err(Fail::usage("--gate only applies to `check`"));
            }
            Cmd::Offenders { window_s, top }
        }
        Some("mcp-serve") => {
            if gate.is_some() || window_s.is_some() || top.is_some() {
                return Err(Fail::usage("`mcp-serve` takes no --gate/--window/--top"));
            }
            Cmd::McpServe
        }
        Some(other) => return Err(Fail::usage(format!("unknown command {other:?}"))),
        None => return Err(Fail::usage("missing command")),
    };

    // Default: humans at a tty get text, everything else gets JSON.
    let output = output
        .unwrap_or_else(|| if std::io::stdout().is_terminal() { Output::Table } else { Output::Json });
    Ok(Cli { cmd, output, socket })
}

fn take_value(
    name: &str,
    inline: Option<String>,
    it: &mut std::iter::Peekable<impl Iterator<Item = String>>,
) -> Result<String, Fail> {
    if let Some(v) = inline {
        return Ok(v);
    }
    it.next().ok_or_else(|| Fail::usage(format!("{name} needs a value")))
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

fn connect(explicit: Option<&std::path::Path>) -> Result<UnixStream, Fail> {
    if let Some(path) = explicit {
        return UnixStream::connect(path).map_err(|e| {
            Fail::new(
                ErrorCode::DaemonUnreachable.exit_code(),
                format!("can't reach drowsed at {} ({e})", path.display()),
            )
        });
    }
    if let Ok(s) = UnixStream::connect(drowse_api::SYSTEM_SOCKET) {
        return Ok(s);
    }
    let user = drowse_api::user_socket_path();
    if let Ok(s) = UnixStream::connect(&user) {
        return Ok(s);
    }
    // Stretch goal: auto-spawn a user daemon from PATH and wait briefly.
    if let Some(s) = spawn_and_wait(&user) {
        return Ok(s);
    }
    Err(Fail::new(
        ErrorCode::DaemonUnreachable.exit_code(),
        format!(
            "no daemon at {} or {} - start one with: drowsed",
            drowse_api::SYSTEM_SOCKET,
            user.display()
        ),
    ))
}

fn spawn_and_wait(path: &std::path::Path) -> Option<UnixStream> {
    use std::process::{Command, Stdio};
    // If drowsed isn't on PATH this fails immediately and we fall through
    // to the DaemonUnreachable hint.
    Command::new("drowsed")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Ok(s) = UnixStream::connect(path) {
            return Some(s);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

// ---------------------------------------------------------------------------
// RPC
// ---------------------------------------------------------------------------

fn send_request(
    stream: &mut UnixStream,
    method: &str,
    params: Option<serde_json::Value>,
) -> Result<(), Fail> {
    let req = RpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(serde_json::Value::from(1)),
        method: method.into(),
        params,
    };
    let mut line = serde_json::to_string(&req).expect("request serializes");
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .map_err(daemon_lost)
}

fn daemon_lost(e: std::io::Error) -> Fail {
    Fail::new(
        ErrorCode::DaemonUnreachable.exit_code(),
        format!("lost connection to drowsed ({e})"),
    )
}

/// Parse one response line; unwrap result or map the error to a Fail with the
/// stable exit code from ErrorData (Internal when the daemon sent none).
fn unwrap_response(line: &str) -> Result<serde_json::Value, Fail> {
    let resp: RpcResponse = serde_json::from_str(line).map_err(|e| {
        Fail::new(
            ErrorCode::Internal.exit_code(),
            format!("bad response from drowsed ({e})"),
        )
    })?;
    if let Some(err) = resp.error {
        let (exit, mut msg) = match &err.data {
            Some(d) => (d.code.exit_code(), format!("{} ({})", err.message, d.detail)),
            None => (ErrorCode::Internal.exit_code(), err.message.clone()),
        };
        if let Some(hint) = err.data.as_ref().and_then(|d| d.hint.as_deref()) {
            msg.push_str(&format!("\n  hint: {hint}"));
        }
        return Err(Fail::new(exit, msg));
    }
    resp.result.ok_or_else(|| {
        Fail::new(
            ErrorCode::Internal.exit_code(),
            "drowsed sent neither result nor error",
        )
    })
}

fn call_once(
    mut stream: UnixStream,
    method: &str,
    params: Option<serde_json::Value>,
) -> Result<serde_json::Value, Fail> {
    send_request(&mut stream, method, params)?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let n = reader.read_line(&mut line).map_err(daemon_lost)?;
    if n == 0 {
        return Err(Fail::new(
            ErrorCode::DaemonUnreachable.exit_code(),
            "drowsed closed the connection before replying",
        ));
    }
    unwrap_response(line.trim_end())
}

// ---------------------------------------------------------------------------
// Subcommands
// ---------------------------------------------------------------------------

/// `check` and `score`: one call, one scorecard, optional gate.
fn cmd_scorecard(
    stream: UnixStream,
    method: &str,
    params: Option<serde_json::Value>,
    output: Output,
    gate: Option<Grade>,
) -> Result<i32, Fail> {
    let result = call_once(stream, method, params)?;
    match output {
        Output::Json => print_json(&result),
        Output::Table => match serde_json::from_value::<Scorecard>(result.clone()) {
            Ok(card) => print!("{}", render_scorecard(&card)),
            // Unknown result shape: show it rather than crash. Additive schema
            // evolution means this should stay rare.
            Err(_) => print_json(&result),
        },
    }
    if let Some(bar) = gate {
        let grade = serde_json::from_value::<Scorecard>(result)
            .ok()
            .and_then(|c| c.score.grade);
        if let Some(verdict) = gate_failure(bar, grade) {
            eprintln!("drowse: gate: {verdict}");
            return Ok(EXIT_GATE_FAILED);
        }
    }
    Ok(0)
}

fn cmd_watch(mut stream: UnixStream, output: Output) -> Result<i32, Fail> {
    send_request(&mut stream, "subscribe", None)?;
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line.map_err(daemon_lost)?;
        if line.trim().is_empty() {
            continue;
        }
        let result = unwrap_response(line.trim_end())?;
        match output {
            // NDJSON for machines: always one compact object per line.
            Output::Json => println!("{result}"),
            Output::Table => {
                let text = match serde_json::from_value::<Scorecard>(result.clone()) {
                    Ok(card) => render_tick(&card),
                    Err(_) => result.to_string(),
                };
                println!("{text}");
            }
        }
        // A broken pipe downstream (e.g. `drowse watch | head`) ends us.
        if std::io::stdout().flush().is_err() {
            break;
        }
    }
    Ok(0)
}

/// `offenders`: one call, a table ranked by battery cost (package-sleep
/// breaks). JSON by default when piped; a human table on a tty.
fn cmd_offenders(
    stream: UnixStream,
    window_s: Option<f64>,
    top: Option<usize>,
    output: Output,
) -> Result<i32, Fail> {
    let mut params = serde_json::Map::new();
    if let Some(w) = window_s {
        params.insert("window_s".into(), serde_json::json!(w));
    }
    if let Some(k) = top {
        params.insert("limit".into(), serde_json::json!(k));
    }
    let params = (!params.is_empty()).then_some(serde_json::Value::Object(params));

    let result = call_once(stream, "offenders.get", params)?;
    match output {
        Output::Json => print_json(&result),
        Output::Table => match serde_json::from_value::<Offenders>(result.clone()) {
            Ok(off) => print!("{}", render_offenders(&off)),
            Err(_) => print_json(&result),
        },
    }
    Ok(0)
}

fn print_json(v: &serde_json::Value) {
    if std::io::stdout().is_terminal() {
        println!("{}", serde_json::to_string_pretty(v).expect("value re-serializes"));
    } else {
        println!("{v}");
    }
}

// ---------------------------------------------------------------------------
// Formatting (pure: unit-tested below)
// ---------------------------------------------------------------------------

fn grade_rank(g: Grade) -> u8 {
    match g {
        Grade::A => 0,
        Grade::B => 1,
        Grade::C => 2,
        Grade::D => 3,
        Grade::E => 4,
        Grade::F => 5,
    }
}

fn grade_str(g: Grade) -> &'static str {
    match g {
        Grade::A => "A",
        Grade::B => "B",
        Grade::C => "C",
        Grade::D => "D",
        Grade::E => "E",
        Grade::F => "F",
    }
}

fn parse_grade(s: &str) -> Option<Grade> {
    match s.trim().to_ascii_uppercase().as_str() {
        "A" => Some(Grade::A),
        "B" => Some(Grade::B),
        "C" => Some(Grade::C),
        "D" => Some(Grade::D),
        "E" => Some(Grade::E),
        "F" => Some(Grade::F),
        _ => None,
    }
}

/// None = gate passes; Some(reason) = fail with exit 20.
fn gate_failure(bar: Grade, measured: Option<Grade>) -> Option<String> {
    match measured {
        None => Some(format!(
            "no grade measured (score unavailable), can't clear the {} bar",
            grade_str(bar)
        )),
        Some(g) if grade_rank(g) > grade_rank(bar) => Some(format!(
            "grade {} is below the {} bar",
            grade_str(g),
            grade_str(bar)
        )),
        Some(_) => None,
    }
}

fn fmt_runtime(min: u64) -> String {
    if min < 60 {
        format!("~{min}m")
    } else {
        format!("~{}h {:02}m", min / 60, min % 60)
    }
}

/// The headline: "SE 71/100 (C) · 4.1 W → ~7h 50m".
fn score_line(card: &Scorecard) -> String {
    let s = &card.score;
    let mut line = match (s.sleep_efficiency, s.grade) {
        (Some(se), Some(g)) => format!("SE {:.0}/100 ({})", se, grade_str(g)),
        (Some(se), None) => format!("SE {se:.0}/100"),
        _ => "SE — (not measurable right now)".to_string(),
    };
    if let Some(w) = s.idle_draw_w {
        line.push_str(&format!(" · {w:.1} W"));
    }
    if let Some(min) = s.runtime_est_min {
        line.push_str(&format!(" → {}", fmt_runtime(min)));
    }
    line
}

/// Plain-verb phrasing for a source's state; no kernel jargon.
fn source_phrase(state: SourceState, detail: Option<&str>) -> String {
    let base = match state {
        SourceState::Ok => "ok",
        SourceState::Degraded => "degraded",
        SourceState::PermissionDenied => "can't read (no permission)",
        SourceState::NotPresent => "not on this machine",
        SourceState::Error => "failed",
    };
    match detail {
        Some(d) if state != SourceState::Ok => format!("{base} — {d}"),
        _ => base.to_string(),
    }
}

/// Full human rendering of a scorecard (check/score on a tty).
fn render_scorecard(card: &Scorecard) -> String {
    let mut out = String::new();
    out.push_str(&score_line(card));
    out.push('\n');

    let s = &card.score;
    out.push_str(&format!(
        "deepest sleep state {} · basis {} · window {:.0}s ({} ticks) on {}\n",
        s.deepest_state, s.basis, card.meta.window_s, card.meta.ticks, card.meta.hostname
    ));
    if let Some(b) = &card.battery {
        let verb = if b.discharging { "draining" } else { "charging or full" };
        let cap = b
            .capacity_pct
            .map(|c| format!(" · {c:.0}% charge"))
            .unwrap_or_default();
        out.push_str(&format!("battery {verb} at {:.1} W{cap}\n", b.power_w));
    }
    if let Some(w) = card.package_power_w {
        out.push_str(&format!("package power {w:.1} W\n"));
    }

    if card.meta.degraded {
        out.push_str("some readings are missing — the numbers above are partial:\n");
    }
    out.push_str("sources:\n");
    let width = card.sources.keys().map(|k| k.len()).max().unwrap_or(0);
    for (name, st) in &card.sources {
        out.push_str(&format!(
            "  {name:<width$}  {}\n",
            source_phrase(st.state, st.detail.as_deref())
        ));
    }
    out
}

/// One line per tick for `watch` on a tty.
fn render_tick(card: &Scorecard) -> String {
    let mut line = format!("[tick {:>4}] {}", card.meta.ticks, score_line(card));
    if card.meta.degraded {
        line.push_str("  (partial)");
    }
    line
}

/// A per-second rate: whole at a glance, one decimal only when sub-10.
fn fmt_per_s(r: f64) -> String {
    if r >= 10.0 {
        format!("{r:.0}")
    } else {
        format!("{r:.1}")
    }
}

/// Human rendering of the offender table (`offenders` on a tty). Ranked by
/// battery cost — package-sleep breaks — which is the point: raw wakeups are
/// dominated by harmless timers; the breaks that shatter all-core-idle windows
/// are what drains the battery.
fn render_offenders(off: &Offenders) -> String {
    let mut out = String::new();

    // Attribution is a privileged subsystem; say so plainly when it's missing
    // rather than implying the machine is clean.
    if off.source.state != SourceState::Ok {
        out.push_str(&format!(
            "attribution unavailable: {}\n",
            source_phrase(off.source.state, off.source.detail.as_deref())
        ));
        out.push_str("  eBPF wakeup attribution needs the root daemon — run drowsed as root.\n");
        return out;
    }

    if off.offenders.is_empty() {
        out.push_str(&format!(
            "no wakeup offenders recorded over {:.0}s on {} yet.\n",
            off.meta.window_s, off.meta.hostname
        ));
        out.push_str("  Keep a session running so the attribution lane drains, then retry.\n");
        return out;
    }

    out.push_str(&format!(
        "wakeup offenders over {:.0}s on {} — ranked by battery cost (package-sleep breaks)\n",
        off.meta.window_s, off.meta.hostname
    ));
    out.push_str(&format!(
        "  {:>2}  {:<30}  {:>6}  {:>6}  {:>5}\n",
        "#", "offender", "pkg/s", "wake/s", "share"
    ));
    for (i, o) in off.offenders.iter().enumerate() {
        out.push_str(&format!(
            "  {:>2}  {:<30}  {:>6}  {:>6}  {:>4.0}%\n",
            i + 1,
            o.id,
            fmt_per_s(o.pkg_breaks_per_s),
            fmt_per_s(o.wakeups_per_s),
            o.share * 100.0,
        ));
    }
    if let Some(obs) = off.observer_wakeups_per_s {
        out.push_str(&format!(
            "drowse's own wakeups: {}/s (counted honestly, not ranked)\n",
            fmt_per_s(obs)
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Tests: formatter and gate logic on hand-built scorecards; no daemon.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use drowse_api::{Battery, Meta, Offender, OffenderKind, Score, SourceStatus};
    use std::collections::BTreeMap;

    fn card() -> Scorecard {
        let mut sources = BTreeMap::new();
        sources.insert("cpuidle".to_string(), SourceStatus::ok());
        sources.insert(
            "rapl".to_string(),
            SourceStatus::err(
                SourceState::PermissionDenied,
                "energy counters are root-only here",
            ),
        );
        Scorecard {
            schema: drowse_api::SCHEMA,
            kind: "scorecard".into(),
            meta: Meta {
                hostname: "x1".into(),
                kernel: "6.12".into(),
                sampled_at_unix_ms: 0,
                window_s: 30.0,
                ticks: 42,
                degraded: true,
            },
            sources,
            score: Score {
                sleep_efficiency: Some(71.2),
                grade: Some(Grade::C),
                deepest_state: "C3".into(),
                basis: "os-timed-mean".into(),
                idle_draw_w: Some(4.1),
                runtime_est_min: Some(470),
            },
            cpu: vec![],
            battery: Some(Battery {
                power_w: 4.1,
                discharging: true,
                capacity_pct: Some(63.0),
            }),
            package_power_w: Some(2.2),
        }
    }

    #[test]
    fn score_line_reads_like_the_brief() {
        assert_eq!(score_line(&card()), "SE 71/100 (C) · 4.1 W → ~7h 50m");
    }

    #[test]
    fn score_line_degrades_honestly_when_unmeasured() {
        let mut c = card();
        c.score.sleep_efficiency = None;
        c.score.grade = None;
        c.score.idle_draw_w = None;
        c.score.runtime_est_min = None;
        assert_eq!(score_line(&c), "SE — (not measurable right now)");
    }

    #[test]
    fn full_render_lists_sources_with_reasons() {
        let out = render_scorecard(&card());
        assert!(out.starts_with("SE 71/100 (C)"), "headline first: {out}");
        assert!(out.contains("cpuidle  ok"));
        assert!(out.contains("rapl     can't read (no permission) — energy counters are root-only here"));
        assert!(out.contains("some readings are missing"));
        assert!(out.contains("battery draining at 4.1 W · 63% charge"));
        assert!(out.contains("deepest sleep state C3"));
    }

    #[test]
    fn tick_line_is_one_line() {
        let line = render_tick(&card());
        assert!(!line.contains('\n'));
        assert!(line.contains("[tick   42] SE 71/100 (C)"));
        assert!(line.ends_with("(partial)"));
    }

    #[test]
    fn runtime_formatting() {
        assert_eq!(fmt_runtime(470), "~7h 50m");
        assert_eq!(fmt_runtime(45), "~45m");
        assert_eq!(fmt_runtime(60), "~1h 00m");
    }

    #[test]
    fn gate_passes_at_or_above_the_bar() {
        assert_eq!(gate_failure(Grade::B, Some(Grade::A)), None);
        assert_eq!(gate_failure(Grade::B, Some(Grade::B)), None);
    }

    #[test]
    fn gate_fails_below_the_bar() {
        let msg = gate_failure(Grade::B, Some(Grade::C)).expect("C < B must fail");
        assert_eq!(msg, "grade C is below the B bar");
    }

    #[test]
    fn gate_fails_closed_when_unmeasured() {
        assert!(gate_failure(Grade::F, None).is_some(), "no grade must not pass a gate");
    }

    #[test]
    fn grade_parsing() {
        assert_eq!(parse_grade("b"), Some(Grade::B));
        assert_eq!(parse_grade(" F "), Some(Grade::F));
        assert_eq!(parse_grade("G"), None);
        assert_eq!(parse_grade(""), None);
    }

    #[test]
    fn source_phrasing_is_plain() {
        assert_eq!(source_phrase(SourceState::Ok, Some("ignored")), "ok");
        assert_eq!(source_phrase(SourceState::NotPresent, None), "not on this machine");
        assert_eq!(
            source_phrase(SourceState::Degraded, Some("battery hidden by firmware")),
            "degraded — battery hidden by firmware"
        );
    }

    fn offenders_fixture(source: SourceStatus, rows: Vec<Offender>) -> Offenders {
        Offenders {
            schema: drowse_api::SCHEMA,
            kind: "offenders".into(),
            meta: Meta {
                hostname: "pocket4".into(),
                kernel: "7.2".into(),
                sampled_at_unix_ms: 0,
                window_s: 10.0,
                ticks: 1,
                degraded: source.state != SourceState::Ok,
            },
            source,
            offenders: rows,
            observer_wakeups_per_s: Some(0.5),
            window_ms_mean: None,
        }
    }

    fn offender(id: &str, kind: OffenderKind, pid: Option<u32>, wake: f64, pkg: f64, share: f64) -> Offender {
        Offender {
            id: id.into(),
            kind,
            summary: "x".into(),
            wakeups_per_s: wake,
            idle_breaks_per_s: pkg,
            pkg_breaks_per_s: pkg,
            pid,
            share,
        }
    }

    #[test]
    fn offenders_table_leads_with_battery_cost() {
        let off = offenders_fixture(
            SourceStatus::ok(),
            vec![
                offender("proc:1663:syncthing", OffenderKind::Process, Some(1663), 5.0, 4.0, 0.05),
                offender("timer:hrtimer_wakeup", OffenderKind::Timer, None, 100.0, 0.0, 0.95),
            ],
        );
        let out = render_offenders(&off);
        assert!(out.contains("ranked by battery cost (package-sleep breaks)"), "{out}");
        // Server ranks; the renderer keeps order — syncthing (high pkg) first.
        let sync_at = out.find("proc:1663:syncthing").unwrap();
        let timer_at = out.find("timer:hrtimer_wakeup").unwrap();
        assert!(sync_at < timer_at, "battery-costly offender must be listed first");
        assert!(out.contains("pkg/s"));
        assert!(out.contains("drowse's own wakeups: 0.5/s"));
    }

    #[test]
    fn offenders_unavailable_says_root_needed() {
        let off = offenders_fixture(
            SourceStatus::err(SourceState::PermissionDenied, "attribution needs root (CAP_BPF)"),
            vec![],
        );
        let out = render_offenders(&off);
        assert!(out.contains("attribution unavailable"), "{out}");
        assert!(out.contains("root daemon"), "must point the user at the root daemon: {out}");
    }

    #[test]
    fn offenders_empty_but_available_explains_warmup() {
        let out = render_offenders(&offenders_fixture(SourceStatus::ok(), vec![]));
        assert!(out.contains("no wakeup offenders recorded"), "{out}");
    }
}
