//! The import-folder pipeline: claims, startup reconcile and sanitize passes.
//!
//! The inbox is the zips lying directly in the import root plus the zips in its
//! optional `unprocessed/` subfolder (kept for compatibility). State folders under the
//! import root: `processing`, `needs-review`, `failed`, `processed` (subfolders are
//! never scanned as inboxes).

use crate::archive::{
    group_volumes, kind_of_name, sanitize_archive_with, sevenz_expected_len, ArchiveKind,
};
use crate::error::{NeedsReview, SanitizeError};
use crate::sanitize::{sanitize_zip_with_options, SanitizeOptions, SanitizeOutcome};
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
    /// Engine versions preferred when an archive wraps one inner archive per version.
    pub prefer_engine_versions: Vec<String>,
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
            prefer_engine_versions: vec!["5.8".to_string()],
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
    /// Later volumes of a multi-volume archive, inside `processing/` (volume order).
    pub extra_volumes: Vec<PathBuf>,
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
    try_claim_set(dirs, &[src.to_path_buf()])
}

/// Claim a volume set: the claim file is for the first volume; every volume is renamed
/// into `processing/` (rolled back when one of them cannot be moved).
pub fn try_claim_set(dirs: &Dirs, srcs: &[PathBuf]) -> ClaimResult {
    let Some(src) = srcs.first().map(PathBuf::as_path) else {
        return ClaimResult::Taken;
    };
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
        Ok(()) => {
            let mut extra = Vec::new();
            for v in &srcs[1..] {
                let vn = v.file_name().map(|n| n.to_os_string()).unwrap_or_default();
                let vdst = dirs.processing.join(&vn);
                if vdst.exists() || fs::rename(v, &vdst).is_err() {
                    // Roll back everything moved so far.
                    for done in &extra {
                        if let Some(n) = Path::new(done).file_name() {
                            let _ = fs::rename(done, origin.join(n));
                        }
                    }
                    let _ = fs::rename(&dst, src);
                    let _ = fs::remove_file(&claim_path);
                    return ClaimResult::Busy(format!(
                        "volume {} could not be moved",
                        vn.to_string_lossy()
                    ));
                }
                extra.push(vdst);
            }
            ClaimResult::Claimed(Claim {
                name: name.to_string(),
                claim_path,
                zip_path: dst,
                origin,
                extra_volumes: extra,
            })
        }
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
    /// Multi-volume sets with a missing volume (they wait; nothing is moved).
    pub waiting: Vec<String>,
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
        for l in o.waiting {
            if !self.waiting.contains(&l) {
                self.waiting.push(l);
            }
        }
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
    // Each inbox is grouped on its own: a volume set never spans two folders.
    struct Found {
        path: PathBuf,
        size: u64,
        mtime: Option<SystemTime>,
    }
    let mut sets: Vec<(crate::archive::VolumeSet, Vec<Found>, PathBuf)> = Vec::new();
    let mut all_paths: Vec<PathBuf> = Vec::new();
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
        let mut files: HashMap<String, Found> = HashMap::new();
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
            files.insert(
                name,
                Found {
                    path: e.path(),
                    size: md.len(),
                    mtime: md.modified().ok(),
                },
            );
        }
        let mut names: Vec<String> = files.keys().cloned().collect();
        names.sort();
        let (vsets, other) = group_volumes(&names);
        for name in other {
            rep.loose_files.push(if i == 0 {
                name
            } else {
                format!("unprocessed/{name}")
            });
        }
        for vs in vsets {
            let mut vols = Vec::new();
            for v in &vs.volumes {
                if let Some(f) = files.remove(v) {
                    all_paths.push(f.path.clone());
                    vols.push(f);
                }
            }
            sets.push((vs, vols, inbox.to_path_buf()));
        }
    }
    state.seen.retain(|k, _| all_paths.contains(k));

    for (set, vols, _inbox) in sets {
        let name = set.label.clone();
        let now = Instant::now();
        let mut all_stable = true;
        for f in &vols {
            let stable = match state.seen.get(&f.path) {
                Some((s, m, since)) if *s == f.size && *m == f.mtime => {
                    now.duration_since(*since) >= Duration::from_secs(cfg.stable_secs)
                }
                _ => {
                    state.seen.insert(f.path.clone(), (f.size, f.mtime, now));
                    cfg.stable_secs == 0
                }
            };
            all_stable &= stable;
        }
        if !set.complete {
            // A volume is missing: wait, never fail.
            if !rep.waiting.contains(&name) {
                rep.waiting.push(name);
            }
            continue;
        }
        if !all_stable {
            rep.pending_unstable += 1;
            continue;
        }
        if set.kind == ArchiveKind::SevenZ {
            let have: u64 = vols.iter().map(|f| f.size).sum();
            if let Some(need) = sevenz_expected_len(&vols[0].path) {
                if have < need {
                    log(&format!(
                        "{name}: 7z set has {have} of {need} bytes, waiting for more volumes"
                    ));
                    rep.waiting.push(name);
                    continue;
                }
            }
        }
        let paths: Vec<PathBuf> = vols.iter().map(|f| f.path.clone()).collect();
        match try_claim_set(&dirs, &paths) {
            ClaimResult::Taken => {}
            ClaimResult::Busy(why) => {
                log(&format!("{name}: busy ({why}), will retry"));
                rep.busy.push(name);
            }
            ClaimResult::Claimed(claim) => {
                log(&format!(
                    "{name}: claimed ({} volume(s)), sanitizing",
                    paths.len()
                ));
                for p in &paths {
                    state.seen.remove(p);
                }
                process_claim(cfg, &dirs, state, claim, &mut rep, log);
            }
        }
    }
    rep
}

fn run_sanitize(
    claim: &Claim,
    library: &Path,
    opts: &SanitizeOptions,
) -> Result<SanitizeOutcome, SanitizeError> {
    match kind_of_name(&claim.name) {
        Some(k) if k != ArchiveKind::Zip => {
            let mut vols = vec![claim.zip_path.clone()];
            vols.extend(claim.extra_volumes.iter().cloned());
            sanitize_archive_with(k, &vols, library, opts)
        }
        _ => sanitize_zip_with_options(&claim.zip_path, library, opts),
    }
}

/// Move the first volume and all later volumes into `dest`; returns the first volume's path.
fn move_claim_files(claim: &Claim, dest: &Path) -> std::io::Result<PathBuf> {
    let first = move_unique(&claim.zip_path, dest, &claim.name)?;
    let mut err = None;
    for v in &claim.extra_volumes {
        let n = v
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if let Err(e) = move_unique(v, dest, &n) {
            err.get_or_insert(e);
        }
    }
    match err {
        Some(e) => Err(e),
        None => Ok(first),
    }
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
    let opts = SanitizeOptions::new(cfg.prefer_engine_versions.clone());
    match run_sanitize(&claim, &cfg.library_dir, &opts) {
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
            if let Ok(p) = move_claim_files(&claim, &dirs.processed) {
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
            let waiting_for_volume = m.starts_with("MISSING_VOLUME");
            let n = state.attempts.entry(name.clone()).or_insert(0);
            if !waiting_for_volume {
                *n += 1;
            }
            if !waiting_for_volume && *n >= cfg.max_retries {
                log(&format!("{name}: giving up after {n} attempts: {m}"));
                send_to_failed(dirs, &claim, &m);
                state.attempts.remove(&name);
                rep.failed.push(name);
            } else {
                log(&format!(
                    "{name}: not readable yet ({m}); attempt {n}, putting back"
                ));
                let mut put_back_ok = true;
                for v in std::iter::once(&claim.zip_path).chain(claim.extra_volumes.iter()) {
                    let Some(vn) = v.file_name() else { continue };
                    if let Err(e) = fs::rename(v, claim.origin.join(vn)) {
                        put_back_ok = false;
                        log(&format!(
                            "{name}: could not put back {}: {e}",
                            vn.to_string_lossy()
                        ));
                    }
                }
                if put_back_ok {
                    let _ = fs::remove_file(&claim.claim_path);
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
    match move_claim_files(claim, dest) {
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
    let moved = move_claim_files(claim, &dirs.needs_review);
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
    let moved = move_claim_files(claim, &dirs.failed);
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

/// Other volumes of the archive whose first volume is `first`, found next to it in `dir`.
fn siblings_in(dir: &Path, first: &str) -> Vec<String> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let names: Vec<String> = rd
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| !n.ends_with(".claim"))
        .collect();
    let (sets, _) = group_volumes(&names);
    sets.into_iter()
        .find(|s| s.volumes.first().map(String::as_str) == Some(first))
        .map(|s| s.volumes.into_iter().skip(1).collect())
        .unwrap_or_default()
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
                let removed = if path.is_dir() {
                    old && fs::remove_dir_all(&path).is_ok()
                } else {
                    old && fs::remove_file(&path).is_ok()
                };
                if removed {
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
                            for sib in siblings_in(&dirs.processing, zip_name) {
                                let _ = move_unique(&dirs.processing.join(&sib), &dirs.root, &sib);
                            }
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
            // Later volumes ride along with their (claimed) first volume.
            if claimed_names
                .iter()
                .any(|c| siblings_in(&dirs.processing, c).contains(&name))
            {
                continue;
            }
            let age = age_secs(&e.path()).unwrap_or(0);
            if age >= stale_secs && kind_of_name(&name).is_some() {
                if cfg.requeue {
                    if move_unique(&e.path(), &dirs.root, &name).is_ok() {
                        for sib in siblings_in(&dirs.processing, &name) {
                            let _ = move_unique(&dirs.processing.join(&sib), &dirs.root, &sib);
                        }
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
