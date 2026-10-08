//! The act layer (M3).
//!
//! A [`drowse_core::PolicyEngine`] that plans, applies, and reverts reversible
//! system changes: tiered sysfs knobs (ASPM, USB autosuspend, EPP/platform
//! profile, audio power_save), and elastic-service cgroup caps via systemd.
//! Every applied change is written to a journal BEFORE the write (write-ahead),
//! so a crash or `revert` restores the prior state. Default posture is safe:
//! only T0/T1 knobs apply without opt-in, apply is gated, and the CLI defaults
//! to dry-run. Verify-after-apply watches for regressions and auto-reverts.
//!
//! STATUS: stub. `probe` reports Unavailable until the engine lands, so the
//! daemon builds and policy.* verbs degrade honestly.

use drowse_api::{SourceState, SourceStatus};
use drowse_core::PolicyProbe;

/// Probe the policy engine. Plan works unprivileged; apply needs privilege.
pub fn probe() -> PolicyProbe {
    PolicyProbe::Unavailable(SourceStatus::err(
        SourceState::NotPresent,
        "policy engine not implemented yet (M3)",
    ))
}
