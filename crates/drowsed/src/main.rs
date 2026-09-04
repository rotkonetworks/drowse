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

use drowse_api::{RpcRequest, RpcResponse};
use drowse_core::service::{drive, Service};
use rustix::event::{PollFd, PollFlags};
use rustix::time::{
    timerfd_create, timerfd_settime, Itimerspec, TimerfdClockId, TimerfdFlags,
    TimerfdTimerFlags, Timespec,
};
use state::{boottime_ns, unix_ms, State};
use std::collections::BTreeMap;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

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
            "--help" | "-h" => {
                println!(
                    "drowsed - drowse sampling daemon (JSON-RPC 2.0, newline-delimited)\n\n\
                     USAGE: drowsed [--socket PATH] [--root PATH]\n\n\
                     OPTIONS:\n  \
                     --socket PATH  control socket (default: $XDG_RUNTIME_DIR/drowsed.sock)\n  \
                     --root PATH    sysfs root, for fixture testing (default: /)"
                );
                return;
            }
            other => die_usage(&format!("unknown argument: {other}")),
        }
    }

    if let Err(e) = run(&socket_path, &root) {
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

fn run(socket_path: &Path, root: &Path) -> std::io::Result<()> {
    // Refuse to steal a live socket; replace a stale one.
    if socket_path.exists() {
        if UnixStream::connect(socket_path).is_ok() {
            eprintln!("drowsed: already running at {}", socket_path.display());
            std::process::exit(drowse_api::ErrorCode::Internal.exit_code());
        }
        std::fs::remove_file(socket_path)?;
    }

    let engine = drowse_core::Engine::new(drowse_collectors::probe_all(root));
    let st = Arc::new(Mutex::new(State::new(engine, sysinfo())));
    let mut svc = rpc::service(st.clone());

    let listener = UnixListener::bind(socket_path)?;
    listener.set_nonblocking(true)?;
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
    }
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
                if s.engine.ticks() == 0 {
                    if let Err(e) = s.tick_now(boottime_ns(), unix_ms()) {
                        eprintln!("drowsed: prime tick failed: {e}");
                    }
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
