//! The import-folder pipeline: claims, startup reconcile and sanitize passes.
//!
//! The inbox is the zips lying directly in the import root plus the zips in its
//! optional `unprocessed/` subfolder (kept for compatibility). State folders under the
//! import root: `processing`, `needs-review`, `failed`, `processed` (subfolders are
//! never scanned as inboxes).

use crate::error::{NeedsReview, SanitizeError};
use crate::sanitize::{sanitize_zip, SanitizeOutcome};
use crate::util::{hostname, is_busy_error, move_unique, rfc3339_utc, unix_secs};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

#[derive(Clone, Debug)]
pub struct Config {
    pub import_dir: PathBuf,
    pub library_dir: PathBuf,
    /// Size+mtime must stay unchanged for this long before a zip is claimed.
    pub stable_secs: u64,
    /// A claim older than this (with its zip still in `processing/`) is stale.
    pub stale_mins: u64,
    /// Move stale zips back into the inbox instead of only reporting them.
    pub requeue: bool,
    /// Library temp files / uncommitted zips must be this old before reconcile touches them.
    pub tmp_age_secs: u64,
    /// A claim without a zip in `processing/` is removed after this long.
    pub claim_grace_secs: u64,
    /// Attempts for truncated / busy zips before they move to `failed/`.
    pub max_retries: u32,
}

impl Config {
    pub fn new(import_dir: impl Into<PathBuf>, library_dir: impl Into<PathBuf>) -> Self {
        Config {
            import_dir: import_dir.into(),
            library_dir: library_dir.into(),
            stable_secs: 5,
            stale_mins: 120,
            requeue: false,
            tmp_age_secs: 900,
            claim_grace_secs: 300,
            max_retries: 3,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Dirs {
    pub root: PathBuf,
    pub unprocessed: PathBuf,
    pub processing: PathBuf,
    pub needs_review: PathBuf,
    pub failed: PathBuf,
    pub processed: PathBuf,
}

impl Dirs {
    pub fn new(root: &Path) -> Self {
        Dirs {
            root: root.to_path_buf(),
            unprocessed: root.join("unprocessed"),
            processing: root.join("processing"),
            needs_review: root.join("needs-review"),
            failed: root.join("failed"),
            processed: root.join("processed"),
        }
    }

    /// Inbox folders that are scanned: the import root and `unprocessed/`.
    pub fn inboxes(&self) -> [&Path; 2] {
        [&self.root, &self.unprocessed]
    }

    /// Create the state folders (`unprocessed/` is optional and never created).
    pub fn create_all(&self) -> std::io::Result<()> {
        for d in [
            &self.processing,
            &self.needs_review,
            &self.failed,
            &self.processed,
        ] {
            fs::create_dir_all(d)?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct Claim {
    pub name: String,
    pub claim_path: PathBuf,
    /// Location of the zip inside `processing/`.
    pub zip_path: PathBuf,
    /// The inbox folder the zip came from (where a deferred zip is put back).
    pub origin: PathBuf,
}

#[derive(Debug)]
pub enum ClaimResult {
    Claimed(Claim),
    /// Another worker owns it (or it is gone).
    Taken,
    /// Temporarily unavailable (file in use); retry next pass.
    Busy(String),
}

/// Claim an inbox zip: create `processing/<name>.claim` exclusively, then rename the
/// zip into `processing/`. Exactly one concurrent caller wins. Two inbox zips with the
/// same name are handled one after the other (the claim is per name), never clobbered.
pub fn try_claim(dirs: &Dirs, src: &Path) -> ClaimResult {
    let Some(name) = src
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|_| src.parent().is_some())
    else {
        return ClaimResult::Taken;
    };
    let name = name.as_str();
    let origin = src.parent().map(Path::to_path_buf).unwrap_or_default();
    let claim_path = dirs.processing.join(format!("{name}.claim"));
    if !src.exists() {
        return ClaimResult::Taken;
    }
    let mut f = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&claim_path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return ClaimResult::Taken,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if fs::create_dir_all(&dirs.processing).is_err() {
                return ClaimResult::Busy(e.to_string());
            }
            return ClaimResult::Busy("processing folder was missing".into());
        }
        Err(e) => return ClaimResult::Busy(e.to_string()),
    };
    let body = format!(
        "{{\"host\":{:?},\"pid\":{},\"started\":{:?},\"started_unix\":{}}}",
        hostname(),
        std::process::id(),
        rfc3339_utc(unix_secs()),
        unix_secs()
    );
    let _ = f.write_all(body.as_bytes());
    let _ = f.sync_all();
    drop(f);
    let dst = dirs.processing.join(name);
    if dst.exists() {
        // rename would overwrite on some platforms; never clobber a zip being processed.
        let _ = fs::remove_file(&claim_path);
        return ClaimResult::Busy(format!("{name} already exists in processing/"));
    }
    match fs::rename(src, &dst) {
        Ok(()) => ClaimResult::Claimed(Claim {
            name: name.to_string(),
            claim_path,
            zip_path: dst,
            origin,
        }),
        Err(e) => {
            let _ = fs::remove_file(&claim_path);
            if e.kind() == std::io::ErrorKind::NotFound {
                ClaimResult::Taken
            } else if is_busy_error(&e) {
                ClaimResult::Busy(e.to_string())
            } else {
                ClaimResult::Busy(format!("rename failed: {e}"))
            }
        }
    }
}

#[derive(Default, Debug, Clone)]
pub struct PassReport {
    pub sanitized: Vec<String>,
    pub duplicates: Vec<String>,
    pub needs_review: Vec<String>,
    pub failed: Vec<String>,
    pub deferred: Vec<String>,
    pub busy: Vec<String>,
    pub loose_files: Vec<String>,
    /// Zips seen but not yet stable.
    pub pending_unstable: usize,
}

impl PassReport {
    pub fn merge(&mut self, o: PassReport) {
        self.sanitized.extend(o.sanitized);
        self.duplicates.extend(o.duplicates);
        self.needs_review.extend(o.needs_review);
        self.failed.extend(o.failed);
        self.deferred.extend(o.deferred);
        self.busy.extend(o.busy);
        for l in o.loose_files {
            if !self.loose_files.contains(&l) {
                self.loose_files.push(l);
            }
        }
        self.pending_unstable = o.pending_unstable;
    }

    /// 0 = ok, 2 = some zips needed review or failed.
    pub fn exit_code(&self) -> i32 {
        if self.needs_review.is_empty() && self.failed.is_empty() {
            0
        } else {
            2
        }
    }
}

#[derive(Default, Debug)]
pub struct State {
    /// Stability tracking keyed by full path (the same name can sit in both inboxes).
    seen: HashMap<PathBuf, (u64, Option<SystemTime>, Instant)>,
    attempts: HashMap<String, u32>,
}

fn write_note(dir: &Path, base: &str, suffix: &str, text: &str) {
    let _ = fs::write(dir.join(format!("{base}{suffix}")), text);
}

/// One scan of the inbox: claim every stable zip and sanitize it.
pub fn run_pass(cfg: &Config, state: &mut State, log: &mut dyn FnMut(&str)) -> PassReport {
    let dirs = Dirs::new(&cfg.import_dir);
    let mut rep = PassReport::default();
    if let Err(e) = dirs.create_all() {
        log(&format!("cannot create import folders: {e}"));
        rep.busy.push(format!("import folders: {e}"));
        return rep;
    }
    // Inboxes: the import root itself and the optional unprocessed/ subfolder.
    let mut found: Vec<(PathBuf, String, u64, Option<SystemTime>)> = Vec::new();
    for (i, inbox) in dirs.inboxes().into_iter().enumerate() {
        let rd = match fs::read_dir(inbox) {
            Ok(rd) => rd,
            Err(e) => {
                // unprocessed/ is optional; the import root itself must be readable.
                if i == 0 {
                    log(&format!("cannot read import folder: {e}"));
                    rep.busy.push(format!("import folder: {e}"));
                }
                continue;
            }
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let lower = name.to_ascii_lowercase();
            let Ok(md) = e.metadata() else { continue };
            if md.is_dir() {
                continue;
            }
            if lower.ends_with(".part") || lower.ends_with(".tmp") || lower.ends_with(".claim") {
                continue;
            }
            if !lower.ends_with(".zip") {
                rep.loose_files.push(if i == 0 {
                    name
                } else {
                    format!("unprocessed/{name}")
                });
                continue;
            }
            found.push((e.path(), name, md.len(), md.modified().ok()));
        }
    }
    found.sort();
    state.seen.retain(|k, _| found.iter().any(|f| &f.0 == k));

    for (path, name, size, mtime) in found {
        let now = Instant::now();
        let stable = match state.seen.get(&path) {
            Some((s, m, since)) if *s == size && *m == mtime => {
                now.duration_since(*since) >= Duration::from_secs(cfg.stable_secs)
            }
            _ => {
                state.seen.insert(path.clone(), (size, mtime, now));
                cfg.stable_secs == 0
            }
        };
        if !stable {
            rep.pending_unstable += 1;
            continue;
        }
        match try_claim(&dirs, &path) {
            ClaimResult::Taken => {}
            ClaimResult::Busy(why) => {
                log(&format!("{name}: busy ({why}), will retry"));
                rep.busy.push(name);
            }
            ClaimResult::Claimed(claim) => {
                log(&format!("{name}: claimed, sanitizing"));
                state.seen.remove(&path);
                process_claim(cfg, &dirs, state, claim, &mut rep, log);
            }
        }
    }
    rep
}

fn process_claim(
    cfg: &Config,
    dirs: &Dirs,
    state: &mut State,
    claim: Claim,
    rep: &mut PassReport,
    log: &mut dyn FnMut(&str),
) {
    let name = claim.name.clone();
    match sanitize_zip(&claim.zip_path, &cfg.library_dir) {
        Ok(SanitizeOutcome::Sanitized {
            library_zip,
            sidecar,
            ..
        }) => {
            log(&format!(
                "{name}: sanitized -> {} ({:?}, {} entries)",
                library_zip.display(),
                sidecar.kind,
                sidecar.entry_count
            ));
            state.attempts.remove(&name);
            finish_move(&claim, &dirs.processed, &name, rep, log);
            rep.sanitized.push(name);
        }
        Ok(SanitizeOutcome::Duplicate { existing }) => {
            log(&format!("{name}: duplicate of {}", existing.display()));
            if let Ok(p) = move_unique(&claim.zip_path, &dirs.processed, &name) {
                let base = p
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or(name.clone());
                write_note(
                    &dirs.processed,
                    &base,
                    ".duplicate.txt",
                    &format!("duplicate of {}\n", existing.display()),
                );
            } else {
                log(&format!("{name}: could not move duplicate to processed"));
            }
            let _ = fs::remove_file(&claim.claim_path);
            rep.duplicates.push(name);
        }
        Err(SanitizeError::NeedsReview(nr)) => {
            log(&format!("{name}: needs review: {nr}"));
            send_to_review(dirs, &claim, &nr);
            rep.needs_review.push(name);
        }
        Err(SanitizeError::Truncated(m)) | Err(SanitizeError::Io(m)) => {
            let n = state.attempts.entry(name.clone()).or_insert(0);
            *n += 1;
            if *n >= cfg.max_retries {
                log(&format!("{name}: giving up after {n} attempts: {m}"));
                send_to_failed(dirs, &claim, &m);
                state.attempts.remove(&name);
                rep.failed.push(name);
            } else {
                log(&format!(
                    "{name}: not readable yet ({m}); attempt {n}, putting back"
                ));
                match fs::rename(&claim.zip_path, claim.origin.join(&name)) {
                    Ok(()) => {
                        let _ = fs::remove_file(&claim.claim_path);
                    }
                    Err(e) => log(&format!("{name}: could not put back: {e}")),
                }
                rep.deferred.push(name);
            }
        }
        Err(SanitizeError::Corrupt(m)) => {
            log(&format!("{name}: corrupt: {m}"));
            send_to_failed(dirs, &claim, &m);
            rep.failed.push(name);
        }
    }
}

fn finish_move(
    claim: &Claim,
    dest: &Path,
    name: &str,
    rep: &mut PassReport,
    log: &mut dyn FnMut(&str),
) {
    match move_unique(&claim.zip_path, dest, name) {
        Ok(_) => {
            let _ = fs::remove_file(&claim.claim_path);
        }
        Err(e) => {
            log(&format!(
                "{name}: sanitized, but moving the original failed: {e}"
            ));
            rep.busy.push(format!("{name}: move to processed failed"));
        }
    }
}

fn send_to_review(dirs: &Dirs, claim: &Claim, nr: &NeedsReview) {
    let moved = move_unique(&claim.zip_path, &dirs.needs_review, &claim.name);
    let base = moved
        .as_ref()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| claim.name.clone());
    let json = serde_json::json!({
        "schema": 1,
        "source_zip": claim.name,
        "code": nr.code(),
        "detail": nr.to_string(),
        "diagnostic": nr,
        "at": rfc3339_utc(unix_secs()),
    });
    let _ = fs::write(
        dirs.needs_review.join(format!("{base}.review.json")),
        serde_json::to_vec_pretty(&json).unwrap_or_default(),
    );
    let _ = fs::remove_file(&claim.claim_path);
}

fn send_to_failed(dirs: &Dirs, claim: &Claim, why: &str) {
    let moved = move_unique(&claim.zip_path, &dirs.failed, &claim.name);
    let base = moved
        .as_ref()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| claim.name.clone());
    write_note(&dirs.failed, &base, ".error.txt", &format!("{why}\n"));
    let _ = fs::remove_file(&claim.claim_path);
}

/// One-shot: reconcile is the caller's job; this scans, waits one stability
/// interval when needed, and scans again.
pub fn run_once(cfg: &Config, log: &mut dyn FnMut(&str)) -> PassReport {
    let mut state = State::default();
    let mut rep = run_pass(cfg, &mut state, log);
    if rep.pending_unstable > 0 {
        log(&format!(
            "{} zip(s) not yet stable, waiting {}s",
            rep.pending_unstable, cfg.stable_secs
        ));
        std::thread::sleep(Duration::from_secs(cfg.stable_secs));
        let second = run_pass(cfg, &mut state, log);
        rep.merge(second);
    }
    rep
}

fn age_secs(path: &Path) -> Option<u64> {
    let m = fs::metadata(path).ok()?.modified().ok()?;
    Some(
        SystemTime::now()
            .duration_since(m)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    )
}

/// Startup reconcile after crashes. Returns human-readable actions.
pub fn reconcile(cfg: &Config) -> Vec<String> {
    let dirs = Dirs::new(&cfg.import_dir);
    let mut actions = Vec::new();
    let _ = dirs.create_all();
    let _ = fs::create_dir_all(&cfg.library_dir);

    // Library leftovers.
    if let Ok(rd) = fs::read_dir(&cfg.library_dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let path = e.path();
            let old = age_secs(&path)
                .map(|a| a >= cfg.tmp_age_secs)
                .unwrap_or(false);
            if name.starts_with(".tmp-") {
                if old && fs::remove_file(&path).is_ok() {
                    actions.push(format!("removed orphan temp file {name}"));
                }
            } else if name.to_ascii_lowercase().ends_with(".zip")
                && !cfg.library_dir.join(format!("{name}.json")).exists()
                && old
            {
                match move_unique(&path, &dirs.needs_review, &name) {
                    Ok(p) => {
                        let base = p
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or(name.clone());
                        let json = serde_json::json!({
                            "schema": 1,
                            "source_zip": name,
                            "code": "ORPHAN_LIBRARY_ZIP",
                            "detail": "library zip without sidecar (interrupted sanitize); moved out of the library",
                            "at": rfc3339_utc(unix_secs()),
                        });
                        let _ = fs::write(
                            dirs.needs_review.join(format!("{base}.review.json")),
                            serde_json::to_vec_pretty(&json).unwrap_or_default(),
                        );
                        actions.push(format!(
                            "library zip without sidecar moved to needs-review: {name}"
                        ));
                    }
                    Err(e) => {
                        actions.push(format!("could not move orphan library zip {name}: {e}"))
                    }
                }
            }
        }
    }

    // Claims and processing leftovers.
    let stale_secs = cfg.stale_mins * 60;
    let mut claimed_names: Vec<String> = Vec::new();
    if let Ok(rd) = fs::read_dir(&dirs.processing) {
        let entries: Vec<_> = rd.flatten().collect();
        for e in &entries {
            let name = e.file_name().to_string_lossy().to_string();
            let Some(zip_name) = name.strip_suffix(".claim") else {
                continue;
            };
            claimed_names.push(zip_name.to_string());
            let claim_path = e.path();
            let age = age_secs(&claim_path).unwrap_or(0);
            let zip_here = dirs.processing.join(zip_name);
            if !zip_here.exists() {
                if age >= cfg.claim_grace_secs && fs::remove_file(&claim_path).is_ok() {
                    actions.push(format!("removed claim without a zip: {name}"));
                }
            } else if age >= stale_secs {
                if cfg.requeue {
                    match move_unique(&zip_here, &dirs.root, zip_name) {
                        Ok(_) => {
                            let _ = fs::remove_file(&claim_path);
                            actions.push(format!("stale claim requeued: {zip_name}"));
                        }
                        Err(e) => actions.push(format!("could not requeue {zip_name}: {e}")),
                    }
                } else {
                    actions.push(format!(
                        "STALE: {zip_name} has been in processing/ for {} min (use --requeue to put it back)",
                        age / 60
                    ));
                }
            }
        }
        for e in &entries {
            let name = e.file_name().to_string_lossy().to_string();
            if name.ends_with(".claim") || claimed_names.contains(&name) {
                continue;
            }
            let age = age_secs(&e.path()).unwrap_or(0);
            if age >= stale_secs && name.to_ascii_lowercase().ends_with(".zip") {
                if cfg.requeue {
                    if move_unique(&e.path(), &dirs.root, &name).is_ok() {
                        actions.push(format!("unclaimed zip in processing requeued: {name}"));
                    }
                } else {
                    actions.push(format!(
                        "STALE: {name} is in processing/ without a claim (use --requeue)"
                    ));
                }
            }
        }
    }
    actions
}
