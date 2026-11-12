use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use ue_asset_library::config::{run_all_once, JobRunner, SanitizerConfig};
use ue_asset_library::pipeline::Dirs;
use ue_asset_library::scan_library;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

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
            "ue_asset_library_cfg/{}-{}-{}",
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
    let mut v = vec![0xC1, 0x83, 0x2A, 0x9E, 0, 0, 0, 0];
    v.extend_from_slice(&((own.len() + 1) as u32).to_le_bytes());
    v.extend_from_slice(own.as_bytes());
    v.push(0);
    v
}

fn drop_zip(inbox: &Path, file: &str, folder: &str) {
    let mut w = ZipWriter::new(fs::File::create(inbox.join(file)).unwrap());
    let o = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    w.start_file(format!("{folder}/Meshes/M.uasset"), o)
        .unwrap();
    w.write_all(&fake_asset(&format!("/Game/{folder}/Meshes/M")))
        .unwrap();
    w.finish().unwrap();
}

fn job_json(import: &Path, library: &Path) -> String {
    format!(
        "{{\"import\":{},\"library\":{}}}",
        serde_json::to_string(&import.to_string_lossy()).unwrap(),
        serde_json::to_string(&library.to_string_lossy()).unwrap()
    )
}

#[test]
fn parses_full_config_and_defaults() {
    let full = r#"{"jobs":[{"import":"D:/a/import","library":"D:/a/library"},
                            {"import":"//srv/x/import","library":"//srv/x/library"}],
                   "poll_secs":10,"stable_secs":2,"stale_mins":60}"#;
    let c = SanitizerConfig::from_json(full).unwrap();
    assert_eq!(c.jobs.len(), 2);
    assert_eq!((c.poll_secs, c.stable_secs, c.stale_mins), (10, 2, 60));
    let min = SanitizerConfig::from_json(r#"{"jobs":[{"import":"a","library":"b"}]}"#).unwrap();
    assert_eq!(
        (min.poll_secs, min.stable_secs, min.stale_mins),
        (30, 5, 120)
    );
    let pc = c.pipeline_configs(true);
    assert_eq!(pc.len(), 2);
    assert!(pc[1].requeue && pc[1].stable_secs == 2 && pc[1].stale_mins == 60);
    // A UTF-8 BOM (Notepad) is tolerated.
    assert!(SanitizerConfig::from_json(
        "\u{feff}{\"jobs\":[{\"import\":\"a\",\"library\":\"b\"}]}"
    )
    .is_ok());
}

#[test]
fn rejects_bad_configs() {
    let bad = [
        "not json",
        r#"{"jobs":[]}"#,
        r#"{}"#,
        r#"{"jobs":[{"import":"a"}]}"#,
        r#"{"jobs":[{"import":"a","library":"a"}]}"#,
        r#"{"jobs":[{"import":"","library":"b"}]}"#,
        r#"{"jobs":[{"import":"a","library":"b"}],"pol_secs":5}"#,
        r#"{"jobs":[{"import":"a","library":"b","extra":1}]}"#,
        r#"{"jobs":[{"import":"a","library":"b"}],"poll_secs":0}"#,
    ];
    for b in bad {
        assert!(SanitizerConfig::from_json(b).is_err(), "should reject: {b}");
    }
}

#[test]
fn load_resolves_relative_paths_against_the_config_folder() {
    let t = Tmp::new();
    let dir = t.dir("cfg");
    let file = dir.join("sanitizer.json");
    fs::write(&file, r#"{"jobs":[{"import":"in","library":"lib"}]}"#).unwrap();
    let c = SanitizerConfig::load(&file).unwrap();
    assert_eq!(c.jobs[0].import, dir.join("in"));
    assert_eq!(c.jobs[0].library, dir.join("lib"));
    let err = SanitizerConfig::load(&dir.join("missing.json")).unwrap_err();
    assert!(err.contains("cannot read"), "{err}");
    fs::write(dir.join("broken.json"), "{").unwrap();
    assert!(SanitizerConfig::load(&dir.join("broken.json"))
        .unwrap_err()
        .contains("broken.json"));
}

#[test]
fn multiple_jobs_run_independently() {
    let t = Tmp::new();
    let (i1, l1) = (t.dir("one_import"), t.0.join("one_library"));
    let (i2, l2) = (t.dir("two_import"), t.0.join("two_library"));
    Dirs::new(&i1).create_all().unwrap();
    Dirs::new(&i2).create_all().unwrap();
    // Job one drops into the import root, job two into the legacy unprocessed/ folder.
    drop_zip(&i1, "Alpha 5.1.zip", "Alpha");
    fs::create_dir_all(&Dirs::new(&i2).unprocessed).unwrap();
    drop_zip(&Dirs::new(&i2).unprocessed, "Beta 5.2.zip", "Beta");
    let json = format!(
        "{{\"jobs\":[{},{}],\"stable_secs\":0}}",
        job_json(&i1, &l1),
        job_json(&i2, &l2)
    );
    let cfg = SanitizerConfig::from_json(&json).unwrap();
    let mut lines = Vec::new();
    let res = run_all_once(&cfg.pipeline_configs(false), &mut |l| {
        lines.push(l.to_string())
    });
    assert_eq!(res.len(), 2);
    assert!(
        res.iter()
            .all(|(_, r)| r.as_ref().unwrap().sanitized.len() == 1),
        "{lines:?}"
    );
    assert_eq!(
        scan_library(&l1)[0].sidecar.content_folder.as_deref(),
        Some("Alpha")
    );
    assert_eq!(
        scan_library(&l2)[0].sidecar.content_folder.as_deref(),
        Some("Beta")
    );
}

#[test]
fn bad_jobs_do_not_stop_the_others() {
    let t = Tmp::new();
    let good_i = t.dir("good_import");
    let good_l = t.0.join("good_library");
    Dirs::new(&good_i).create_all().unwrap();
    drop_zip(&good_i, "Gamma 5.3.zip", "Gamma");
    // Job 0: import folder does not exist (unreachable share). Job 2: library path is a file.
    let missing = t.0.join("no_such_import");
    let blocked_i = t.dir("blocked_import");
    Dirs::new(&blocked_i).create_all().unwrap();
    let blocked_l = t.0.join("library_is_a_file");
    fs::write(&blocked_l, b"x").unwrap();
    let json = format!(
        "{{\"jobs\":[{},{},{}],\"stable_secs\":0}}",
        job_json(&missing, &t.0.join("lib_a")),
        job_json(&good_i, &good_l),
        job_json(&blocked_i, &blocked_l),
    );
    let cfg = SanitizerConfig::from_json(&json).unwrap();
    let mut lines = Vec::new();
    let res = run_all_once(&cfg.pipeline_configs(false), &mut |l| {
        lines.push(l.to_string())
    });
    assert!(
        res[0].1.as_ref().unwrap_err().contains("unavailable"),
        "{:?}",
        res[0].1
    );
    assert_eq!(res[1].1.as_ref().unwrap().sanitized.len(), 1, "{lines:?}");
    assert!(res[2].1.is_err());
    assert_eq!(scan_library(&good_l).len(), 1);
    assert!(
        !missing.exists(),
        "an unreachable import folder must not be created"
    );
    assert!(lines.iter().any(|l| l.contains("job failed")), "{lines:?}");
}

#[test]
fn watch_tick_recovers_when_a_job_becomes_available() {
    let t = Tmp::new();
    let import = t.0.join("late_import");
    let mut cfg = ue_asset_library::pipeline::Config::new(&import, t.0.join("late_library"));
    cfg.stable_secs = 0;
    let mut job = JobRunner::new(cfg);
    let mut lines = Vec::new();
    assert!(job.tick(&mut |l| lines.push(l.to_string())).is_err());
    assert!(job.tick(&mut |l| lines.push(l.to_string())).is_err());
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.contains("job unavailable"))
            .count(),
        1,
        "logged once: {lines:?}"
    );
    fs::create_dir_all(&import).unwrap();
    drop_zip(&import, "Delta 5.4.zip", "Delta");
    let rep = job.tick(&mut |l| lines.push(l.to_string())).unwrap();
    assert_eq!(rep.sanitized.len(), 1, "{lines:?}");
    assert!(lines.iter().any(|l| l.contains("available again")));
}
