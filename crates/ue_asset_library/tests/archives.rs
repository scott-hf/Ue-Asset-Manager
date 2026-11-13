//! `.7z`, `.rar` and multi-volume inputs (fixtures are generated in-process).

use sevenz_rust2::{ArchiveEntry, ArchiveWriter};
use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use ue_asset_library::pipeline::{reconcile, run_once, Config, Dirs};
use ue_asset_library::{
    sanitize_archive, ArchiveKind, Kind, NeedsReview, SanitizeError, SanitizeOutcome,
};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

static N: AtomicU64 = AtomicU64::new(0);

struct Tmp(PathBuf);
impl Tmp {
    fn new() -> Tmp {
        let n = N.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!(
            "ue_asset_library_arch/{}-{}-{}",
            std::process::id(),
            nanos,
            n
        ));
        fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }
    fn dir(&self, name: &str) -> PathBuf {
        let p = self.0.join(name);
        fs::create_dir_all(&p).unwrap();
        p
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fake_asset(own: &str) -> Vec<u8> {
    let mut v = vec![0xC1, 0x83, 0x2A, 0x9E, 0, 0, 0, 0, 7, 7];
    v.extend_from_slice(&((own.len() + 1) as u32).to_le_bytes());
    v.extend_from_slice(own.as_bytes());
    v.push(0);
    v.extend_from_slice(&[0x12, 0x34]);
    v
}

fn room_entries() -> Vec<(String, Vec<u8>)> {
    vec![
        (
            "OperatingRoomPack/Levels/Operating_Room.umap".into(),
            fake_asset("/Game/OperatingRoom/Levels/Operating_Room"),
        ),
        (
            "OperatingRoomPack/Materials/M_Master.uasset".into(),
            fake_asset("/Game/OperatingRoom/Materials/M_Master"),
        ),
        (
            "OperatingRoomPack/Materials/M_Master.uexp".into(),
            vec![1, 2, 3],
        ),
        ("OperatingRoomPack/readme.txt".into(), b"hi".to_vec()),
    ]
}

fn make_7z(path: &Path, entries: &[(String, Vec<u8>)]) {
    let mut w = ArchiveWriter::create(path).unwrap();
    for (name, data) in entries {
        w.push_archive_entry(
            ArchiveEntry::new_file(name),
            Some(Cursor::new(data.clone())),
        )
        .unwrap();
    }
    w.finish().unwrap();
}

fn split(path: &Path, base: &str, ext: &str, parts: usize) -> Vec<PathBuf> {
    let data = fs::read(path).unwrap();
    let chunk = data.len().div_ceil(parts);
    let dir = path.parent().unwrap();
    let mut out = Vec::new();
    for (i, c) in data.chunks(chunk).enumerate() {
        let p = dir.join(format!("{base}.{ext}.{:03}", i + 1));
        fs::write(&p, c).unwrap();
        out.push(p);
    }
    out
}

fn sanitized(r: Result<SanitizeOutcome, SanitizeError>) -> (PathBuf, ue_asset_library::Sidecar) {
    match r.expect("sanitize") {
        SanitizeOutcome::Sanitized {
            library_zip,
            sidecar,
            ..
        } => (library_zip, sidecar),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn sevenz_single_becomes_library_zip_with_range() {
    let t = Tmp::new();
    let src = t
        .dir("src")
        .join("Operating Room Pack (5.0 \u{2013} 5.8).7z");
    make_7z(&src, &room_entries());
    let lib = t.dir("lib");
    let (zip, sc) = sanitized(sanitize_archive(
        ArchiveKind::SevenZ,
        std::slice::from_ref(&src),
        &lib,
    ));
    assert_eq!(sc.kind, Kind::Content);
    assert_eq!(sc.display_name, "Operating Room Pack");
    assert_eq!(sc.engine_version_guess.as_deref(), Some("5.0"));
    assert_eq!(sc.extra["engine_version_min"], "5.0");
    assert_eq!(sc.extra["engine_version_max"], "5.8");
    assert_eq!(sc.extra["source_format"], "7z");
    assert_eq!(sc.source_zip, "Operating Room Pack (5.0 \u{2013} 5.8).7z");
    let sha = ue_asset_library::archive::sha256_files(std::slice::from_ref(&src)).unwrap();
    assert_eq!(sc.sha256, sha);
    assert!(zip
        .file_name()
        .unwrap()
        .to_string_lossy()
        .ends_with(&format!(".{}.zip", &sha[..12])));
    let z = ZipArchive::new(fs::File::open(&zip).unwrap()).unwrap();
    assert!(z
        .file_names()
        .any(|n| n.starts_with("Content/OperatingRoom/")));
    // Same archive again: duplicate, no temp files left behind.
    match sanitize_archive(ArchiveKind::SevenZ, &[src], &lib).unwrap() {
        SanitizeOutcome::Duplicate { .. } => {}
        o => panic!("{o:?}"),
    }
    let leftovers: Vec<_> = fs::read_dir(&lib)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn sevenz_multi_volume_hash_covers_all_volumes() {
    let t = Tmp::new();
    let one = t.dir("src").join("whole.7z");
    make_7z(&one, &room_entries());
    let vols = split(
        &one,
        "Post_Apocalyptic_NYC_Environment_Megapack[5.0-5.3]",
        "7z",
        2,
    );
    let lib = t.dir("lib");
    let (_zip, sc) = sanitized(sanitize_archive(ArchiveKind::SevenZ, &vols, &lib));
    assert_eq!(sc.display_name, "Post_Apocalyptic_NYC_Environment_Megapack");
    assert_eq!(
        sc.source_zip,
        "Post_Apocalyptic_NYC_Environment_Megapack[5.0-5.3].7z.001"
    );
    assert_eq!(sc.extra["source_volumes"].as_array().unwrap().len(), 2);
    assert_eq!(
        sc.sha256,
        ue_asset_library::archive::sha256_files(&vols).unwrap()
    );
    assert_eq!(sc.engine_version_guess.as_deref(), Some("5.0"));
}

#[test]
fn sevenz_rejects_path_traversal() {
    let t = Tmp::new();
    let src = t.dir("src").join("evil.7z");
    let mut e = room_entries();
    e.push(("../evil.uasset".into(), vec![1]));
    make_7z(&src, &e);
    let lib = t.dir("lib");
    match sanitize_archive(ArchiveKind::SevenZ, &[src], &lib) {
        Err(SanitizeError::NeedsReview(NeedsReview::Unsafe { code, .. })) => {
            assert_eq!(code, "SLIP")
        }
        other => panic!("{other:?}"),
    }
    assert!(fs::read_dir(&lib)
        .unwrap()
        .flatten()
        .all(|e| !e.file_name().to_string_lossy().ends_with(".zip.json")));
}

#[test]
fn garbage_archives_go_to_review_not_panic() {
    let t = Tmp::new();
    let lib = t.dir("lib");
    for (name, kind) in [
        ("junk.7z", ArchiveKind::SevenZ),
        ("junk.rar", ArchiveKind::Rar),
    ] {
        let p = t.dir("src").join(name);
        fs::write(&p, vec![0x55u8; 4096]).unwrap();
        match sanitize_archive(kind, &[p], &lib) {
            Err(SanitizeError::NeedsReview(n)) => assert_eq!(n.code(), "ARCHIVE_CORRUPT", "{name}"),
            other => panic!("{name}: {other:?}"),
        }
    }
}

#[test]
fn split_zip_is_concatenated() {
    let t = Tmp::new();
    let whole = t.dir("src").join("whole.zip");
    {
        let mut w = ZipWriter::new(fs::File::create(&whole).unwrap());
        let o = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        for (n, d) in room_entries() {
            w.start_file(n, o).unwrap();
            w.write_all(&d).unwrap();
        }
        w.finish().unwrap();
    }
    let vols = split(&whole, "Room Pack 5.4", "zip", 3);
    let lib = t.dir("lib");
    let (_z, sc) = sanitized(sanitize_archive(ArchiveKind::SplitZip, &vols, &lib));
    assert_eq!(sc.display_name, "Room Pack");
    assert_eq!(sc.engine_version_guess.as_deref(), Some("5.4"));
    assert_eq!(sc.source_zip, "Room Pack 5.4.zip.001");
}

fn cfg(import: &Path, lib: &Path) -> Config {
    let mut c = Config::new(import, lib);
    c.stable_secs = 0;
    c
}

#[test]
fn pipeline_waits_for_missing_volume_then_moves_set_together() {
    let t = Tmp::new();
    let import = t.dir("import");
    let lib = t.dir("library");
    let whole = t.dir("tmp").join("w.7z");
    make_7z(&whole, &room_entries());
    let vols = split(&whole, "Pack[5.0-5.3]", "7z", 3);
    let dirs = Dirs::new(&import);
    dirs.create_all().unwrap();
    // Volume 2 missing: nothing may be claimed or moved.
    fs::copy(&vols[0], import.join(vols[0].file_name().unwrap())).unwrap();
    fs::copy(&vols[2], import.join(vols[2].file_name().unwrap())).unwrap();
    let rep = run_once(&cfg(&import, &lib), &mut |_| {});
    assert_eq!(rep.waiting, vec!["Pack[5.0-5.3].7z.001".to_string()]);
    assert!(rep.sanitized.is_empty() && rep.failed.is_empty() && rep.needs_review.is_empty());
    assert!(import.join("Pack[5.0-5.3].7z.001").is_file());
    assert!(import.join("Pack[5.0-5.3].7z.003").is_file());
    assert_eq!(fs::read_dir(&dirs.processing).unwrap().count(), 0);

    // Last volume missing (set 001 + 002 of 3): the 7z header says it is too short.
    fs::remove_file(import.join("Pack[5.0-5.3].7z.003")).unwrap();
    fs::copy(&vols[1], import.join(vols[1].file_name().unwrap())).unwrap();
    let rep = run_once(&cfg(&import, &lib), &mut |_| {});
    assert!(rep.sanitized.is_empty() && rep.failed.is_empty(), "{rep:?}");
    assert!(!rep.waiting.is_empty());

    // Everything present: processed as a unit.
    fs::copy(&vols[2], import.join(vols[2].file_name().unwrap())).unwrap();
    let mut lines = Vec::new();
    let rep = run_once(&cfg(&import, &lib), &mut |l| lines.push(l.to_string()));
    assert_eq!(
        rep.sanitized,
        vec!["Pack[5.0-5.3].7z.001".to_string()],
        "{lines:?}"
    );
    for i in 1..=3 {
        let n = format!("Pack[5.0-5.3].7z.00{i}");
        assert!(dirs.processed.join(&n).is_file(), "{n}");
        assert!(!import.join(&n).exists() && !dirs.processing.join(&n).exists());
    }
    assert_eq!(
        fs::read_dir(&dirs.processing).unwrap().count(),
        0,
        "claim removed"
    );
    assert!(reconcile(&cfg(&import, &lib)).is_empty());
}

#[test]
fn pipeline_review_moves_all_volumes() {
    let t = Tmp::new();
    let import = t.dir("import");
    let lib = t.dir("library");
    let whole = t.dir("tmp").join("w.7z");
    let mut e = room_entries();
    e.push(("../x".into(), vec![1]));
    make_7z(&whole, &e);
    let vols = split(&whole, "Bad", "7z", 2);
    let dirs = Dirs::new(&import);
    dirs.create_all().unwrap();
    for v in &vols {
        fs::copy(v, import.join(v.file_name().unwrap())).unwrap();
    }
    let rep = run_once(&cfg(&import, &lib), &mut |_| {});
    assert_eq!(rep.needs_review, vec!["Bad.7z.001".to_string()]);
    assert!(dirs.needs_review.join("Bad.7z.001").is_file());
    assert!(dirs.needs_review.join("Bad.7z.002").is_file());
    assert!(dirs.needs_review.join("Bad.7z.001.review.json").is_file());
}

// ---------- partial extraction tolerance and retry cap ----------

fn pad_entries(n: usize) -> Vec<(String, Vec<u8>)> {
    let mut seed = 0x1234_5678u32;
    (0..n)
        .map(|i| {
            let data: Vec<u8> = (0..2000)
                .map(|_| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (seed >> 24) as u8
                })
                .collect();
            (format!("OperatingRoomPack/Materials/pad_{i:03}.bin"), data)
        })
        .collect()
}

fn damaged_7z(path: &Path) {
    let mut e = room_entries();
    e.extend(pad_entries(60));
    make_7z(path, &e);
    let mut bytes = fs::read(path).unwrap();
    let pos = 32 + (bytes.len() - 32) / 3;
    for b in &mut bytes[pos..pos + 16] {
        *b ^= 0xFF;
    }
    fs::write(path, bytes).unwrap();
}

#[test]
fn damaged_7z_entry_is_skipped_and_recorded() {
    let t = Tmp::new();
    let src = t.dir("src").join("Damaged Pack 5.1.7z");
    damaged_7z(&src);
    let lib = t.dir("lib");
    let (zip, sc) = sanitized(sanitize_archive(
        ArchiveKind::SevenZ,
        std::slice::from_ref(&src),
        &lib,
    ));
    let skipped = sc.extra["skipped_entries"].as_array().unwrap();
    assert!(!skipped.is_empty() && skipped.len() <= 3, "{skipped:?}");
    assert!(sc.extra["extract_success_ratio"].as_f64().unwrap() >= 0.95);
    let z = ZipArchive::new(fs::File::open(&zip).unwrap()).unwrap();
    for s in skipped {
        let p = s["path"].as_str().unwrap();
        assert!(!s["error"].as_str().unwrap().is_empty());
        assert!(
            !z.file_names()
                .any(|n| n.ends_with(p.rsplit('/').next().unwrap())),
            "{p} must not be in the library zip"
        );
    }
    assert_eq!(sc.kind, Kind::Content);
}

#[test]
fn damaged_7z_below_ratio_fails_as_archive_corrupt() {
    use ue_asset_library::SanitizeOptions;
    let t = Tmp::new();
    let src = t.dir("src").join("Damaged Pack 5.1.7z");
    damaged_7z(&src);
    let opts = SanitizeOptions::default().with_min_extract_success_ratio(0.999);
    match ue_asset_library::sanitize_archive_with(
        ArchiveKind::SevenZ,
        std::slice::from_ref(&src),
        &t.dir("lib"),
        &opts,
    ) {
        Err(SanitizeError::Corrupt(m)) => assert!(m.starts_with("ARCHIVE_CORRUPT"), "{m}"),
        o => panic!("{o:?}"),
    }
}

#[test]
fn retryable_errors_are_capped_with_a_persistent_counter() {
    let t = Tmp::new();
    let import = t.dir("import");
    let lib = t.dir("library");
    let mut cfg = Config::new(&import, &lib);
    cfg.stable_secs = 0;
    cfg.max_retries = 3;
    cfg.retry_min_age_secs = 0;
    let dirs = Dirs::new(&import);
    dirs.create_all().unwrap();
    fs::write(import.join("Cut 5.0.zip"), b"PK\x03\x04 truncated upload").unwrap();
    let mut lines = Vec::new();
    for n in 1..=2 {
        let rep = run_once(&cfg, &mut |l| lines.push(l.to_string()));
        assert_eq!(
            rep.deferred,
            vec!["Cut 5.0.zip".to_string()],
            "pass {n}: {lines:?}"
        );
        assert!(import.join("Cut 5.0.zip").is_file());
        let at = fs::read_to_string(dirs.processing.join("Cut 5.0.zip.attempts")).unwrap();
        assert!(at.contains(&format!("\"count\":{n}")), "{at}");
        assert!(
            lines.iter().any(|l| l.contains(&format!("attempt {n}/3"))),
            "real attempt number in the log: {lines:?}"
        );
    }
    let rep = run_once(&cfg, &mut |l| lines.push(l.to_string()));
    assert_eq!(rep.failed, vec!["Cut 5.0.zip".to_string()], "{lines:?}");
    assert!(dirs.failed.join("Cut 5.0.zip").is_file());
    let review = fs::read_to_string(dirs.failed.join("Cut 5.0.zip.review.json")).unwrap();
    assert!(review.contains("RETRIES_EXCEEDED"), "{review}");
    assert!(!dirs.processing.join("Cut 5.0.zip.attempts").exists());
}

#[test]
fn retry_cap_waits_for_the_minimum_age() {
    let t = Tmp::new();
    let import = t.dir("import");
    let lib = t.dir("library");
    let mut cfg = Config::new(&import, &lib);
    cfg.stable_secs = 0;
    cfg.max_retries = 2;
    cfg.retry_min_age_secs = 3600;
    Dirs::new(&import).create_all().unwrap();
    fs::write(import.join("Slow 5.0.zip"), b"PK\x03\x04 partial").unwrap();
    for _ in 0..4 {
        let rep = run_once(&cfg, &mut |_| {});
        assert!(rep.failed.is_empty(), "{rep:?}");
        assert_eq!(rep.deferred.len(), 1);
    }
}

#[test]
fn busy_io_errors_do_not_count_as_attempts() {
    let busy = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
    assert!(matches!(SanitizeError::from(busy), SanitizeError::Busy(_)));
    let other = std::io::Error::from(std::io::ErrorKind::UnexpectedEof);
    assert!(matches!(SanitizeError::from(other), SanitizeError::Io(_)));
}
