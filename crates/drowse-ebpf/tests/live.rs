//! Live load/attach/drain test. Requires root (CAP_BPF) + a BTF kernel, so it is
//! `#[ignore]`d and never runs under `cargo test`. Run it explicitly as root:
//!
//!     cargo test -p drowse-ebpf --test live -- --ignored --nocapture
//!
//! It loads the programs, generates wakeups (sleeping threads => timers +
//! sched_waking), drains, and asserts the attribution table is plausible.

use std::thread;
use std::time::{Duration, Instant};

use drowse_core::AttributorProbe;

#[test]
#[ignore = "needs root + BTF; run with --ignored under sudo"]
fn live_load_attach_drain() {
    let mut attr = match drowse_ebpf::probe() {
        AttributorProbe::Ready(a) => a,
        AttributorProbe::Unavailable(s) => {
            panic!("probe returned Unavailable ({:?}) — run as root on a BTF kernel", s);
        }
    };

    // First drain clears whatever accumulated during attach.
    let _ = attr.drain().expect("initial drain");

    // Generate wakeups: worker threads that repeatedly sleep (arming timers and
    // waking each other / the main thread), plus timer churn on this thread.
    let busy = Duration::from_millis(1200);
    let start = Instant::now();
    let mut handles = Vec::new();
    for _ in 0..4 {
        handles.push(thread::spawn(move || {
            while start.elapsed() < busy {
                thread::sleep(Duration::from_millis(5));
            }
        }));
    }
    while start.elapsed() < busy {
        thread::sleep(Duration::from_millis(10));
    }
    for h in handles {
        h.join().unwrap();
    }

    let t0 = Instant::now();
    let delta = attr.drain().expect("drain after activity");
    let drain_cost = t0.elapsed();

    eprintln!(
        "drain: {} offenders, observer_wakeups={}, cost={:?}",
        delta.offenders.len(),
        delta.observer_wakeups,
        drain_cost
    );

    let mut sorted: Vec<_> = delta.offenders.iter().collect();
    sorted.sort_unstable_by_key(|o| std::cmp::Reverse(o.wakeups));
    for o in sorted.iter().take(20) {
        eprintln!(
            "  {:?} key={} name={} pid={:?} wakeups={} idle_breaks={} pkg_breaks={}",
            o.kind, o.key, o.name, o.pid, o.wakeups, o.idle_breaks, o.pkg_breaks
        );
    }

    // Plausibility.
    assert!(!delta.offenders.is_empty(), "expected some offenders");
    let total: u64 = delta.offenders.iter().map(|o| o.wakeups).sum();
    assert!(total > 0, "expected total wakeups > 0");
    for o in &delta.offenders {
        assert!(o.wakeups > 0, "offender {} has zero wakeups", o.name);
        assert!(
            o.idle_breaks <= o.wakeups,
            "idle_breaks {} > wakeups {} for {}",
            o.idle_breaks,
            o.wakeups,
            o.name
        );
        assert!(
            o.pkg_breaks <= o.idle_breaks,
            "pkg_breaks {} > idle_breaks {} for {}",
            o.pkg_breaks,
            o.idle_breaks,
            o.name
        );
    }
    // Some idle CPU should have been woken over 1.2s on a laptop.
    let idle: u64 = delta.offenders.iter().map(|o| o.idle_breaks).sum();
    eprintln!("total wakeups={total}, idle_breaks={idle}");
    assert!(idle > 0, "expected at least one idle-break wakeup");

    // The test process itself should appear as the observer, not as an offender.
    let self_pid = std::process::id();
    assert!(
        !delta.offenders.iter().any(|o| o.pid == Some(self_pid)),
        "daemon's own tgid leaked into the offender table"
    );
}
