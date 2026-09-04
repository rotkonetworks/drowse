//! Live self-footprint check: drowsed must not itself hurt the battery.
//!
//! Runs the real daemon with one 2 s session for ~20 s and asserts its own
//! wakeup rate is <= 1.1 per tick, measured as voluntary_ctxt_switches
//! deltas from /proc/<pid>/status (a wakeup ends in a voluntary switch when
//! the daemon blocks again; no perf needed).
//!
//! Ignored by default (needs ~25 s of wall time and a quiet scheduler):
//!   cargo test -p drowsed --test self_footprint -- --ignored
//!
//! This exercises the REAL collector stack on the host, so a failure indicts
//! whichever collector blocks the sampling thread — not just the transport.
//! To isolate the transport, run against a synthetic fixture root with only
//! cpuidle (`drowsed --root <fixture>`); that path measures ~1 switch/tick.
//! As of this writing the live check fails on EC-bound laptops because the
//! battery collector's `power_now` pread wakes the embedded controller (~4.5
//! ms, ~25 voluntary switches) every tick — see the report. The fix belongs
//! in drowse-collectors / the engine (sample slow battery gauges at a
//! sub-cadence), not here.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

fn voluntary_ctxt_switches(pid: u32) -> u64 {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).expect("read status");
    status
        .lines()
        .find_map(|l| l.strip_prefix("voluntary_ctxt_switches:"))
        .expect("voluntary_ctxt_switches in /proc/pid/status")
        .trim()
        .parse()
        .expect("parse switch count")
}

struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "live 25s wall-clock test; run with -- --ignored"]
fn wakeup_rate_at_most_1_1_per_tick() {
    let dir = std::env::temp_dir().join(format!("drowsed-footprint-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("sock");

    let child = Command::new(env!("CARGO_BIN_EXE_drowsed"))
        .args(["--socket", sock.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(Stdio::null()) // per-tick log must not block on a full pipe
        .spawn()
        .expect("spawn drowsed");
    let child = KillOnDrop(child);
    let pid = child.0.id();

    // Wait for the socket.
    let mut stream = None;
    for _ in 0..50 {
        if let Ok(s) = UnixStream::connect(&sock) {
            stream = Some(s);
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let mut stream = stream.expect("daemon socket did not appear");

    // One 2 s session; then leave the connection completely idle.
    stream
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session.start\",\
              \"params\":{\"session_id\":\"fp\",\"interval_ms\":2000}}\n",
        )
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.contains("\"created\":true"), "session.start reply: {line}");

    // Settle past the prime tick and rearm churn, then measure 10 ticks.
    std::thread::sleep(Duration::from_secs(3));
    let v1 = voluntary_ctxt_switches(pid);
    const TICKS: u64 = 10;
    std::thread::sleep(Duration::from_secs(2 * TICKS));
    let v2 = voluntary_ctxt_switches(pid);

    let wakeups = v2 - v1;
    let rate = wakeups as f64 / TICKS as f64;
    eprintln!("drowsed footprint: {wakeups} voluntary switches over {TICKS} ticks = {rate:.2}/tick");
    assert!(
        rate <= 1.1,
        "drowsed woke {rate:.2} times per tick (limit 1.1) — the daemon is hurting the battery"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
