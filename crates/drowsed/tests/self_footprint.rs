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
//! This exercises the REAL collector stack on the host. The ppoll sampler
//! thread reads only the cheap collectors (cpuidle/amdgpu/rapl); the EC-bound
//! battery collector runs on the slow-lane thread, whose context switches
//! accrue to a different TID and so do not appear in /proc/<pid>/status (that
//! path reports the main thread's task counters, verified). The test also
//! asserts battery stays fresh, so the slow lane cannot silently freeze.

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

    // Slow lane cadence 1 s: battery is read on its own thread (off the ppoll
    // sampler), so this does NOT count against the fast-thread budget measured
    // below, and it makes battery freshness observable inside the test window.
    let child = Command::new(env!("CARGO_BIN_EXE_drowsed"))
        .args([
            "--socket",
            sock.to_str().unwrap(),
            "--slow-cadence-ms",
            "1000",
        ])
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
    let stream = stream.expect("daemon socket did not appear");
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut w = stream;

    let mut next_id = 0u64;
    let mut call = |w: &mut UnixStream, method: &str, params: &str| -> String {
        next_id += 1;
        let frame = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{next_id},\"method\":\"{method}\",\"params\":{params}}}\n"
        );
        w.write_all(frame.as_bytes()).unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        line
    };

    // One 2 s session; the slow lane starts sampling battery every 1 s.
    let started = call(&mut w, "session.start", "{\"session_id\":\"fp\",\"interval_ms\":2000}");
    assert!(started.contains("\"created\":true"), "session.start reply: {started}");

    // Settle past the prime tick and rearm churn.
    std::thread::sleep(Duration::from_secs(3));

    // Freshness, reading 1: battery must be present and its source `ok`
    // (published within the staleness window = 3x cadence).
    let sc1 = call(&mut w, "score.get", "{\"window_s\":10}");
    assert!(
        sc1.contains("\"battery\":{\"state\":\"ok\"}"),
        "battery source not fresh at t=3s: {sc1}"
    );
    assert!(sc1.contains("\"power_w\":"), "battery value missing at t=3s: {sc1}");

    // ---- fast-thread wakeup budget over 10 ticks --------------------------
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

    // Freshness, reading 2 (~20 s later): still `ok` proves the slow lane kept
    // publishing — it did not silently freeze after the first sample.
    let sc2 = call(&mut w, "score.get", "{\"window_s\":10}");
    assert!(
        sc2.contains("\"battery\":{\"state\":\"ok\"}"),
        "battery source went stale — slow lane froze: {sc2}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
