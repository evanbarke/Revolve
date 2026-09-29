//! Small shared helpers: error type, clock, identifiers, fingerprints, memory.

use sha2::{Digest, Sha256};
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

/// Project-wide error: a message with enough context to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub msg: String,
}

impl Error {
    pub fn new(msg: impl Into<String>) -> Self {
        Error { msg: msg.into() }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for Error {}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::new(format!("sqlite: {e}"))
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::new(format!("json: {e}"))
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::new(format!("io: {e}"))
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[macro_export]
macro_rules! bail {
    ($($arg:tt)*) => { return Err($crate::util::Error::new(format!($($arg)*))) };
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Hex SHA-256 of a byte string.
pub fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut out = String::with_capacity(64);
    for b in digest.iter() {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Canonical JSON text: serde_json maps are BTreeMap-ordered (no `preserve_order`
/// feature), so serialising a `Value` yields sorted keys and a stable fingerprint.
pub fn canonical_json(v: &serde_json::Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

pub fn fingerprint(v: &serde_json::Value) -> String {
    sha256_hex(canonical_json(v).as_bytes())
}

/// A fresh, sortable, unique-enough identifier for local runs: time + process + counter.
pub fn fresh_id(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let t = now_ms();
    let h = sha256_hex(format!("{t}:{}:{n}", std::process::id()).as_bytes());
    format!("{prefix}-{t}-{}", &h[..6])
}

/// Peak resident memory of the current process in bytes, where the platform exposes it.
pub fn peak_rss_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmHWM:") {
                let kb: u64 = rest.trim().trim_end_matches("kB").trim().parse().ok()?;
                return Some(kb * 1024);
            }
        }
        None
    }
    #[cfg(windows)]
    {
        win_mem::peak_working_set()
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        None
    }
}

#[cfg(windows)]
mod win_mem {
    // Minimal binding to K32GetProcessMemoryInfo (kernel32, Windows 7+).
    #[repr(C)]
    #[allow(non_snake_case)]
    struct ProcessMemoryCounters {
        cb: u32,
        PageFaultCount: u32,
        PeakWorkingSetSize: usize,
        WorkingSetSize: usize,
        QuotaPeakPagedPoolUsage: usize,
        QuotaPagedPoolUsage: usize,
        QuotaPeakNonPagedPoolUsage: usize,
        QuotaNonPagedPoolUsage: usize,
        PagefileUsage: usize,
        PeakPagefileUsage: usize,
    }
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn K32GetProcessMemoryInfo(
            process: isize,
            counters: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
    }
    pub fn peak_working_set() -> Option<u64> {
        let mut c = ProcessMemoryCounters {
            cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
            PageFaultCount: 0,
            PeakWorkingSetSize: 0,
            WorkingSetSize: 0,
            QuotaPeakPagedPoolUsage: 0,
            QuotaPagedPoolUsage: 0,
            QuotaPeakNonPagedPoolUsage: 0,
            QuotaNonPagedPoolUsage: 0,
            PagefileUsage: 0,
            PeakPagefileUsage: 0,
        };
        // SAFETY: pseudo-handle from GetCurrentProcess is always valid; struct is sized per cb.
        let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
        if ok != 0 {
            Some(c.PeakWorkingSetSize as u64)
        } else {
            None
        }
    }
}

/// Exit code used by the simulator's crash fault. Documented so the harness can
/// distinguish an injected crash from a genuine failure.
pub const SIMULATED_CRASH_EXIT_CODE: i32 = 86;
