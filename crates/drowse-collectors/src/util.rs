//! Shared fd plumbing and parsing. Everything here that runs per-tick
//! (`pread0`, `parse_u64`, `read_u64_at`) is allocation-free; the probe-time
//! helpers may allocate freely.

use drowse_api::{SourceState, SourceStatus};
use drowse_core::Probe;
use rustix::fd::OwnedFd;
use rustix::fs::{Mode, OFlags};
use std::io;
use std::path::Path;

/// Open a directory fd to hold for later `openat`.
pub fn open_dir(path: &Path) -> io::Result<OwnedFd> {
    Ok(rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::DIRECTORY,
        Mode::empty(),
    )?)
}

/// Open a file relative to a held directory fd, read-only.
pub fn open_at(dir: &OwnedFd, name: &str) -> io::Result<OwnedFd> {
    Ok(rustix::fs::openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}

/// `pread(fd, buf, 0)`: sysfs attributes are re-generated on every read from
/// offset 0, so a kept fd + pread never goes stale. Returns the filled slice.
pub fn pread0<'a>(fd: &OwnedFd, buf: &'a mut [u8]) -> io::Result<&'a [u8]> {
    let n = rustix::io::pread(fd, &mut *buf, 0)?;
    Ok(&buf[..n])
}

/// Hand-rolled ASCII u64 parse: skips leading whitespace, consumes digits,
/// ignores anything after the first non-digit. `None` on no digits or
/// overflow. No allocation, no UTF-8 validation.
pub fn parse_u64(b: &[u8]) -> Option<u64> {
    let mut i = 0;
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    let start = i;
    let mut v: u64 = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        v = v
            .checked_mul(10)?
            .checked_add(u64::from(b[i] - b'0'))?;
        i += 1;
    }
    if i == start { None } else { Some(v) }
}

/// Read + parse one sysfs u64 attribute from a kept fd. Allocation-free:
/// the parse-failure error is built from an `ErrorKind`, not a boxed message.
pub fn read_u64_at(fd: &OwnedFd, buf: &mut [u8]) -> io::Result<u64> {
    let s = pread0(fd, buf)?;
    parse_u64(s).ok_or_else(|| io::ErrorKind::InvalidData.into())
}

/// Classify a probe-time failure into the typed degradation the API promises:
/// EACCES → PermissionDenied, ENOENT → NotPresent, anything else → Error.
pub fn unavailable(name: &'static str, what: &str, err: &io::Error) -> Probe {
    let state = match err.kind() {
        io::ErrorKind::PermissionDenied => SourceState::PermissionDenied,
        io::ErrorKind::NotFound => SourceState::NotPresent,
        _ => SourceState::Error,
    };
    Probe::Unavailable {
        name,
        status: SourceStatus::err(state, format!("{what}: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_u64;

    #[test]
    fn parse_plain_and_newline() {
        assert_eq!(parse_u64(b"123\n"), Some(123));
        assert_eq!(parse_u64(b"0"), Some(0));
        assert_eq!(parse_u64(b"  42"), Some(42));
        assert_eq!(parse_u64(b"\t\n7\n"), Some(7));
    }

    #[test]
    fn parse_stops_at_first_non_digit() {
        assert_eq!(parse_u64(b"99 100"), Some(99));
        assert_eq!(parse_u64(b"55%\n"), Some(55));
    }

    #[test]
    fn parse_rejects_garbage_and_empty() {
        assert_eq!(parse_u64(b""), None);
        assert_eq!(parse_u64(b"\n"), None);
        assert_eq!(parse_u64(b"abc"), None);
        assert_eq!(parse_u64(b"-5"), None);
    }

    #[test]
    fn parse_bounds() {
        assert_eq!(parse_u64(b"18446744073709551615"), Some(u64::MAX));
        assert_eq!(parse_u64(b"18446744073709551616"), None); // overflow
    }
}
