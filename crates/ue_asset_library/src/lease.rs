//! Claim leases: owner identity, heartbeat and automatic reclaim of stale claims.
//!
//! The claim file `processing/<first volume>.claim` holds the owner (host, pid, process start
//! time, run id) and a lease expiry that the owner pushes forward from a heartbeat thread
//! while a job runs. A claim is reclaimed when its lease (plus a clock-skew grace margin)
//! has expired, or when the owner is provably dead (same host, pid gone or reused). A live
//! owner on the same host is never reclaimed. The takeover is an exclusive-create lock file
//! (`create_new`; renames are not exclusive on Windows), so exactly one sanitizer wins; the
//! winner moves every volume back to the inbox, or to `failed/` with `STALE_RETRIES_EXCEEDED`
//! after too many reclaims.

use crate::pipeline::{siblings_in, Config, Dirs};
use crate::util::{hostname, move_unique, rfc3339_utc, unique_token, unix_secs};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

/// Lease timing shared by claim creation, heartbeat and reclaim.
#[derive(Clone, Copy, Debug)]
pub struct LeaseParams {
    /// A claim is valid this long after the owner's last heartbeat.
    pub lease_secs: u64,
    /// The owner refreshes the lease this often (well under `lease_secs`).
    pub heartbeat_secs: u64,
    /// Clock-skew / SMB caution added to the expiry before anyone may reclaim.
    pub grace_secs: u64,
}

impl Default for LeaseParams {
    fn default() -> Self {
        LeaseParams {
            lease_secs: 300,
            heartbeat_secs: 60,
            grace_secs: 150,
        }
    }
}

/// Default clock-skew margin for a lease length.
pub fn default_grace(lease_secs: u64) -> u64 {
    (lease_secs / 2).max(30)
}

/// Contents of a `.claim` file. Older versions wrote only host/pid/started; those parse with
/// `lease_expires_unix == 0` and are treated as "legacy" claims.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ClaimBody {
    #[serde(default)]
    pub schema: u32,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub pid: u32,
    /// Opaque process start time (Windows FILETIME / Linux starttime ticks).
    #[serde(default)]
    pub proc_start: Option<String>,
    #[serde(default)]
    pub run_id: String,
    #[serde(default)]
    pub started: String,
    #[serde(default)]
    pub started_unix: u64,
    /// Last heartbeat (unix seconds). Reclaim markers reuse it as their timestamp.
    #[serde(default)]
    pub heartbeat_unix: u64,
    /// Lease expiry written by the owner (unix seconds); 0 = legacy claim without a lease.
    #[serde(default)]
    pub lease_expires_unix: u64,
    #[serde(default)]
    pub lease_secs: u64,
    /// All volume file names of the claimed set (first volume first).
    #[serde(default)]
    pub volumes: Vec<String>,
}

/// Unique id of this process run.
pub fn run_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(unique_token)
}

pub fn new_body(volumes: Vec<String>, p: &LeaseParams) -> ClaimBody {
    let now = unix_secs();
    ClaimBody {
        schema: 2,
        host: hostname(),
        pid: std::process::id(),
        proc_start: process_start(std::process::id()),
        run_id: run_id().to_string(),
        started: rfc3339_utc(now),
        started_unix: now,
        heartbeat_unix: now,
        lease_expires_unix: now + p.lease_secs,
        lease_secs: p.lease_secs,
        volumes,
    }
}

/// Write `body` to `path` through a temp file + rename (readers never see a partial file).
pub fn write_atomic(path: &Path, body: &ClaimBody) -> io::Result<()> {
    let tmp = PathBuf::from(format!("{}.hb-{}", path.display(), &unique_token()[..8]));
    let json = serde_json::to_vec(body).map_err(io::Error::other)?;
    fs::write(&tmp, json)?;
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

pub fn read_body(path: &Path) -> Option<ClaimBody> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()
}

// ------------------------------------------------------------------ process liveness

enum Query {
    Gone,
    Running(Option<String>),
    Unknown,
}

#[cfg(windows)]
fn query(pid: u32) -> Query {
    use std::ffi::c_void;
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct FileTime {
        lo: u32,
        hi: u32,
    }
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        fn CloseHandle(h: *mut c_void) -> i32;
        fn GetLastError() -> u32;
        fn GetExitCodeProcess(h: *mut c_void, code: *mut u32) -> i32;
        fn GetProcessTimes(
            h: *mut c_void,
            c: *mut FileTime,
            e: *mut FileTime,
            k: *mut FileTime,
            u: *mut FileTime,
        ) -> i32;
    }
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const ERROR_INVALID_PARAMETER: u32 = 87;
    const STILL_ACTIVE: u32 = 259;
    // SAFETY: plain Win32 calls with valid out-pointers; the handle is closed before return.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return if GetLastError() == ERROR_INVALID_PARAMETER {
                Query::Gone
            } else {
                Query::Unknown
            };
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code);
        if ok != 0 && code != STILL_ACTIVE {
            CloseHandle(h);
            return Query::Gone;
        }
        let (mut c, mut e, mut k, mut u) = (
            FileTime::default(),
            FileTime::default(),
            FileTime::default(),
            FileTime::default(),
        );
        let start = if GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) != 0 {
            Some(((u64::from(c.hi) << 32) | u64::from(c.lo)).to_string())
        } else {
            None
        };
        CloseHandle(h);
        Query::Running(start)
    }
}

#[cfg(not(windows))]
fn query(pid: u32) -> Query {
    if !Path::new("/proc/self/stat").exists() {
        return Query::Unknown;
    }
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => {
            // Fields after the parenthesised command name; starttime is field 22 overall.
            let start = s
                .rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().nth(19).map(str::to_string));
            Query::Running(start)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Query::Gone,
        Err(_) => Query::Unknown,
    }
}

/// Opaque start time of a running process, if the platform can tell.
pub fn process_start(pid: u32) -> Option<String> {
    match query(pid) {
        Query::Running(s) => s,
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Liveness {
    Alive,
    Dead,
}

/// Is the claimed owner (on this host) still running? Unknown counts as alive (conservative).
pub fn owner_liveness(pid: u32, recorded_start: &Option<String>) -> Liveness {
    match query(pid) {
        Query::Gone => Liveness::Dead,
        Query::Unknown => Liveness::Alive,
        Query::Running(now_start) => match (recorded_start, now_start) {
            (Some(r), Some(n)) if *r != n => Liveness::Dead,
            _ => Liveness::Alive,
        },
    }
}

// ------------------------------------------------------------------ heartbeat

/// Refreshes the claim's lease from a background thread until dropped.
pub struct Heartbeat {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn beat(claim_path: &Path, p: &LeaseParams) -> bool {
    // False when the claim is gone (reclaimed by someone else): stop beating.
    if !claim_path.exists() {
        return false;
    }
    if let Some(mut b) = read_body(claim_path) {
        let now = unix_secs();
        b.heartbeat_unix = now;
        b.lease_expires_unix = now + p.lease_secs;
        b.lease_secs = p.lease_secs;
        let _ = write_atomic(claim_path, &b);
    }
    true
}

/// Start refreshing `claim_path` every `heartbeat_secs`. Drop the guard BEFORE the claim file
/// is removed so a late beat cannot recreate it.
pub fn start_heartbeat(claim_path: PathBuf, p: LeaseParams) -> Heartbeat {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let handle = std::thread::spawn(move || {
        let interval = Duration::from_secs(p.heartbeat_secs.max(1));
        'outer: loop {
            let t0 = std::time::Instant::now();
            while t0.elapsed() < interval {
                if flag.load(Ordering::SeqCst) {
                    break 'outer;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            if flag.load(Ordering::SeqCst) || !beat(&claim_path, &p) {
                break;
            }
        }
    });
    Heartbeat {
        stop,
        handle: Some(handle),
    }
}

// ------------------------------------------------------------------ reclaim

#[derive(Default, Debug, Clone)]
pub struct ReclaimReport {
    /// Human-readable actions (for reconcile / logs).
    pub actions: Vec<String>,
    /// First-volume names put back into the inbox.
    pub requeued: Vec<String>,
    /// First-volume names moved to `failed/` (retry cap).
    pub failed: Vec<String>,
}

fn mtime_unix(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_retries(path: &Path) -> u32 {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Remove library temp files left by a dead worker (old enough that no live one owns them).
fn clean_library_tmp(cfg: &Config, rep: &mut ReclaimReport) {
    let min_age = cfg.tmp_age_secs.max(cfg.lease_secs + cfg.lease_grace_secs);
    let Ok(rd) = fs::read_dir(&cfg.library_dir) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with(".tmp-") {
            continue;
        }
        let path = e.path();
        let age = unix_secs().saturating_sub(mtime_unix(&path));
        if age < min_age {
            continue;
        }
        let ok = if path.is_dir() {
            fs::remove_dir_all(&path).is_ok()
        } else {
            fs::remove_file(&path).is_ok()
        };
        if ok {
            rep.actions
                .push(format!("removed partial output {name} of a dead worker"));
        }
    }
}

/// Move the claimed volumes out of `processing/` (back to the inbox, or to `failed/` after too
/// many reclaims), then drop the claim. Idempotent: a crashed reclaimer's successor resumes.
/// The first volume moves LAST and the claim goes after it, so a half-done reclaim still looks
/// like a claimed job (and not like a claim without a zip).
fn finish_reclaim(
    cfg: &Config,
    dirs: &Dirs,
    claim_path: &Path,
    first: &str,
    body: &ClaimBody,
    rep: &mut ReclaimReport,
) {
    let mut volumes = body.volumes.clone();
    if !volumes.iter().any(|v| v == first) {
        volumes.insert(0, first.to_string());
    }
    if body.volumes.len() <= 1 {
        for s in siblings_in(&dirs.processing, first) {
            if !volumes.contains(&s) {
                volumes.push(s);
            }
        }
    }
    // First volume last.
    volumes.retain(|v| v != first);
    volumes.push(first.to_string());
    let retries_path = dirs.processing.join(format!("{first}.retries"));
    let n = read_retries(&retries_path) + 1;
    clean_library_tmp(cfg, rep);
    if n > cfg.max_reclaims {
        for v in &volumes {
            let src = dirs.processing.join(v);
            if src.exists() {
                let _ = move_unique(&src, &dirs.failed, v);
            }
        }
        let _ = fs::write(
            dirs.failed.join(format!("{first}.error.txt")),
            format!(
                "STALE_RETRIES_EXCEEDED: the claim was reclaimed {} times (worker crashed or hung each time); giving up\n",
                n - 1
            ),
        );
        let _ = fs::remove_file(&retries_path);
        rep.failed.push(first.to_string());
        rep.actions.push(format!(
            "STALE_RETRIES_EXCEEDED: {first} moved to failed/ after {} reclaims",
            n - 1
        ));
    } else {
        // Count first, then move: a crash in between still counts the attempt.
        let _ = fs::write(&retries_path, n.to_string());
        for v in &volumes {
            let src = dirs.processing.join(v);
            if src.exists() {
                if let Err(e) = move_unique(&src, &dirs.root, v) {
                    rep.actions
                        .push(format!("could not requeue volume {v} of {first}: {e}"));
                }
            }
        }
        rep.requeued.push(first.to_string());
        rep.actions.push(format!(
            "stale claim reclaimed ({n}/{}): {first} back in the inbox",
            cfg.max_reclaims
        ));
    }
    let _ = fs::remove_file(claim_path);
}

/// Exclusive reclaim lock. `create_new` is the only primitive that is exclusive on every
/// platform (a rename of a file with open handles is not on Windows). A lock whose timestamp
/// is older than lease + grace belongs to a crashed reclaimer and is taken over.
fn acquire_lock(lock: &Path, cfg: &Config) -> bool {
    let body = serde_json::json!({
        "run_id": run_id(),
        "pid": std::process::id(),
        "ts": unix_secs(),
    })
    .to_string();
    for attempt in 0..2 {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(lock)
        {
            Ok(mut f) => {
                use io::Write;
                let _ = f.write_all(body.as_bytes());
                let _ = f.sync_all();
                return true;
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempt == 0 => {
                let ts = fs::read_to_string(lock)
                    .ok()
                    .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                    .and_then(|v| v.get("ts").and_then(|x| x.as_u64()))
                    .unwrap_or_else(|| mtime_unix(lock));
                if unix_secs() <= ts + cfg.lease_secs + cfg.lease_grace_secs {
                    return false; // a live reclaimer holds it
                }
                let _ = fs::remove_file(lock); // crashed reclaimer: take over once
            }
            Err(_) => return false,
        }
    }
    false
}

/// Look for stale claims in `processing/` and reclaim them. Safe to call from several
/// sanitizers at once. `report_legacy` also reports (without moving) legacy claims that are
/// old but not provably dead when `--requeue` is not set.
pub fn reclaim_stale(cfg: &Config, dirs: &Dirs, report_legacy: bool) -> ReclaimReport {
    let mut rep = ReclaimReport::default();
    let Ok(rd) = fs::read_dir(&dirs.processing) else {
        return rep;
    };
    let names: Vec<(String, PathBuf)> = rd
        .flatten()
        .map(|e| (e.file_name().to_string_lossy().to_string(), e.path()))
        .collect();
    let me = hostname();

    // Is this claim reclaimable right now? Some(true) = stale, Some(false) = leave it,
    // None = legacy claim that is only reported.
    let decide = |claim_path: &Path, first: &str, rep: &mut ReclaimReport| -> Option<bool> {
        let now = unix_secs();
        let parsed = read_body(claim_path);
        let legacy = parsed.as_ref().is_none_or(|b| b.lease_expires_unix == 0);
        let body = parsed.unwrap_or_default();
        if body.run_id == run_id() && body.pid == std::process::id() {
            return Some(false); // our own job
        }
        let expiry = if legacy {
            mtime_unix(claim_path) + cfg.stale_mins * 60
        } else {
            body.lease_expires_unix + cfg.lease_grace_secs
        };
        let expired = if legacy { now >= expiry } else { now > expiry };
        let same_host =
            body.pid != 0 && !body.host.is_empty() && body.host.eq_ignore_ascii_case(&me);
        let liveness = same_host.then(|| owner_liveness(body.pid, &body.proc_start));
        if liveness == Some(Liveness::Alive) {
            return Some(false); // a live owner on this host is never reclaimed
        }
        let dead = liveness == Some(Liveness::Dead);
        if !(expired || dead) {
            return Some(false);
        }
        if legacy && !dead && !cfg.requeue {
            if report_legacy {
                let age = now.saturating_sub(mtime_unix(claim_path));
                rep.actions.push(format!(
                    "STALE: {first} has been in processing/ for {} min (use --requeue to put it back)",
                    age / 60
                ));
            }
            return None;
        }
        Some(true)
    };

    for (name, claim_path) in &names {
        let Some(first) = name.strip_suffix(".claim") else {
            continue;
        };
        // A claim whose zip never arrived (claimer died between the two steps).
        if !dirs.processing.join(first).exists() {
            let age = unix_secs().saturating_sub(mtime_unix(claim_path));
            if age >= cfg.claim_grace_secs && fs::remove_file(claim_path).is_ok() {
                rep.actions
                    .push(format!("removed claim without a zip: {name}"));
            }
            continue;
        }
        if decide(claim_path, first, &mut rep) != Some(true) {
            continue;
        }
        // Exclusive takeover; then re-check what we may have raced with (a heartbeat, or a
        // reclaimer that finished just before we got the lock).
        let lock = dirs.processing.join(format!("{first}.reclaim"));
        if !acquire_lock(&lock, cfg) {
            continue;
        }
        if claim_path.exists() && decide(claim_path, first, &mut rep) == Some(true) {
            let body = read_body(claim_path).unwrap_or_default();
            finish_reclaim(cfg, dirs, claim_path, first, &body, &mut rep);
        }
        let _ = fs::remove_file(&lock);
    }
    rep
}
