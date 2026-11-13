//! Non-zip inputs: `.rar`, `.7z` and multi-volume sets, converted into a hardened temp
//! zip that the normal sanitize pipeline then rewrites.
//!
//! * [`group_volumes`] groups the files of one inbox folder into volume sets and says
//!   whether each set is complete (a set with a missing volume waits).
//! * [`sanitize_archive`] hashes all volumes in order (that hash is the identity used for
//!   duplicate detection), converts the archive to a temp zip inside the library folder
//!   (same filesystem) and calls the zip sanitizer on it.
//!
//! 7z (and split 7z) is read with the pure-Rust `sevenz-rust2` and streamed into the temp
//! zip. RAR is read with the `unrar` crate (bundled C++ source): headers are listed and
//! validated first, then files are extracted to `<library>/.tmp-<id>/` under names we chose
//! ourselves, then zipped. A split zip (`.zip.001`) is simply concatenated.

use crate::error::{NeedsReview, SanitizeError};
use crate::harden::{check_paths, normalize_name, EntryInfo, MAX_ENTRIES, MAX_UNCOMPRESSED};
use crate::sanitize::{sanitize_zip_with, SanitizeOptions, SanitizeOutcome, SourceOverride};
use crate::util::unique_token;
use regex::Regex;
use sevenz_rust2::{ArchiveReader, Password};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

/// An entry that could not be extracted (damaged data, truncated archive, ...).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Skipped {
    pub path: String,
    pub error: String,
}

/// Result of converting an archive to a temp zip.
pub(crate) struct Converted {
    /// File entries in the archive.
    pub total: usize,
    pub skipped: Vec<Skipped>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ArchiveKind {
    Zip,
    /// `.zip.001` byte-split zip.
    SplitZip,
    SevenZ,
    Rar,
}

impl ArchiveKind {
    pub fn label(self) -> &'static str {
        match self {
            ArchiveKind::Zip => "zip",
            ArchiveKind::SplitZip => "split-zip",
            ArchiveKind::SevenZ => "7z",
            ArchiveKind::Rar => "rar",
        }
    }
}

struct Parsed {
    kind: ArchiveKind,
    /// Distinguishes naming schemes that share a base (`x.rar` + `x.r00` vs `x.part1.rar`).
    style: u8,
    base: String,
    /// 1-based volume number.
    vol: u32,
}

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).unwrap())
}

fn parse(name: &str) -> Option<Parsed> {
    static SEVENZ_SPLIT: OnceLock<Regex> = OnceLock::new();
    static ZIP_SPLIT: OnceLock<Regex> = OnceLock::new();
    static RAR_PART: OnceLock<Regex> = OnceLock::new();
    static RAR_OLD: OnceLock<Regex> = OnceLock::new();
    let num = |c: &regex::Captures, add: u32| c["n"].parse::<u32>().ok().map(|n| n + add);
    if let Some(c) = re(&SEVENZ_SPLIT, r"(?i)^(?P<b>.+)\.7z\.(?P<n>\d{3,})$").captures(name) {
        return Some(Parsed {
            kind: ArchiveKind::SevenZ,
            style: 1,
            base: c["b"].to_string(),
            vol: num(&c, 0)?,
        });
    }
    if let Some(c) = re(&ZIP_SPLIT, r"(?i)^(?P<b>.+)\.zip\.(?P<n>\d{3,})$").captures(name) {
        return Some(Parsed {
            kind: ArchiveKind::SplitZip,
            style: 1,
            base: c["b"].to_string(),
            vol: num(&c, 0)?,
        });
    }
    if let Some(c) = re(&RAR_PART, r"(?i)^(?P<b>.+)\.part(?P<n>\d+)\.rar$").captures(name) {
        return Some(Parsed {
            kind: ArchiveKind::Rar,
            style: 1,
            base: c["b"].to_string(),
            vol: num(&c, 0)?,
        });
    }
    if let Some(c) = re(&RAR_OLD, r"(?i)^(?P<b>.+)\.r(?P<n>\d{2,3})$").captures(name) {
        return Some(Parsed {
            kind: ArchiveKind::Rar,
            style: 0,
            base: c["b"].to_string(),
            vol: num(&c, 2)?,
        });
    }
    let lower = name.to_ascii_lowercase();
    for (ext, kind) in [
        (".rar", ArchiveKind::Rar),
        (".7z", ArchiveKind::SevenZ),
        (".zip", ArchiveKind::Zip),
    ] {
        if lower.ends_with(ext) && name.len() > ext.len() {
            return Some(Parsed {
                kind,
                style: 0,
                base: name[..name.len() - ext.len()].to_string(),
                vol: 1,
            });
        }
    }
    None
}

/// The archive kind implied by a file name, if it is a supported archive/volume name.
pub fn kind_of_name(name: &str) -> Option<ArchiveKind> {
    parse(name).map(|p| p.kind)
}

/// File stem used for name cleaning: the first volume's name without its archive suffixes.
/// Other names lose only their last extension (as plain zips always did).
pub fn source_stem(file_name: &str) -> String {
    if parse(file_name).is_some() {
        let s = crate::name::strip_archive_suffixes(file_name);
        if s != file_name.trim() {
            return s;
        }
    }
    file_name
        .rsplit_once('.')
        .map(|(s, _)| s.to_string())
        .unwrap_or_else(|| file_name.to_string())
}

/// A set of files that together form one archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeSet {
    pub kind: ArchiveKind,
    /// Display label (the first volume's name, or the lowest present name when it is missing).
    pub label: String,
    /// Volume file names ordered by volume number (the first volume first).
    pub volumes: Vec<String>,
    /// Volumes 1..=N are all present (gaps and a missing first volume make it incomplete).
    pub complete: bool,
}

impl VolumeSet {
    /// The entry-point file name.
    pub fn first(&self) -> &str {
        &self.volumes[0]
    }
}

/// Group the (file) names of one folder into volume sets; the second value lists names
/// that are not archives at all.
pub fn group_volumes(names: &[String]) -> (Vec<VolumeSet>, Vec<String>) {
    let mut groups: BTreeMap<(ArchiveKind, u8, String), Vec<(u32, String)>> = BTreeMap::new();
    let mut other = Vec::new();
    for n in names {
        match parse(n) {
            Some(p) => groups
                .entry((p.kind, p.style, p.base.to_lowercase()))
                .or_default()
                .push((p.vol, n.clone())),
            None => other.push(n.clone()),
        }
    }
    let mut sets = Vec::new();
    for ((kind, _, _), mut vols) in groups {
        vols.sort();
        let complete = vols
            .iter()
            .enumerate()
            .all(|(i, (v, _))| *v == i as u32 + 1);
        let names: Vec<String> = vols.into_iter().map(|(_, n)| n).collect();
        sets.push(VolumeSet {
            kind,
            label: names[0].clone(),
            volumes: names,
            complete,
        });
    }
    sets.sort_by(|a, b| a.label.cmp(&b.label));
    (sets, other)
}

/// Total size a 7z set must reach, read from the 32-byte start header of the first volume.
pub fn sevenz_expected_len(first: &Path) -> Option<u64> {
    let mut f = File::open(first).ok()?;
    let mut h = [0u8; 32];
    f.read_exact(&mut h).ok()?;
    if h[..6] != [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C] {
        return None;
    }
    let off = u64::from_le_bytes(h[12..20].try_into().ok()?);
    let len = u64::from_le_bytes(h[20..28].try_into().ok()?);
    32u64.checked_add(off)?.checked_add(len)
}

/// Read+Seek over several files laid end to end.
pub struct ChainReader<R: Read + Seek> {
    parts: Vec<(R, u64)>,
    starts: Vec<u64>,
    total: u64,
    pos: u64,
    /// (part, offset inside part) the underlying reader is currently positioned at.
    cursor: Option<(usize, u64)>,
}

impl<R: Read + Seek> ChainReader<R> {
    pub fn new(parts: Vec<(R, u64)>) -> Self {
        let mut starts = Vec::with_capacity(parts.len());
        let mut total = 0u64;
        for (_, len) in &parts {
            starts.push(total);
            total += len;
        }
        ChainReader {
            parts,
            starts,
            total,
            pos: 0,
            cursor: None,
        }
    }

    pub fn len(&self) -> u64 {
        self.total
    }

    pub fn is_empty(&self) -> bool {
        self.total == 0
    }
}

impl ChainReader<File> {
    pub fn open(paths: &[PathBuf]) -> io::Result<Self> {
        let mut parts = Vec::new();
        for p in paths {
            let f = File::open(p)?;
            let len = f.metadata()?.len();
            parts.push((f, len));
        }
        Ok(ChainReader::new(parts))
    }
}

impl<R: Read + Seek> Read for ChainReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.pos >= self.total {
            return Ok(0);
        }
        let i = self.starts.partition_point(|s| *s <= self.pos) - 1;
        let off = self.pos - self.starts[i];
        let remain = self.parts[i].1 - off;
        let want = buf.len().min(remain as usize);
        if self.cursor != Some((i, off)) {
            self.parts[i].0.seek(SeekFrom::Start(off))?;
        }
        let n = self.parts[i].0.read(&mut buf[..want])?;
        self.pos += n as u64;
        self.cursor = Some((i, off + n as u64));
        Ok(n)
    }
}

impl<R: Read + Seek> Seek for ChainReader<R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let np = match from {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::End(d) => self.total as i128 + d as i128,
            SeekFrom::Current(d) => self.pos as i128 + d as i128,
        };
        if np < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before start",
            ));
        }
        self.pos = np as u64;
        Ok(self.pos)
    }
}

/// SHA-256 over all volumes, in order.
pub fn sha256_files(paths: &[PathBuf]) -> io::Result<String> {
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    for p in paths {
        let mut f = File::open(p)?;
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn unsafe_(code: &str, detail: impl Into<String>) -> SanitizeError {
    SanitizeError::NeedsReview(NeedsReview::unsafe_entry(code, detail))
}

fn corrupt(detail: impl Into<String>) -> SanitizeError {
    unsafe_("ARCHIVE_CORRUPT", detail)
}

/// Entry admission shared by the 7z and RAR paths (same caps and name rules as zips).
#[derive(Default)]
struct Caps {
    infos: Vec<EntryInfo>,
    exact: HashSet<String>,
    declared: u64,
}

impl Caps {
    /// Validate one header; returns the normalised path.
    fn admit(&mut self, raw: &str, is_dir: bool, size: u64) -> Result<String, SanitizeError> {
        if self.infos.len() >= MAX_ENTRIES {
            return Err(unsafe_(
                "TOO_MANY_ENTRIES",
                format!("more than {MAX_ENTRIES} entries"),
            ));
        }
        let path = normalize_name(raw).map_err(SanitizeError::NeedsReview)?;
        self.declared = self.declared.saturating_add(size);
        if self.declared > MAX_UNCOMPRESSED {
            return Err(unsafe_(
                "TOO_BIG",
                format!("more than {MAX_UNCOMPRESSED} bytes uncompressed"),
            ));
        }
        if !self.exact.insert(path.to_lowercase()) {
            return Err(unsafe_("DUPLICATE", path));
        }
        self.infos.push(EntryInfo {
            index: self.infos.len(),
            path: path.clone(),
            is_dir,
            size,
        });
        Ok(path)
    }

    fn finish(&self) -> Result<(), SanitizeError> {
        check_paths(&self.infos).map_err(SanitizeError::NeedsReview)
    }
}

fn zip_opts() -> SimpleFileOptions {
    SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .compression_level(Some(1))
        .large_file(true)
}

fn finish_zip(zw: ZipWriter<BufWriter<File>>) -> Result<(), SanitizeError> {
    let mut out = zw.finish().map_err(|e| SanitizeError::Io(e.to_string()))?;
    out.flush()?;
    out.into_inner()
        .map_err(|e| SanitizeError::Io(e.to_string()))?
        .sync_all()?;
    Ok(())
}

// ---------------------------------------------------------------- 7z

fn map_7z(e: sevenz_rust2::Error) -> SanitizeError {
    use sevenz_rust2::Error as E;
    match e {
        E::PasswordRequired | E::MaybeBadPassword(_) => {
            unsafe_("ENCRYPTED", "password-protected 7z archive")
        }
        E::Io(io, _) | E::FileOpen(io, _) if io.kind() == io::ErrorKind::UnexpectedEof => {
            SanitizeError::Truncated(format!("MISSING_VOLUME or truncated 7z: {io}"))
        }
        E::Io(io, _) | E::FileOpen(io, _) => SanitizeError::Io(format!("7z read failed: {io}")),
        E::UnsupportedCompressionMethod(m) => unsafe_("UNSUPPORTED_METHOD", m),
        other => corrupt(other.to_string()),
    }
}

enum SevenPass {
    Done(Converted),
    /// Reading this entry failed; run again without it.
    DiedAt(String, String),
}

/// Convert a 7z set to a temp zip. A damaged entry is left out: the pass is repeated with that
/// entry (and the rest of its solid block) skipped, because a half-written entry cannot be
/// removed from the zip stream.
fn sevenz_to_zip(volumes: &[PathBuf], tmp_zip: &Path) -> Result<Converted, SanitizeError> {
    let mut bad: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for _ in 0..6 {
        match sevenz_pass(volumes, tmp_zip, &bad)? {
            SevenPass::Done(c) => return Ok(c),
            SevenPass::DiedAt(path, err) => {
                bad.insert(path, err);
            }
        }
    }
    Err(corrupt("too many damaged entries in the 7z archive"))
}

fn sevenz_pass(
    volumes: &[PathBuf],
    tmp_zip: &Path,
    bad: &std::collections::HashMap<String, String>,
) -> Result<SevenPass, SanitizeError> {
    use sevenz_rust2::EntryDecision;
    let chain = ChainReader::open(volumes)?;
    let mut ar = ArchiveReader::new(BufReader::with_capacity(1 << 20, chain), Password::empty())
        .map_err(map_7z)?;
    let all_files: Vec<String> = ar
        .archive()
        .files
        .iter()
        .filter(|f| !f.is_directory() && !f.is_anti_item())
        .map(|f| f.name().to_string())
        .collect();
    let mut zw = ZipWriter::new(BufWriter::with_capacity(1 << 20, File::create(tmp_zip)?));
    let mut caps = Caps::default();
    let mut written_total: u64 = 0;
    let mut rejected: Option<SanitizeError> = None;
    let mut died: Option<(String, String)> = None;
    let mut last_written: Option<String> = None;
    let mut visited: HashSet<String> = HashSet::new();
    let mut skipped: Vec<Skipped> = Vec::new();
    let res = ar.for_each_entries_with_decision(|entry, rd| {
        let mut step = || -> Result<EntryDecision, SanitizeError> {
            let a = entry.windows_attributes();
            let unix_mode = if a & 0x8000 != 0 { a >> 16 } else { 0 };
            if a & 0x400 != 0 || unix_mode & 0xF000 == 0xA000 {
                return Err(unsafe_("SYMLINK", entry.name()));
            }
            if entry.is_anti_item() {
                return Ok(EntryDecision::Continue);
            }
            let is_dir = entry.is_directory();
            let path = caps.admit(entry.name(), is_dir, entry.size())?;
            if is_dir {
                return Ok(EntryDecision::Continue);
            }
            visited.insert(entry.name().to_string());
            if let Some(why) = bad.get(&path) {
                skipped.push(Skipped {
                    path,
                    error: why.clone(),
                });
                // Abandon the rest of this compression block (not drained, not validated).
                return Ok(EntryDecision::SkipBlock);
            }
            zw.start_file(path.as_str(), zip_opts())
                .map_err(|e| SanitizeError::Io(format!("{path}: {e}")))?;
            let limit = MAX_UNCOMPRESSED.saturating_sub(written_total) + 1;
            match io::copy(&mut rd.take(limit), &mut zw) {
                Ok(n) => {
                    written_total += n;
                    if written_total > MAX_UNCOMPRESSED {
                        return Err(unsafe_("TOO_BIG", "7z entry larger than declared"));
                    }
                    last_written = Some(path);
                    Ok(EntryDecision::Continue)
                }
                Err(e) => {
                    died = Some((path, format!("read error: {e}")));
                    Ok(EntryDecision::Stop)
                }
            }
        };
        match step() {
            Ok(d) => Ok(d),
            Err(e) => {
                rejected = Some(e);
                Err(sevenz_rust2::Error::Other("rejected".into()))
            }
        }
    });
    if let Some(r) = rejected {
        return Err(r);
    }
    if let Some((p, e)) = died {
        return Ok(SevenPass::DiedAt(p, e));
    }
    if let Err(e) = res {
        use sevenz_rust2::Error as E;
        if matches!(e, E::PasswordRequired | E::MaybeBadPassword(_)) {
            return Err(map_7z(e));
        }
        // Integrity failure while draining the entry we just wrote (checksum, decoder).
        return match last_written {
            Some(p) => Ok(SevenPass::DiedAt(p, format!("damaged data: {e}"))),
            None => Err(map_7z(e)),
        };
    }
    // Entries never reached because their solid block was abandoned.
    for raw in &all_files {
        if !visited.contains(raw) {
            skipped.push(Skipped {
                path: normalize_name(raw).unwrap_or_else(|_| raw.clone()),
                error: "not extracted: an earlier entry in the same solid block is damaged"
                    .to_string(),
            });
        }
    }
    caps.finish()?;
    finish_zip(zw)?;
    Ok(SevenPass::Done(Converted {
        total: all_files.len(),
        skipped,
    }))
}

// ---------------------------------------------------------------- RAR

fn map_rar(e: unrar::error::UnrarError) -> SanitizeError {
    use unrar::error::{Code, When};
    match e.code {
        Code::MissingPassword | Code::BadPassword => {
            unsafe_("ENCRYPTED", "password-protected rar archive")
        }
        Code::EOpen if e.when != When::Open => {
            SanitizeError::Truncated(format!("MISSING_VOLUME: next rar volume missing ({e})"))
        }
        Code::EOpen | Code::NoMemory | Code::ECreate | Code::EClose | Code::EWrite => {
            SanitizeError::Io(format!("rar: {e}"))
        }
        Code::ERead => SanitizeError::Truncated(format!("rar read failed ({e})")),
        _ => corrupt(format!("rar: {e}")),
    }
}

/// Removes the extraction folder on every exit path.
struct TempDir(PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn slot_name(i: usize) -> String {
    format!("f{i:06}")
}

/// Per-entry extraction failures we tolerate (the rest of the archive can still be good).
fn rar_entry_failure(e: &unrar::error::UnrarError) -> bool {
    use unrar::error::Code;
    matches!(
        e.code,
        Code::BadData | Code::EOpen | Code::ERead | Code::BadArchive | Code::EReference
    )
}

fn rar_fatal(e: &unrar::error::UnrarError) -> bool {
    use unrar::error::Code;
    matches!(e.code, Code::MissingPassword | Code::BadPassword)
}

fn rar_to_zip(
    first: &Path,
    library_dir: &Path,
    tmp_zip: &Path,
) -> Result<Converted, SanitizeError> {
    // Pass 1: list and validate every header before anything is written.
    let mut caps = Caps::default();
    let mut files: Vec<String> = Vec::new();
    let mut slot_of: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut listing_error: Option<String> = None;
    for h in Archive::new(first).open_for_listing().map_err(map_rar)? {
        let h = match h {
            Ok(h) => h,
            // A damaged / cut-off tail: keep what was listed, the rest counts as failed.
            Err(e) if rar_entry_failure(&e) => {
                listing_error = Some(format!("{e}"));
                break;
            }
            Err(e) => return Err(map_rar(e)),
        };
        let raw = h.filename.to_string_lossy().to_string();
        if h.is_encrypted() {
            return Err(unsafe_("ENCRYPTED", raw));
        }
        if h.file_attr & 0xF000 == 0xA000 || h.file_attr & 0x400 != 0 {
            return Err(unsafe_("SYMLINK", raw));
        }
        let path = caps.admit(&raw, h.is_directory(), h.unpacked_size)?;
        if !h.is_directory() {
            slot_of.insert(path.clone(), files.len());
            files.push(path);
        }
    }
    caps.finish()?;

    // Pass 2: extract under short sequential names (`f000001`): unrar uses Win32 paths that
    // fail on >260 characters or non-ASCII names, so the real (validated) entry name is only
    // applied when the file is written into the output zip. A damaged entry is recorded and
    // skipped: the archive is reopened and extraction continues after it (non-solid), or the
    // remaining entries count as failed (solid: later entries depend on the broken one).
    let dir = TempDir(library_dir.join(format!(".tmp-{}", &unique_token()[..8])));
    fs::create_dir_all(&dir.0)?;
    let mut done = vec![false; files.len()];
    let mut failed: Vec<Option<String>> = vec![None; files.len()];
    let mut stop_reason = String::new();
    let mut actual: u64 = 0;
    let mut resume_from = 0usize; // header index to start extracting at
    loop {
        let ar = Archive::new(first).open_for_processing().map_err(map_rar)?;
        let solid = ar.is_solid();
        let mut cur = Some(ar);
        let mut i = 0usize;
        let mut stopped_at: Option<usize> = None;
        while let Some(a) = cur.take() {
            let h = match a.read_header() {
                Ok(Some(h)) => h,
                Ok(None) => break,
                Err(e) if rar_fatal(&e) => return Err(map_rar(e)),
                Err(e) => {
                    stop_reason = format!("header read failed: {e}");
                    break;
                }
            };
            if i < resume_from || h.entry().is_directory() {
                match h.skip() {
                    Ok(n) => cur = Some(n),
                    Err(e) if rar_fatal(&e) => return Err(map_rar(e)),
                    Err(e) => {
                        stop_reason = format!("skipping an entry failed: {e}");
                        break;
                    }
                }
                i += 1;
                continue;
            }
            let raw = h.entry().filename.to_string_lossy().to_string();
            let path = normalize_name(&raw).map_err(SanitizeError::NeedsReview)?;
            let Some(&slot) = slot_of.get(&path) else {
                if let Some(le) = &listing_error {
                    stop_reason = format!("entry after the damaged tail: {le}");
                    break;
                }
                return Err(corrupt(format!("{path}: entry not in the header listing")));
            };
            let dest = dir.0.join(slot_name(slot));
            match h.extract_to(&dest) {
                Ok(n) => {
                    cur = Some(n);
                    let md = fs::symlink_metadata(&dest)?;
                    if md.file_type().is_symlink() {
                        return Err(unsafe_("SYMLINK", path));
                    }
                    actual = actual.saturating_add(md.len());
                    if actual > MAX_UNCOMPRESSED {
                        return Err(unsafe_("TOO_BIG", "rar content larger than the cap"));
                    }
                    done[slot] = true;
                }
                Err(e) if rar_entry_failure(&e) => {
                    failed[slot] = Some(format!("{e}"));
                    stopped_at = Some(i);
                    if solid {
                        stop_reason = "solid archive cannot continue after a damaged entry".into();
                    }
                    break;
                }
                Err(e) => return Err(map_rar(e)),
            }
            i += 1;
        }
        match stopped_at {
            Some(k) if !solid => resume_from = k + 1,
            _ => break,
        }
    }
    if stop_reason.is_empty() {
        stop_reason = "not extracted (archive ended early)".into();
    }

    let mut zw = ZipWriter::new(BufWriter::with_capacity(1 << 20, File::create(tmp_zip)?));
    let mut skipped: Vec<Skipped> = Vec::new();
    for (slot, path) in files.iter().enumerate() {
        if !done[slot] {
            skipped.push(Skipped {
                path: path.clone(),
                error: failed[slot].clone().unwrap_or_else(|| stop_reason.clone()),
            });
            continue;
        }
        let src = dir.0.join(slot_name(slot));
        let md = fs::symlink_metadata(&src)
            .map_err(|_| corrupt(format!("{path}: not extracted (damaged rar?)")))?;
        if md.file_type().is_symlink() {
            return Err(unsafe_("SYMLINK", path.clone()));
        }
        zw.start_file(path.as_str(), zip_opts())
            .map_err(|e| SanitizeError::Io(format!("{path}: {e}")))?;
        let mut f = File::open(&src)?;
        io::copy(&mut f, &mut zw).map_err(|e| SanitizeError::Io(format!("{path}: {e}")))?;
    }
    finish_zip(zw)?;
    let mut total = files.len();
    if let Some(le) = listing_error {
        // Entries behind the damage are unknown: count them as one failed item.
        total += 1;
        skipped.push(Skipped {
            path: "(entries after the damaged end of the archive)".into(),
            error: le,
        });
    }
    Ok(Converted { total, skipped })
}

use unrar::Archive;

// ---------------------------------------------------------------- entry point

/// Convert the archive (all `volumes`, first volume first) into a temp zip inside
/// `library_dir`, sanitize that zip and remove it. The identity (SHA-256 over all volumes in
/// order, `source_zip` = first volume name) is that of the original archive.
pub fn sanitize_archive(
    kind: ArchiveKind,
    volumes: &[PathBuf],
    library_dir: &Path,
) -> Result<SanitizeOutcome, SanitizeError> {
    sanitize_archive_with(kind, volumes, library_dir, &SanitizeOptions::default())
}

/// [`sanitize_archive`] with explicit options (nested-archive version preference).
pub fn sanitize_archive_with(
    kind: ArchiveKind,
    volumes: &[PathBuf],
    library_dir: &Path,
    opts: &SanitizeOptions,
) -> Result<SanitizeOutcome, SanitizeError> {
    if volumes.is_empty() {
        return Err(SanitizeError::Io("archive set without volumes".into()));
    }
    fs::create_dir_all(library_dir)?;
    let names: Vec<String> = volumes
        .iter()
        .map(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
        })
        .collect();
    let sha = sha256_files(volumes)?;
    let sov = SourceOverride {
        name: names[0].clone(),
        sha256: sha,
        volumes: names,
        format: kind.label(),
        extra: serde_json::Map::new(),
        engine_version: None,
        skipped: Vec::new(),
        total: 0,
    };
    convert_and_sanitize(kind, volumes, library_dir, &sov, opts)
}

/// Convert `volumes` to a temp zip (unless already a zip) and sanitize it under `sov`.
pub(crate) fn convert_and_sanitize(
    kind: ArchiveKind,
    volumes: &[PathBuf],
    library_dir: &Path,
    sov: &SourceOverride,
    opts: &SanitizeOptions,
) -> Result<SanitizeOutcome, SanitizeError> {
    let first = &volumes[0];
    if kind == ArchiveKind::Zip {
        return sanitize_zip_with(first, library_dir, Some(sov), opts);
    }
    // Duplicate check before any extraction work.
    if let Some(existing) = crate::sanitize::find_duplicate_pub(library_dir, &sov.sha256[..12]) {
        return Ok(SanitizeOutcome::Duplicate { existing });
    }
    let tmp_zip = library_dir.join(format!(".tmp-{}.src.zip", unique_token()));
    let converted = match kind {
        ArchiveKind::SevenZ => sevenz_to_zip(volumes, &tmp_zip),
        ArchiveKind::Rar => rar_to_zip(first, library_dir, &tmp_zip),
        ArchiveKind::SplitZip => concat_to(volumes, &tmp_zip).map(|()| Converted {
            total: 0,
            skipped: Vec::new(),
        }),
        ArchiveKind::Zip => unreachable!(),
    };
    let result = converted.and_then(|conv| {
        let mut sov2 = sov.clone();
        if !conv.skipped.is_empty() {
            let ratio = extract_ratio(conv.total, conv.skipped.len());
            if ratio < opts.min_extract_success_ratio {
                return Err(corrupt_code(format!(
                    "{} of {} entries could not be extracted (ratio {:.3} < {:.3})",
                    conv.skipped.len(),
                    conv.total,
                    ratio,
                    opts.min_extract_success_ratio
                )));
            }
        }
        sov2.skipped.extend(conv.skipped);
        sov2.total = conv.total;
        sanitize_zip_with(&tmp_zip, library_dir, Some(&sov2), opts)
    });
    let _ = fs::remove_file(&tmp_zip);
    result
}

/// Share of entries that extracted fine.
pub(crate) fn extract_ratio(total: usize, skipped: usize) -> f64 {
    if total == 0 {
        1.0
    } else {
        1.0 - skipped.min(total) as f64 / total as f64
    }
}

fn corrupt_code(detail: String) -> SanitizeError {
    SanitizeError::Corrupt(format!("ARCHIVE_CORRUPT: {detail}"))
}

fn concat_to(volumes: &[PathBuf], out: &Path) -> Result<(), SanitizeError> {
    let mut w = BufWriter::with_capacity(1 << 20, File::create(out)?);
    for v in volumes {
        io::copy(&mut File::open(v)?, &mut w)?;
    }
    w.flush()?;
    w.into_inner()
        .map_err(|e| SanitizeError::Io(e.to_string()))?
        .sync_all()?;
    Ok(())
}

// `unrar_sys` calls Win32 registry/token/crypto functions without linking advapi32 (the
// windows-gnu toolchain does not link it by default, and a plain `-ladvapi32` lands before
// the rlibs on the link line). Declaring the imports `raw-dylib` here supplies them to
// every binary that uses this crate.
#[cfg(all(windows, target_env = "gnu"))]
#[link(name = "advapi32", kind = "raw-dylib")]
extern "system" {
    fn OpenProcessToken();
    fn LookupPrivilegeValueW();
    fn AdjustTokenPrivileges();
    fn SetFileSecurityW();
    fn RegOpenKeyExW();
    fn RegQueryValueExW();
    fn RegCloseKey();
    fn AllocateAndInitializeSid();
    fn CheckTokenMembership();
    fn FreeSid();
    fn CryptAcquireContextW();
    fn CryptGenRandom();
    fn CryptReleaseContext();
}

/// Keeps the raw-dylib imports above alive (never called).
#[cfg(all(windows, target_env = "gnu"))]
#[used]
static ADVAPI32_IMPORTS: [unsafe extern "system" fn(); 13] = [
    OpenProcessToken,
    LookupPrivilegeValueW,
    AdjustTokenPrivileges,
    SetFileSecurityW,
    RegOpenKeyExW,
    RegQueryValueExW,
    RegCloseKey,
    AllocateAndInitializeSid,
    CheckTokenMembership,
    FreeSid,
    CryptAcquireContextW,
    CryptGenRandom,
    CryptReleaseContext,
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn n(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn groups_and_completeness() {
        let (sets, other) = group_volumes(&n(&[
            "A (5.0 \u{2013} 5.8).rar",
            "B[5.0-5.3].7z.001",
            "B[5.0-5.3].7z.002",
            "C.7z.001",
            "C.7z.003",
            "D.part1.rar",
            "D.part2.rar",
            "E.rar",
            "E.r00",
            "E.r01",
            "F.r00",
            "G.zip",
            "H.zip.001",
            "H.zip.002",
            "x.uasset",
        ]));
        assert_eq!(other, n(&["x.uasset"]));
        let get = |l: &str| sets.iter().find(|s| s.label == l).unwrap();
        let a = get("A (5.0 \u{2013} 5.8).rar");
        assert!(a.complete && a.volumes.len() == 1 && a.kind == ArchiveKind::Rar);
        let b = get("B[5.0-5.3].7z.001");
        assert!(b.complete);
        assert_eq!(b.volumes, n(&["B[5.0-5.3].7z.001", "B[5.0-5.3].7z.002"]));
        assert_eq!(b.first(), "B[5.0-5.3].7z.001");
        assert!(!get("C.7z.001").complete);
        let d = get("D.part1.rar");
        assert!(d.complete && d.volumes.len() == 2);
        let e = get("E.rar");
        assert!(e.complete);
        assert_eq!(e.volumes, n(&["E.rar", "E.r00", "E.r01"]));
        assert!(!get("F.r00").complete);
        assert_eq!(get("G.zip").kind, ArchiveKind::Zip);
        let h = get("H.zip.001");
        assert!(h.complete && h.kind == ArchiveKind::SplitZip && h.volumes.len() == 2);
    }

    #[test]
    fn extraction_ratio() {
        assert_eq!(extract_ratio(0, 0), 1.0);
        assert_eq!(extract_ratio(1117, 0), 1.0);
        assert!(extract_ratio(1117, 1) > 0.95);
        assert!(extract_ratio(100, 6) < 0.95);
        assert_eq!(extract_ratio(10, 20), 0.0);
    }

    #[test]
    fn stems() {
        assert_eq!(
            source_stem("Post[5.0-5.3].7z.001"),
            "Post[5.0-5.3]".to_string()
        );
        assert_eq!(source_stem("Pack 5.5.zip"), "Pack 5.5");
        assert_eq!(source_stem("Big.part01.rar"), "Big");
        assert_eq!(source_stem("odd.name.zip"), "odd.name");
    }

    #[test]
    fn chain_reader_reads_across_parts() {
        let data: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        let parts = vec![
            (Cursor::new(data[..300].to_vec()), 300),
            (Cursor::new(data[300..301].to_vec()), 1),
            (Cursor::new(data[301..].to_vec()), 699),
        ];
        let mut r = ChainReader::new(parts);
        assert_eq!(r.len(), 1000);
        let mut all = Vec::new();
        r.read_to_end(&mut all).unwrap();
        assert_eq!(all, data);
        r.seek(SeekFrom::Start(295)).unwrap();
        let mut b = [0u8; 10];
        r.read_exact(&mut b).unwrap();
        assert_eq!(&b[..], &data[295..305]);
        r.seek(SeekFrom::End(-5)).unwrap();
        let mut t = Vec::new();
        r.read_to_end(&mut t).unwrap();
        assert_eq!(t, data[995..]);
        r.seek(SeekFrom::Start(0)).unwrap();
        r.seek(SeekFrom::Current(-1)).unwrap_err();
    }

    #[test]
    fn sevenz_header_expected_len() {
        let dir = std::env::temp_dir().join(format!("ual-7z-hdr-{}", unique_token()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.7z.001");
        let mut h = vec![0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C, 0, 4, 0, 0, 0, 0];
        h.extend_from_slice(&100u64.to_le_bytes());
        h.extend_from_slice(&20u64.to_le_bytes());
        h.extend_from_slice(&[0; 4]);
        fs::write(&p, &h).unwrap();
        assert_eq!(sevenz_expected_len(&p), Some(152));
        let _ = fs::remove_dir_all(&dir);
    }
}
