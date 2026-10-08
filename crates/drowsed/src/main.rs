//! drowsed: the daemon. Single thread, one ppoll over {timerfd, listener,
//! clients}; newline-delimited JSON-RPC 2.0; zero timers armed while no
//! session exists — an idle daemon blocks indefinitely and causes no wakeups.
//!
//! TODO(timer slack): we'd like prctl(PR_SET_TIMERSLACK, 50ms) so our tick
//! coalesces with other system timers, but rustix 1.1 has no timer-slack
//! wrapper and we don't take a libc dep for one syscall. Revisit if rustix
//! grows it (rustix::process has other prctl wrappers, not this one).

mod rpc;
mod score;
mod state;
#[cfg(test)]
mod tests;

use drowse_api::{RpcRequest, RpcResponse, SourceStatus};
use drowse_core::service::{drive, Service};
use rustix::event::{PollFd, PollFlags};
use rustix::time::{
    timerfd_create, timerfd_settime, Itimerspec, TimerfdClockId, TimerfdFlags,
    TimerfdTimerFlags, Timespec,
};
use drowse_core::{Attributor, Collector, Engine, Probe};
use state::{boottime_ns, unix_ms, Policy, PolicyHandle, State};
use std::collections::BTreeMap;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Default slow-lane cadence: battery gauges change slowly, and each read
/// stalls on the EC, so it is read far less often than the fast tick.
const DEFAULT_SLOW_CADENCE_MS: u64 = 10_000;
/// Default attribution drain cadence: the eBPF maps accumulate in-kernel, so
/// one drain every 10 s is enough and keeps userspace/kernel crossings rare.
const DEFAULT_ATTR_CADENCE_MS: u64 = 10_000;

/// Notifications are dropped (never block the sampler) once a client's
/// outgoing buffer exceeds this.
const NOTIF_DROP_AT: usize = 256 * 1024;
/// A client whose buffered responses exceed this is disconnected.
const OUT_HARD_CAP: usize = 1024 * 1024;
/// Inbound line-buffer cap per client.
const IN_CAP: usize = 1024 * 1024;

struct Client {
    stream: UnixStream,
    inbuf: Vec<u8>,
    out: Vec<u8>,
    /// Tick frames dropped since the last delivered one.
    dropped: u64,
    dead: bool,
}

impl Client {
    fn queue(&mut self, line: String) {
        self.out.extend_from_slice(line.as_bytes());
        self.out.push(b'\n');
        if self.out.len() > OUT_HARD_CAP {
            self.dead = true;
        }
    }

    /// Notification path: drop instead of buffering past the cap.
    fn queue_notification(&mut self, line: String) {
        if self.out.len() > NOTIF_DROP_AT {
            self.dropped += 1;
            return;
        }
        self.dropped = 0;
        self.queue(line);
    }

    fn flush(&mut self) {
        while !self.out.is_empty() {
            match (&self.stream).write(&self.out) {
                Ok(0) => {
                    self.dead = true;
                    return;
                }
                Ok(n) => {
                    self.out.drain(..n);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.dead = true;
                    return;
                }
            }
        }
    }
}

/// Removes the socket file on clean shutdown.
struct SocketGuard(PathBuf);
impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn main() {
    let mut socket_path = drowse_api::user_socket_path();
    let mut root = PathBuf::from("/");
    let mut slow_cadence_ms = DEFAULT_SLOW_CADENCE_MS;
    let mut attr_cadence_ms = DEFAULT_ATTR_CADENCE_MS;
    let mut socket_mode: Option<u32> = None;
    let mut auto_apply = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" => match args.next() {
                Some(p) => socket_path = PathBuf::from(p),
                None => die_usage("--socket requires a path"),
            },
            "--root" => match args.next() {
                Some(p) => root = PathBuf::from(p),
                None => die_usage("--root requires a path"),
            },
            "--slow-cadence-ms" => match args.next().and_then(|s| s.parse::<u64>().ok()) {
                Some(n) if n >= 100 => slow_cadence_ms = n,
                _ => die_usage("--slow-cadence-ms requires an integer >= 100"),
            },
            "--attr-cadence-ms" => match args.next().and_then(|s| s.parse::<u64>().ok()) {
                Some(n) if n >= 100 => attr_cadence_ms = n,
                _ => die_usage("--attr-cadence-ms requires an integer >= 100"),
            },
            // Octal mode applied to the control socket after bind, so a system
            // daemon's socket is reachable by the user's bar/CLI (e.g. 0666).
            "--socket-mode" => match args.next() {
                Some(s) => match u32::from_str_radix(s.trim_start_matches("0o"), 8) {
                    Ok(m) => socket_mode = Some(m),
                    Err(_) => die_usage("--socket-mode requires an octal mode, e.g. 0666"),
                },
                None => die_usage("--socket-mode requires a mode"),
            },
            // Automatically apply the battery plan on unplug, revert on plug.
            // Off by default; a real mutation gate.
            "--auto-apply" => auto_apply = true,
            "--help" | "-h" => {
                println!(
                    "drowsed - drowse sampling daemon (JSON-RPC 2.0, newline-delimited)\n\n\
                     USAGE: drowsed [--socket PATH] [--root PATH] [--slow-cadence-ms N] [--attr-cadence-ms N]\n\n\
                     OPTIONS:\n  \
                     --socket PATH        control socket (default: $XDG_RUNTIME_DIR/drowsed.sock)\n  \
                     --root PATH          sysfs root, for fixture testing (default: /)\n  \
                     --slow-cadence-ms N  slow-lane (battery) sampling period (default: 10000)\n  \
                     --attr-cadence-ms N  eBPF attribution drain period (default: 10000)\n  \
                     --socket-mode OCTAL  chmod the socket after bind (e.g. 0666 for a system daemon)\n  \
                     --auto-apply         apply the battery plan on unplug, revert on plug (default off)"
                );
                return;
            }
            other => die_usage(&format!("unknown argument: {other}")),
        }
    }

    if let Err(e) = run(&socket_path, &root, slow_cadence_ms, attr_cadence_ms, socket_mode, auto_apply) {
        eprintln!("drowsed: fatal: {e}");
        std::process::exit(drowse_api::ErrorCode::Internal.exit_code());
    }
}

fn die_usage(msg: &str) -> ! {
    eprintln!("drowsed: {msg} (see --help)");
    std::process::exit(drowse_api::ErrorCode::Usage.exit_code());
}

fn sysinfo() -> score::SysInfo {
    let read = |p: &str| {
        std::fs::read_to_string(p)
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "unknown".into())
    };
    score::SysInfo {
        hostname: read("/proc/sys/kernel/hostname"),
        kernel: read("/proc/sys/kernel/osrelease"),
    }
}

/// Latency classifier. High-latency, hardware-waking collectors (the battery
/// EC gauges) go to the slow lane so the fast tick never blocks. The class is
/// the collector's own declaration — no name knowledge here.
fn is_slow(c: &dyn Collector) -> bool {
    c.latency() == drowse_core::Latency::Slow
}

/// Split probes into (fast, slow). Unavailable probes stay on the fast side so
/// their typed status still surfaces in the sources map; only Ready collectors
/// classified Slow move to the slow lane.
fn partition_probes(probes: Vec<Probe>) -> (Vec<Probe>, Vec<Probe>) {
    let mut fast = Vec::new();
    let mut slow = Vec::new();
    for p in probes {
        match &p {
            Probe::Ready(c) if is_slow(c.as_ref()) => slow.push(p),
            _ => fast.push(p),
        }
    }
    (fast, slow)
}

#[allow(clippy::too_many_arguments)]
fn run(
    socket_path: &Path,
    root: &Path,
    slow_cadence_ms: u64,
    attr_cadence_ms: u64,
    socket_mode: Option<u32>,
    auto_apply: bool,
) -> std::io::Result<()> {
    // Refuse to steal a live socket; replace a stale one.
    if socket_path.exists() {
        if UnixStream::connect(socket_path).is_ok() {
            eprintln!("drowsed: already running at {}", socket_path.display());
            std::process::exit(drowse_api::ErrorCode::Internal.exit_code());
        }
        std::fs::remove_file(socket_path)?;
    }

    let (fast_probes, slow_probes) = partition_probes(drowse_collectors::probe_all(root));
    let has_slow = slow_probes.iter().any(|p| matches!(p, Probe::Ready(_)));
    let fast_engine = Engine::new(fast_probes);
    let slow_engine = Engine::new(slow_probes);
    let slow_layout = score::layout_of(&slow_engine);

    // Attribution (eBPF): Ready needs root; Unavailable unprivileged is a typed
    // value, carried through to offenders.get as its source — never an error.
    let (attr_source, attributor) = match drowse_ebpf::probe() {
        drowse_core::AttributorProbe::Ready(a) => (SourceStatus::ok(), Some(a)),
        drowse_core::AttributorProbe::Unavailable(status) => (status, None),
    };
    eprintln!("drowsed: attribution {:?}", attr_source.state);

    // Policy (M3): the act layer. Always Ready — plan works unprivileged and
    // apply reports per-action PermissionDenied. Held behind its OWN mutex so a
    // slow apply() never blocks the sampler lock.
    let policy = Arc::new(Mutex::new(PolicyHandle::new(match drowse_policy::probe() {
        drowse_core::PolicyProbe::Ready(engine) => Policy::Ready(engine),
        drowse_core::PolicyProbe::Unavailable(status) => {
            eprintln!("drowsed: policy {:?}", status.state);
            Policy::Unavailable(status)
        }
    })));

    let st = Arc::new(Mutex::new(State::new(
        fast_engine,
        slow_layout,
        slow_cadence_ms,
        attr_source,
        sysinfo(),
    )));
    let mut svc = rpc::service(st.clone(), policy.clone());

    // Auto-apply watcher (opt-in): on AC->battery apply the battery plan and
    // record the txn; on battery->AC revert it. Off unless --auto-apply.
    if auto_apply {
        spawn_auto_apply(st.clone(), policy.clone());
    }

    // Both lanes park on this until a session exists, so an idle daemon wakes
    // neither the fast (ppoll) thread nor either background thread.
    let wake = Arc::new(Condvar::new());
    if has_slow {
        spawn_slow_lane(st.clone(), wake.clone(), slow_engine, slow_cadence_ms);
    }
    if let Some(attributor) = attributor {
        spawn_attribution_lane(st.clone(), wake.clone(), attributor, attr_cadence_ms);
    }
    let slow_wake = wake;

    let listener = UnixListener::bind(socket_path)?;
    listener.set_nonblocking(true)?;
    if let Some(mode) = socket_mode {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(mode))?;
    }
    let _guard = SocketGuard(socket_path.to_path_buf());

    // One timerfd for the daemon's lifetime; disarmed = never fires.
    let tfd = timerfd_create(
        TimerfdClockId::Boottime,
        TimerfdFlags::CLOEXEC | TimerfdFlags::NONBLOCK,
    )?;

    {
        let st = st.lock().unwrap();
        let sources = st.sources_now();
        eprintln!(
            "drowsed: listening on {} ({} sources: {})",
            socket_path.display(),
            sources.len(),
            sources
                .iter()
                .map(|(k, v)| format!("{k}={:?}", v.state))
                .collect::<Vec<_>>()
                .join(", "),
        );
    }

    let mut clients: BTreeMap<u64, Client> = BTreeMap::new();
    let mut next_conn: u64 = 0;
    let mut armed_ms: Option<u64> = None;

    loop {
        rearm_if_needed(&tfd, &st, &mut armed_ms)?;

        // Build the poll set. PollFds borrow the clients map immutably; all
        // mutation happens after revents are copied out.
        let ids: Vec<u64> = clients.keys().copied().collect();
        let mut fds: Vec<PollFd> = Vec::with_capacity(2 + ids.len());
        fds.push(PollFd::new(&tfd, PollFlags::IN));
        fds.push(PollFd::new(&listener, PollFlags::IN));
        for id in &ids {
            let c = &clients[id];
            let mut flags = PollFlags::IN;
            if !c.out.is_empty() {
                flags |= PollFlags::OUT;
            }
            fds.push(PollFd::new(&c.stream, flags));
        }

        // Block indefinitely: with no session there is no timer and the
        // daemon takes zero wakeups until a client shows up.
        match rustix::event::poll(&mut fds, None) {
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(e.into()),
        }
        let revents: Vec<PollFlags> = fds.iter().map(|f| f.revents()).collect();
        drop(fds);

        // 1. Timer tick.
        if revents[0].intersects(PollFlags::IN) {
            let mut buf = [0u8; 8];
            let _ = rustix::io::read(&tfd, &mut buf); // expiry count; unused
            on_tick(&st, &mut clients);
        }

        // 2. New connections.
        if revents[1].intersects(PollFlags::IN) {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(true)?;
                        next_conn += 1;
                        clients.insert(
                            next_conn,
                            Client {
                                stream,
                                inbuf: Vec::new(),
                                out: Vec::new(),
                                dropped: 0,
                                dead: false,
                            },
                        );
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) => {
                        eprintln!("drowsed: accept: {e}");
                        break;
                    }
                }
            }
        }

        // 3. Client I/O.
        for (idx, id) in ids.iter().enumerate() {
            let ev = revents[idx + 2];
            let Some(c) = clients.get_mut(id) else { continue };
            if ev.intersects(PollFlags::HUP | PollFlags::ERR) {
                c.dead = true;
            }
            if ev.intersects(PollFlags::OUT) {
                c.flush();
            }
            if ev.intersects(PollFlags::IN) && !c.dead {
                read_client(&mut svc, &st, *id, c);
            }
        }

        // 4. Reap the dead.
        let dead: Vec<u64> = clients.iter().filter(|(_, c)| c.dead).map(|(id, _)| *id).collect();
        for id in dead {
            clients.remove(&id);
            st.lock().unwrap().subscribers.remove(&id);
        }

        // 5. A request may have started the first session; wake the slow lane
        // out of its idle park. (No-op while it is already running.)
        if st.lock().unwrap().min_interval_ms().is_some() {
            slow_wake.notify_all();
        }
    }
}

/// Read whether the machine is on battery, directly from sysfs (no sampler
/// lock). None when no power supply reports `online`.
fn on_battery_now(st: &Arc<Mutex<State>>) -> Option<bool> {
    let base = std::path::Path::new("/sys/class/power_supply");
    let mut fallback: Option<bool> = None;
    if let Ok(dir) = std::fs::read_dir(base) {
        for entry in dir.flatten() {
            let p = entry.path();
            let is_mains = std::fs::read_to_string(p.join("type"))
                .map(|t| t.trim() == "Mains")
                .unwrap_or(false);
            if let Ok(s) = std::fs::read_to_string(p.join("online")) {
                let online = s.trim() == "1";
                if is_mains {
                    return Some(!online);
                }
                fallback.get_or_insert(!online);
            }
        }
    }
    fallback.or_else(|| st.lock().unwrap().battery_discharging())
}

/// The auto-apply watcher (opt-in via `--auto-apply`): a low-rate thread that
/// applies the battery power-save plan on unplug and reverts it on plug. It
/// holds only the policy mutex, never the sampler lock, so a slow apply can't
/// dent the self-footprint budget. Default off — a real mutation gate.
fn spawn_auto_apply(st: Arc<Mutex<State>>, policy: Arc<Mutex<PolicyHandle>>) {
    use drowse_core::PowerState;
    std::thread::spawn(move || {
        let mut last: Option<bool> = None;
        loop {
            std::thread::sleep(Duration::from_secs(5));
            let Some(on_batt) = on_battery_now(&st) else { continue };
            if Some(on_batt) == last {
                continue;
            }
            last = Some(on_batt);

            let mut g = policy.lock().unwrap();
            let active = g.active_txn.clone();
            // None = leave active_txn as-is; Some(x) = set it to x.
            let next: Option<Option<String>> = match &mut g.engine {
                Policy::Ready(engine) => {
                    if on_batt && active.is_none() {
                        match engine.apply(PowerState::Battery, false) {
                            Ok(res) if !res.dry_run => {
                                eprintln!("drowsed: auto-applied battery plan ({})", res.txn_id);
                                Some(Some(res.txn_id))
                            }
                            Ok(_) => None,
                            Err(e) => {
                                eprintln!("drowsed: auto-apply failed: {e}");
                                None
                            }
                        }
                    } else if !on_batt {
                        if let Some(txn) = &active {
                            let _ = engine.revert(txn);
                            eprintln!("drowsed: auto-reverted on AC ({txn})");
                            Some(None)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                Policy::Unavailable(_) => None,
            };
            if let Some(v) = next {
                g.active_txn = v;
            }
        }
    });
}

/// The slow lane: a dedicated thread owning the high-latency collectors. It
/// reads them on its own cadence and publishes the latest values into shared
/// `State`, so the ppoll sampler thread never blocks on an EC/SMU stall. Its
/// own wakeups are rare (~1 per cadence) and accrue to this thread, not the
/// fast one. It parks entirely while no session exists.
fn spawn_slow_lane(
    st: Arc<Mutex<State>>,
    wake: Arc<Condvar>,
    mut engine: Engine,
    cadence_ms: u64,
) {
    let publish = move |engine: &mut Engine, st: &Arc<Mutex<State>>| {
        // Sample OFF the lock: the EC stall happens here, on this thread only.
        let t = Instant::now();
        if let Err(e) = engine.tick(boottime_ns()) {
            eprintln!("drowsed: slow-lane tick failed: {e}");
        }
        let values: Vec<Option<u64>> = engine.values().map(|s| s.value).collect();
        eprintln!("drowsed: slow-lane sampled in {}us", t.elapsed().as_micros());
        st.lock().unwrap().publish_slow(values, unix_ms());
    };

    std::thread::Builder::new()
        .name("drowse-slow".into())
        .spawn(move || {
            // Prime once at startup so battery is available to a no-session
            // one-shot `score.get` before any session exists.
            publish(&mut engine, &st);
            loop {
                // Park (zero wakeups) until at least one session is running.
                {
                    let guard = st.lock().unwrap();
                    let _g = wake
                        .wait_while(guard, |s| s.sessions.is_empty())
                        .unwrap();
                }
                // Active: sample every cadence until the last session stops.
                loop {
                    publish(&mut engine, &st);
                    std::thread::sleep(Duration::from_millis(cadence_ms));
                    if st.lock().unwrap().sessions.is_empty() {
                        break;
                    }
                }
            }
        })
        .expect("spawn slow lane thread");
}

/// The attribution lane: a dedicated thread owning the eBPF attributor. It
/// drains the in-kernel counter maps on its own cadence and publishes each
/// window into shared `State`, so `offenders.get` never blocks on the drain.
/// Like the slow lane it parks while sessionless. `drain` is delete-on-read,
/// so each published window covers exactly the wall time since the prior
/// drain; counts accumulated while parked are discarded on unpark, so a
/// window is always a clean cadence-width measurement.
fn spawn_attribution_lane(
    st: Arc<Mutex<State>>,
    wake: Arc<Condvar>,
    mut attributor: Box<dyn Attributor>,
    cadence_ms: u64,
) {
    std::thread::Builder::new()
        .name("drowse-attr".into())
        .spawn(move || {
            // Zero the kernel counters at startup so the first real window does
            // not fold in everything since program load.
            let _ = attributor.drain();
            loop {
                // Park (zero wakeups) until at least one session is running.
                {
                    let guard = st.lock().unwrap();
                    let _g = wake
                        .wait_while(guard, |s| s.sessions.is_empty())
                        .unwrap();
                }
                // Discard counts accumulated while parked; start a clean window.
                let _ = attributor.drain();
                let mut prev_ns = boottime_ns();
                loop {
                    std::thread::sleep(Duration::from_millis(cadence_ms));
                    let now_ns = boottime_ns();
                    match attributor.drain() {
                        Ok(delta) => {
                            let window_ns = now_ns.saturating_sub(prev_ns);
                            prev_ns = now_ns;
                            let n = delta.offenders.len();
                            st.lock().unwrap().publish_attribution(delta, window_ns, unix_ms());
                            eprintln!("drowsed: attribution drained {n} offenders");
                        }
                        Err(e) => eprintln!("drowsed: attribution drain failed: {e}"),
                    }
                    if st.lock().unwrap().sessions.is_empty() {
                        break;
                    }
                }
            }
        })
        .expect("spawn attribution lane thread");
}

/// Pure timer policy: what the timerfd should be set to for a desired
/// cadence. `None` = disarm (idle daemon takes zero timer wakeups). For
/// `Some`, returns `(first_expiry_ns, interval_ns)` with the first expiry
/// aligned to an interval multiple of CLOCK_BOOTTIME, so our wakeups coalesce
/// with other interval-aligned timers instead of free-running at a phase
/// offset. Unit-tested: sessions→0 MUST disarm.
fn timer_plan(desired_ms: Option<u64>, now_ns: u64) -> Option<(u64, u64)> {
    let iv_ns = desired_ms? * 1_000_000;
    Some(((now_ns / iv_ns + 1) * iv_ns, iv_ns))
}

fn ts(ns: u64) -> Timespec {
    Timespec { tv_sec: (ns / 1_000_000_000) as i64, tv_nsec: (ns % 1_000_000_000) as i64 }
}

/// Apply [`timer_plan`] to the real timerfd when the desired cadence changed.
fn rearm_if_needed(
    tfd: &impl std::os::fd::AsFd,
    st: &Arc<Mutex<State>>,
    armed_ms: &mut Option<u64>,
) -> std::io::Result<()> {
    let desired = st.lock().unwrap().min_interval_ms();
    if desired == *armed_ms {
        return Ok(());
    }
    match timer_plan(desired, boottime_ns()) {
        None => {
            timerfd_settime(
                tfd,
                TimerfdTimerFlags::empty(),
                &Itimerspec { it_interval: ts(0), it_value: ts(0) },
            )?;
        }
        Some((next_ns, iv_ns)) => {
            // Prime the engine so the first timer expiry yields a delta.
            {
                let mut s = st.lock().unwrap();
                if s.engine.ticks() == 0
                    && let Err(e) = s.tick_now(boottime_ns(), unix_ms())
                {
                    eprintln!("drowsed: prime tick failed: {e}");
                }
            }
            timerfd_settime(
                tfd,
                TimerfdTimerFlags::ABSTIME,
                &Itimerspec { it_interval: ts(iv_ns), it_value: ts(next_ns) },
            )?;
        }
    }
    *armed_ms = desired;
    Ok(())
}

/// Timer expiry: sample, record, fan out one notification frame per
/// subscriber. Slow subscribers get frames dropped, never a blocked sampler.
fn on_tick(st: &Arc<Mutex<State>>, clients: &mut BTreeMap<u64, Client>) {
    let (params, subs) = {
        let mut s = st.lock().unwrap();
        // Time the sampling itself so hot-path regressions are visible.
        let t = std::time::Instant::now();
        let ticked = s.tick_now(boottime_ns(), unix_ms());
        eprintln!("drowsed: tick sampled in {}us", t.elapsed().as_micros());
        match ticked {
            Ok(true) => {}
            Ok(false) => return, // warmup tick, no delta yet
            Err(e) => {
                eprintln!("drowsed: tick failed: {e}");
                return;
            }
        }
        let subs: Vec<u64> = s.subscribers.keys().copied().collect();
        if subs.is_empty() {
            return;
        }
        match s.make_tick_params() {
            Some(p) => (p, subs),
            None => return,
        }
    };
    for id in subs {
        let Some(c) = clients.get_mut(&id) else { continue };
        let mut p = params.clone();
        if c.dropped > 0 {
            p["dropped"] = serde_json::json!(c.dropped);
        }
        let frame =
            serde_json::json!({ "jsonrpc": "2.0", "method": "tick", "params": p }).to_string();
        c.queue_notification(frame);
        c.flush();
    }
}

fn read_client<S>(svc: &mut S, st: &Arc<Mutex<State>>, conn_id: u64, c: &mut Client)
where
    S: Service<RpcRequest, Response = RpcResponse, Error = std::convert::Infallible>,
{
    let mut buf = [0u8; 4096];
    loop {
        match (&c.stream).read(&mut buf) {
            Ok(0) => {
                c.dead = true;
                break;
            }
            Ok(n) => {
                c.inbuf.extend_from_slice(&buf[..n]);
                if c.inbuf.len() > IN_CAP {
                    c.dead = true;
                    return;
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => {
                c.dead = true;
                return;
            }
        }
    }
    // Process complete lines (even on a half-closed connection).
    while let Some(pos) = c.inbuf.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = c.inbuf.drain(..=pos).collect();
        let line = &line[..line.len() - 1];
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        if let Some(resp) = handle_line(svc, st, conn_id, line) {
            c.queue(resp);
        }
    }
    c.flush();
}

/// Parse one frame, dispatch through the service stack. Returns the response
/// line for requests (id present); notifications get none per JSON-RPC 2.0.
fn handle_line<S>(
    svc: &mut S,
    st: &Arc<Mutex<State>>,
    conn_id: u64,
    line: &[u8],
) -> Option<String>
where
    S: Service<RpcRequest, Response = RpcResponse, Error = std::convert::Infallible>,
{
    let req: RpcRequest = match serde_json::from_slice(line) {
        Ok(r) => r,
        Err(e) => {
            let resp = RpcResponse::err(None, -32700, format!("parse error: {e}"), None);
            return Some(serde_json::to_string(&resp).unwrap());
        }
    };
    let has_id = req.id.is_some();
    st.lock().unwrap().current_conn = Some(conn_id);
    let resp = drive(svc, req);
    st.lock().unwrap().current_conn = None;
    has_id.then(|| serde_json::to_string(&resp).unwrap())
}
