//! Bundles of the hosted server (`prepare_import` + `GET /bundles/<id>`).
//!
//! A bundle is one zip with the selected packages (plus companions and in-pack dependencies) under
//! `Content/<rel>` and a `manifest.json` with a SHA-256 per file. The client downloads it and
//! applies it to its own project; this server never writes a project, never writes the library
//! share and keeps nothing except its own bundle folder (`--bundle-dir`, server-local).
//!
//! [`BundleStore`] is the single owner of bundle state: published bundles with their expiry, the
//! build slot, the cache reservation and the rolling-hour budget. It has its own lock and is
//! shared (`Arc`) by the tool and the HTTP route; nothing here ever takes the catalog lock, so a
//! build or a download never blocks a search.
//!
//! Publication is atomic: the zip is written to `.tmp-<id>`, flushed, then renamed to `<id>.zip`.
//! A published bundle can be fetched repeatedly until its TTL (counted from publication) ends.

use crate::import::BundlePlan;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use ue_asset_library::{BundleError, BundleOutcome, BundleWriter, ImportError};

/// Settings of the bundle store; the defaults are the wire contract.
#[derive(Clone, Debug)]
pub struct BundleConfig {
    /// Server-local folder for bundles (never the library share).
    pub dir: PathBuf,
    /// How long a published bundle stays downloadable (from publication).
    pub ttl: Duration,
    /// Cap of one bundle (uncompressed bytes).
    pub max_bundle_bytes: u64,
    /// Cap of everything in the folder (published plus reserved for builds).
    pub max_cache_bytes: u64,
    /// Bundles built at the same time.
    pub max_builds: usize,
    /// Uncompressed bytes that may be bundled per rolling hour, all clients together.
    pub max_hour_bytes: u64,
    /// How often expired bundles are removed.
    pub sweep_interval: Duration,
    /// Tests: sleep inside the build (after the slot is taken, no lock held).
    pub build_delay: Duration,
}

impl BundleConfig {
    pub fn new(dir: PathBuf) -> BundleConfig {
        BundleConfig {
            dir,
            ttl: Duration::from_secs(900),
            max_bundle_bytes: 256 * 1024 * 1024,
            max_cache_bytes: 2 * 1024 * 1024 * 1024,
            max_builds: 1,
            max_hour_bytes: 1024 * 1024 * 1024,
            sweep_interval: Duration::from_secs(60),
            build_delay: Duration::ZERO,
        }
    }
}

/// A refused request: a stable `code` and a message that names sizes and caps.
#[derive(Debug)]
pub struct Refusal {
    pub code: &'static str,
    pub message: String,
}

impl Refusal {
    fn new(code: &'static str, message: impl Into<String>) -> Refusal {
        Refusal {
            code,
            message: message.into(),
        }
    }

    /// `CODE: message` (the tool error text without the `error: ` prefix).
    pub fn text(&self) -> String {
        format!("{}: {}", self.code, self.message)
    }
}

/// Why a download is not served.
#[derive(Debug, PartialEq, Eq)]
pub enum LeaseError {
    NotFound,
    /// The bundle existed but its TTL ended (the client prepares again).
    Expired,
}

/// An open bundle file admitted for download (admitted before expiry = served to the end).
pub struct Lease {
    pub id: String,
    pub file: File,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone)]
struct Published {
    path: PathBuf,
    size: u64,
    sha256: String,
    expires: Instant,
}

#[derive(Default)]
struct Inner {
    published: HashMap<String, Published>,
    /// Ids that expired while this process ran (answered 410 instead of 404).
    expired: HashMap<String, Instant>,
    building: usize,
    reserved: u64,
    hour: VecDeque<(Instant, u64)>,
    active: HashSet<String>,
}

pub struct BundleStore {
    cfg: BundleConfig,
    inner: Mutex<Inner>,
    counter: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

const HOUR: Duration = Duration::from_secs(3600);
const MAX_TOMBSTONES: usize = 4096;

/// A bundle id: 32 lower-case hex digits.
pub fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `<id>.zip` of a published bundle.
fn is_bundle_file(name: &str) -> bool {
    name.strip_suffix(".zip").is_some_and(valid_id)
}

/// `.tmp-<id>` of a bundle being built.
fn tmp_id(name: &str) -> Option<&str> {
    name.strip_prefix(".tmp-").filter(|id| valid_id(id))
}

/// RFC 3339 UTC time of a unix timestamp.
pub fn rfc3339_unix(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl BundleStore {
    /// Open the store: create the folder and remove what an earlier run left behind (files named
    /// exactly `.tmp-<id>` or `<id>.zip` with a 32-hex id; nothing else in the folder is touched).
    pub fn open(cfg: BundleConfig) -> std::io::Result<BundleStore> {
        fs::create_dir_all(&cfg.dir)?;
        for e in fs::read_dir(&cfg.dir)?.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if is_bundle_file(&name) || tmp_id(&name).is_some() {
                let _ = fs::remove_file(e.path());
            }
        }
        Ok(BundleStore {
            cfg,
            inner: Mutex::new(Inner::default()),
            counter: AtomicU64::new(0),
        })
    }

    pub fn config(&self) -> &BundleConfig {
        &self.cfg
    }

    /// Published bundles that can still be downloaded.
    pub fn pending(&self) -> usize {
        let now = Instant::now();
        lock(&self.inner)
            .published
            .values()
            .filter(|p| p.expires > now)
            .count()
    }

    fn new_id(&self) -> String {
        use std::collections::hash_map::RandomState;
        use std::hash::{BuildHasher, Hasher};
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        let mut h = Sha256::new();
        // RandomState keys come from the operating system's random source.
        for _ in 0..2 {
            let mut s = RandomState::new().build_hasher();
            s.write_u64(n);
            h.update(s.finish().to_le_bytes());
        }
        h.update(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
                .to_le_bytes(),
        );
        h.update(n.to_le_bytes());
        h.update(std::process::id().to_le_bytes());
        hex(&h.finalize()[..16])
    }

    /// Move expired bundles to the tombstones; returns the files to delete (outside the lock).
    fn expire_locked(g: &mut Inner, now: Instant) -> Vec<PathBuf> {
        let gone: Vec<String> = g
            .published
            .iter()
            .filter(|(_, p)| p.expires <= now)
            .map(|(id, _)| id.clone())
            .collect();
        let mut files = Vec::new();
        for id in gone {
            if let Some(p) = g.published.remove(&id) {
                files.push(p.path);
                g.expired.insert(id, now);
            }
        }
        g.expired.retain(|_, at| now.duration_since(*at) < HOUR);
        if g.expired.len() > MAX_TOMBSTONES {
            g.expired.clear();
        }
        files
    }

    fn delete_all(files: Vec<PathBuf>) {
        for f in files {
            let _ = fs::remove_file(f);
        }
    }

    /// Take the build slot and reserve room for a bundle of about `estimate` uncompressed bytes.
    /// `BUSY` when the slot or the cache budget is full, `CAP_EXCEEDED` when the rolling-hour
    /// budget would be exceeded.
    pub fn begin(&self, estimate: u64) -> Result<BuildSlot<'_>, Refusal> {
        let now = Instant::now();
        // The zip can be a little larger than its content (headers; Stored data).
        let reserve = estimate
            .saturating_add(estimate / 16)
            .saturating_add(1 << 20);
        let (files, outcome) = {
            let mut g = lock(&self.inner);
            let files = Self::expire_locked(&mut g, now);
            while g
                .hour
                .front()
                .is_some_and(|(at, _)| now.duration_since(*at) >= HOUR)
            {
                g.hour.pop_front();
            }
            let published: u64 = g.published.values().map(|p| p.size).sum();
            let used_hour: u64 = g.hour.iter().map(|(_, b)| *b).sum();
            let outcome = if g.building >= self.cfg.max_builds {
                Err(Refusal::new(
                    "BUSY",
                    "another bundle is being built; retry in a few seconds",
                ))
            } else if published.saturating_add(g.reserved).saturating_add(reserve)
                > self.cfg.max_cache_bytes
            {
                Err(Refusal::new(
                    "BUSY",
                    format!(
                        "the bundle cache is full ({} of {} bytes in use, this bundle needs about {reserve}); retry when older bundles expire",
                        published + g.reserved,
                        self.cfg.max_cache_bytes
                    ),
                ))
            } else if used_hour.saturating_add(estimate) > self.cfg.max_hour_bytes {
                Err(Refusal::new(
                    "CAP_EXCEEDED",
                    format!(
                        "the hourly bundle budget is used up ({used_hour} of {} bytes in the last hour, this bundle needs {estimate}); retry later",
                        self.cfg.max_hour_bytes
                    ),
                ))
            } else {
                let id = self.new_id();
                g.building += 1;
                g.reserved += reserve;
                g.active.insert(format!(".tmp-{id}"));
                Ok(id)
            };
            (files, outcome)
        };
        Self::delete_all(files);
        outcome.map(|id| BuildSlot {
            store: self,
            id,
            reserve,
            done: false,
        })
    }

    /// Open a published bundle for download. A lease admitted here completes even if the TTL ends
    /// while it streams.
    pub fn lease(&self, id: &str) -> Result<Lease, LeaseError> {
        if !valid_id(id) {
            return Err(LeaseError::NotFound);
        }
        let now = Instant::now();
        let found = {
            let g = lock(&self.inner);
            match g.published.get(id) {
                Some(p) if p.expires > now => Ok(p.clone()),
                Some(_) => Err(LeaseError::Expired),
                None if g.expired.contains_key(id) => Err(LeaseError::Expired),
                None => Err(LeaseError::NotFound),
            }
        };
        let p = found?;
        let file = File::open(&p.path).map_err(|_| LeaseError::Expired)?;
        Ok(Lease {
            id: id.to_string(),
            file,
            size: p.size,
            sha256: p.sha256,
        })
    }

    /// Remove expired bundles and stale `.tmp-*` files (older than an hour, not being built).
    pub fn sweep(&self) {
        let (files, active) = {
            let mut g = lock(&self.inner);
            let files = Self::expire_locked(&mut g, Instant::now());
            (files, g.active.clone())
        };
        Self::delete_all(files);
        let Ok(rd) = fs::read_dir(&self.cfg.dir) else {
            return;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if tmp_id(&name).is_none() || active.contains(&name) {
                continue;
            }
            let old = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age >= HOUR);
            if old {
                let _ = fs::remove_file(e.path());
            }
        }
    }
}

/// The build slot: dropped without [`BuildSlot::publish`] it frees the reservation and deletes
/// the temporary file.
pub struct BuildSlot<'a> {
    store: &'a BundleStore,
    id: String,
    reserve: u64,
    done: bool,
}

/// What publication returns.
#[derive(Debug, Clone)]
pub struct PublishedInfo {
    pub expires_at: String,
}

impl BuildSlot<'_> {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Where the zip is written until it is published.
    pub fn tmp_path(&self) -> PathBuf {
        self.store.cfg.dir.join(format!(".tmp-{}", self.id))
    }

    fn release(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        let mut g = lock(&self.store.inner);
        g.building = g.building.saturating_sub(1);
        g.reserved = g.reserved.saturating_sub(self.reserve);
        g.active.remove(&format!(".tmp-{}", self.id));
    }

    /// Rename the finished temporary file to `<id>.zip` (atomic publication) and start its TTL.
    pub fn publish(mut self, outcome: &BundleOutcome) -> std::io::Result<PublishedInfo> {
        let dest = self.store.cfg.dir.join(format!("{}.zip", self.id));
        fs::rename(self.tmp_path(), &dest)?;
        let now = Instant::now();
        let ttl = self.store.cfg.ttl;
        let expires = now.checked_add(ttl).unwrap_or(now + HOUR);
        let expires_at = rfc3339_unix(unix_now().saturating_add(ttl.as_secs() as i64));
        {
            let mut g = lock(&self.store.inner);
            g.published.insert(
                self.id.clone(),
                Published {
                    path: dest,
                    size: outcome.size_bytes,
                    sha256: outcome.sha256.clone(),
                    expires,
                },
            );
            g.hour.push_back((now, outcome.uncompressed_bytes));
        }
        self.release();
        Ok(PublishedInfo { expires_at })
    }
}

impl Drop for BuildSlot<'_> {
    fn drop(&mut self) {
        if !self.done {
            let _ = fs::remove_file(self.tmp_path());
            self.release();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// prepare_import
// ---------------------------------------------------------------------------------------------

/// A resolved `prepare_import` request (made under the catalog lock, run without it).
#[derive(Clone, Debug)]
pub struct PrepareJob {
    pub plan: BundlePlan,
    pub catalog_generation: u64,
}

/// Cap of the list lengths in the tool result (the manifest in the zip has everything).
const MAX_LIST: usize = 200;

fn capped(items: &[String]) -> Vec<&String> {
    items.iter().take(MAX_LIST).collect()
}

fn refusal(e: BundleError) -> Refusal {
    match e {
        BundleError::CapExceeded { size, cap } => Refusal::new(
            "CAP_EXCEEDED",
            format!("the bundle would hold {size} bytes uncompressed; the cap is {cap}. Select fewer assets or pass include_deps=false."),
        ),
        BundleError::Conflict(paths) => Refusal::new(
            "CONFLICT",
            format!(
                "the selected packs fill the same Content path with different bytes: {}",
                paths.iter().take(20).cloned().collect::<Vec<_>>().join(", ")
            ),
        ),
        BundleError::Import(ImportError::Io(_)) => Refusal::new(
            "NOT_FOUND",
            "a library zip could not be read (it may have been removed); retry after the catalog refreshes",
        ),
        BundleError::Import(ImportError::UnknownPackage(g)) => Refusal::new(
            "NOT_FOUND",
            format!("package {g} is not in its pack any more (stale catalog); retry after the catalog refreshes"),
        ),
        BundleError::Import(e) => Refusal::new("BUILD_FAILED", e.to_string()),
        BundleError::Io(_) => Refusal::new(
            "BUILD_FAILED",
            "the bundle could not be written on the server",
        ),
    }
}

fn overall_verdict(verdicts: impl Iterator<Item = &'static str>) -> &'static str {
    let mut rank = 0u8;
    for v in verdicts {
        rank = rank.max(match v {
            "newer" => 3,
            "unknown" => 2,
            "older" => 1,
            _ => 0,
        });
    }
    match rank {
        3 => "newer",
        2 => "unknown",
        1 => "older",
        _ => "exact",
    }
}

/// Build, verify and publish the bundle of `job`; returns the tool result. Holds only the store's
/// own lock (briefly): the catalog is never locked here.
pub fn prepare(store: &BundleStore, job: PrepareJob) -> Result<Value, Refusal> {
    let cfg = store.config();
    let plan = &job.plan;
    if plan.estimated_bytes > cfg.max_bundle_bytes {
        return Err(refusal(BundleError::CapExceeded {
            size: plan.estimated_bytes,
            cap: cfg.max_bundle_bytes,
        }));
    }
    for p in &plan.packs {
        if !p.entry.zip_path.is_file() {
            return Err(Refusal::new(
                "NOT_FOUND",
                format!(
                    "the library zip of pack `{}` is missing on the server",
                    p.pack_name
                ),
            ));
        }
    }
    let slot = store.begin(plan.estimated_bytes)?;
    if !cfg.build_delay.is_zero() {
        std::thread::sleep(cfg.build_delay);
    }
    let mut writer =
        BundleWriter::create(&slot.tmp_path(), cfg.max_bundle_bytes).map_err(refusal)?;
    let mut external: BTreeSet<String> = BTreeSet::new();
    let mut omitted: Vec<String> = Vec::new();
    for p in &plan.packs {
        let ex = writer
            .add_subset(&p.entry, &p.index, &p.selection)
            .map_err(refusal)?;
        external.extend(ex.external_missing);
        omitted.extend(
            ex.unselected_deps
                .into_iter()
                .map(|g| format!("{}:{g}", p.pack_id)),
        );
    }
    omitted.sort();
    omitted.dedup();
    let external: Vec<String> = external.into_iter().collect();
    let mut warnings = plan.warnings.clone();
    if !external.is_empty() {
        warnings.push(format!(
            "{} referenced package(s) are not in the pack(s) (e.g. {}); the imported assets may show missing references until they are added from elsewhere",
            external.len(),
            external[0]
        ));
    }
    if !omitted.is_empty() {
        warnings.push(format!(
            "{} in-pack dependenc{} not included (e.g. {}); a skipped dependency can leave an asset unusable",
            omitted.len(),
            if omitted.len() == 1 { "y was" } else { "ies were" },
            omitted[0]
        ));
    }
    let verdict = overall_verdict(plan.packs.iter().map(|p| p.verdict));
    let engine = crate::release::format_version(plan.engine);
    let packs: Vec<Value> = plan
        .packs
        .iter()
        .map(|p| {
            json!({
                "pack_id": p.pack_id,
                "pack": p.pack_name,
                "pack_engine": p.pack_engine,
                "release_choice": p.verdict,
            })
        })
        .collect();
    let bundle_id = slot.id().to_string();
    let mut manifest = Map::new();
    manifest.insert("bundle_id".into(), json!(bundle_id));
    manifest.insert("catalog_generation".into(), json!(job.catalog_generation));
    manifest.insert("created_at".into(), json!(rfc3339_unix(unix_now())));
    manifest.insert("project_engine_version".into(), json!(engine));
    manifest.insert(
        "engine_verdict".into(),
        json!({"verdict": verdict, "packs": packs}),
    );
    manifest.insert("omitted_deps".into(), json!(omitted));
    manifest.insert("external_missing".into(), json!(external));
    manifest.insert("warnings".into(), json!(warnings));
    let outcome = writer.finish(manifest).map_err(refusal)?;
    let published = slot.publish(&outcome).map_err(|_| {
        Refusal::new(
            "BUILD_FAILED",
            "the bundle could not be published on the server",
        )
    })?;
    let files: Vec<Value> = outcome
        .files
        .iter()
        .take(MAX_LIST)
        .map(|f| json!({"path": f.path, "size": f.size, "sha256": f.sha256}))
        .collect();
    Ok(json!({
        "bundle_id": bundle_id,
        "catalog_generation": job.catalog_generation,
        "size_bytes": outcome.size_bytes,
        "uncompressed_bytes": outcome.uncompressed_bytes,
        "bundle_sha256": outcome.sha256,
        "files": files,
        "files_total": outcome.files.len(),
        "verdict": verdict,
        "packs": packs,
        "omitted_deps": capped(&omitted),
        "omitted_deps_total": omitted.len(),
        "external_missing": capped(&external),
        "external_missing_total": external.len(),
        "warnings": warnings,
        "expires_at": published.expires_at,
        "download": format!("/bundles/{bundle_id}"),
    }))
}

/// Does `dir` lie inside (or equal) `parent`? Compared after resolving links; when one of them
/// cannot be resolved (an unreachable share) the lexical paths are compared instead.
pub fn is_inside(dir: &Path, parent: &Path) -> bool {
    match (fs::canonicalize(dir), fs::canonicalize(parent)) {
        (Ok(d), Ok(p)) => d.starts_with(p),
        _ => {
            let d = crate::import::norm_path(&dir.to_string_lossy());
            let p = crate::import::norm_path(&parent.to_string_lossy());
            d == p || d.starts_with(&format!("{}/", p.trim_end_matches('/')))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_32_lower_hex() {
        assert!(valid_id(&"a1".repeat(16)));
        assert!(!valid_id("abc"));
        assert!(!valid_id(&"A1".repeat(16)));
        assert!(!valid_id(&"g1".repeat(16)));
        assert!(!valid_id("../../../etc/passwd/../../../../x"));
    }

    #[test]
    fn rfc3339_known_values() {
        assert_eq!(rfc3339_unix(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_unix(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn verdict_is_the_worst_pack() {
        assert_eq!(overall_verdict(["exact", "older"].into_iter()), "older");
        assert_eq!(overall_verdict(["older", "newer"].into_iter()), "newer");
        assert_eq!(overall_verdict(std::iter::empty()), "exact");
    }
}
