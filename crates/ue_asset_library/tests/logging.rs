//! Rotating log, log config parsing and the sanitizer binary's `--log-file`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use ue_asset_library::config::SanitizerConfig;
use ue_asset_library::rotating_log::{level_of, Level, Options, RotatingLog};

static N: AtomicU64 = AtomicU64::new(0);

struct Tmp(PathBuf);
impl Tmp {
    fn new() -> Tmp {
        let n = N.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!(
            "ue_asset_library_tests/log-{}-{}",
            std::process::id(),
            n
        ));
        fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn opts(max_bytes: u64, keep: u32) -> Options {
    Options {
        max_bytes,
        keep,
        ..Options::default()
    }
}

fn total_size(dir: &Path) -> u64 {
    fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.metadata().unwrap().len())
        .sum()
}

fn all_text(dir: &Path) -> String {
    let mut s = String::new();
    for e in fs::read_dir(dir).unwrap().flatten() {
        s.push_str(&fs::read_to_string(e.path()).unwrap());
    }
    s
}

#[test]
fn rotation_respects_the_cap_and_drops_the_oldest() {
    let t = Tmp::new();
    let path = t.path().join("logs/sanitizer.log"); // parent is created
    let o = opts(1000, 3);
    let log = RotatingLog::open(&path, o.clone());
    for i in 0..300 {
        log.info(&format!("line {i:04} {}", "x".repeat(60)));
    }
    let dir = path.parent().unwrap();
    let names: Vec<String> = {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        v.sort();
        v
    };
    assert_eq!(
        names,
        vec![
            "sanitizer.log",
            "sanitizer.log.1",
            "sanitizer.log.2",
            "sanitizer.log.3"
        ]
    );
    assert!(
        total_size(dir) <= o.worst_case_bytes(),
        "{}",
        total_size(dir)
    );
    for e in fs::read_dir(dir).unwrap().flatten() {
        assert!(e.metadata().unwrap().len() <= o.max_bytes);
    }
    let text = all_text(dir);
    assert!(text.contains("line 0299"), "newest line kept");
    assert!(!text.contains("line 0000"), "oldest line dropped");
    // Every line is whole and carries timestamp + level.
    for l in text.lines() {
        assert!(
            l.len() > 25 && l.as_bytes()[4] == b'-' && l.contains(" INFO  line "),
            "{l}"
        );
    }
    assert_eq!(o.worst_case_bytes(), 4000);
}

#[test]
fn keep_zero_still_holds_the_cap() {
    let t = Tmp::new();
    let path = t.path().join("a.log");
    let log = RotatingLog::open(&path, opts(500, 0));
    for i in 0..200 {
        log.warn(&format!("w {i} {}", "y".repeat(40)));
    }
    assert!(total_size(t.path()) <= 500);
    assert_eq!(fs::read_dir(t.path()).unwrap().count(), 1);
}

#[test]
fn old_and_excess_rotated_files_are_cleaned_up() {
    let t = Tmp::new();
    let path = t.path().join("s.log");
    let old = SystemTime::now() - Duration::from_secs(40 * 86_400);
    for n in [
        "s.log.1",
        "s.log.2",
        "s.log.3",
        "s.log.9",
        "s.log.bak",
        "other.log.1",
    ] {
        fs::write(t.path().join(n), b"x").unwrap();
    }
    for n in ["s.log.1", "s.log.9", "s.log.bak", "other.log.1"] {
        fs::File::options()
            .write(true)
            .open(t.path().join(n))
            .unwrap()
            .set_modified(old)
            .unwrap();
    }
    let _log = RotatingLog::open(&path, opts(1000, 5));
    assert!(!t.path().join("s.log.1").exists(), "too old");
    assert!(!t.path().join("s.log.9").exists(), "beyond keep");
    assert!(t.path().join("s.log.2").exists() && t.path().join("s.log.3").exists());
    assert!(t.path().join("s.log.bak").exists(), "not a rotated file");
    assert!(t.path().join("other.log.1").exists(), "another log's file");
}

#[test]
fn long_lines_are_capped_and_single_line() {
    let t = Tmp::new();
    let path = t.path().join("l.log");
    let log = RotatingLog::open(&path, Options::default());
    let hostile = format!("bad\r\nname\u{0}{}\u{e9}", "A".repeat(100_000));
    log.error(&hostile);
    log.info("after");
    let text = fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text:?}");
    assert!(lines[0].len() < 4096 + 64, "{}", lines[0].len());
    assert!(lines[0].contains("ERROR bad  name") && lines[0].ends_with("[truncated]"));
    assert!(lines[1].ends_with("INFO  after"));
    // Multi-byte characters are never cut in half.
    let log2 = RotatingLog::open(
        t.path().join("m.log"),
        Options {
            max_line: 5,
            ..Options::default()
        },
    );
    log2.info("\u{e9}\u{e9}\u{e9}\u{e9}");
    assert!(fs::read_to_string(t.path().join("m.log"))
        .unwrap()
        .contains("\u{e9}\u{e9}...[truncated]"));
}

#[test]
fn unwritable_path_falls_back_without_panicking() {
    let t = Tmp::new();
    // The parent "directory" is a file: the log can never be opened.
    fs::write(t.path().join("blocker"), b"x").unwrap();
    let log = RotatingLog::open(t.path().join("blocker/child.log"), Options::default());
    for i in 0..5 {
        log.info(&format!("still fine {i}"));
    }
    log.cleanup_rotated();
    // A directory where the file should be.
    fs::create_dir(t.path().join("dir.log")).unwrap();
    let log = RotatingLog::open(t.path().join("dir.log"), Options::default());
    log.error("no file, no panic");
    // Recovers once the problem is gone (the retry delay is only for repeated failures; a fresh
    // logger on a good path works immediately).
    let good = RotatingLog::open(t.path().join("ok.log"), Options::default());
    good.info("ok");
    assert!(fs::read_to_string(t.path().join("ok.log"))
        .unwrap()
        .contains("ok"));
}

#[test]
fn concurrent_writers_keep_every_line_intact() {
    let t = Tmp::new();
    let path = t.path().join("c.log");
    let log = Arc::new(RotatingLog::open(&path, opts(10 * 1024 * 1024, 2)));
    let mut hs = Vec::new();
    for th in 0..8 {
        let log = log.clone();
        hs.push(std::thread::spawn(move || {
            for i in 0..200 {
                log.info(&format!("thread {th} line {i} {}", "z".repeat(30)));
            }
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
    let text = fs::read_to_string(&path).unwrap();
    assert_eq!(text.lines().count(), 1600);
    assert!(text
        .lines()
        .all(|l| l.contains(" INFO  thread ") && l.ends_with(&"z".repeat(30))));
}

#[test]
fn concurrent_writers_with_rotation_stay_under_the_cap() {
    let t = Tmp::new();
    let path = t.path().join("r.log");
    let o = opts(2000, 2);
    let log = Arc::new(RotatingLog::open(&path, o.clone()));
    let mut hs = Vec::new();
    for th in 0..6 {
        let log = log.clone();
        hs.push(std::thread::spawn(move || {
            for i in 0..150 {
                log.warn(&format!("t{th} {i} {}", "q".repeat(50)));
            }
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
    assert!(total_size(t.path()) <= o.worst_case_bytes());
}

#[test]
fn two_handles_on_one_file_both_append() {
    // An old instance still exiting while the new one starts.
    let t = Tmp::new();
    let path = t.path().join("two.log");
    let a = RotatingLog::open(&path, Options::default());
    let b = RotatingLog::open(&path, Options::default());
    a.info("from a");
    b.info("from b");
    a.info("from a again");
    let text = fs::read_to_string(&path).unwrap();
    assert_eq!(text.lines().count(), 3, "{text}");
}

#[test]
fn levels_are_guessed_from_pipeline_messages() {
    assert_eq!(
        level_of("x.zip: sanitized -> y (Content, 3 entries, 0.5s)"),
        Level::Info
    );
    assert_eq!(
        level_of("WARN: x.zip: asset index not written: boom"),
        Level::Warn
    );
    assert_eq!(
        level_of("[F:/in] job unavailable: import folder gone"),
        Level::Warn
    );
    assert_eq!(level_of("x.zip: needs review: NO_GAME_MATCH"), Level::Warn);
    assert_eq!(level_of("x.zip: corrupt: bad"), Level::Error);
    assert_eq!(
        level_of("internal error (panic) while processing"),
        Level::Error
    );
}

#[test]
fn config_with_and_without_the_log_object() {
    let base = r#""jobs":[{"import":"/a","library":"/b"}]"#;
    let c = SanitizerConfig::from_json(&format!("{{{base}}}")).unwrap();
    assert!(c.log.is_none());
    let c = SanitizerConfig::from_json(&format!(
        "{{{base},\"log\":{{\"file\":\"C:/x/s.log\",\"max_mb\":2,\"keep\":3,\"max_age_days\":7,\"future\":1}}}}"
    ))
    .unwrap();
    let l = c.log.unwrap();
    assert_eq!(l.file.unwrap(), PathBuf::from("C:/x/s.log"));
    assert_eq!(
        (l.max_mb, l.keep, l.max_age_days),
        (Some(2), Some(3), Some(7))
    );
    let c = SanitizerConfig::from_json(&format!("{{{base},\"log\":{{}}}}")).unwrap();
    assert!(c.log.unwrap().file.is_none());
    for bad in [
        "\"max_mb\":0",
        "\"max_mb\":5000",
        "\"keep\":101",
        "\"max_age_days\":0",
    ] {
        assert!(
            SanitizerConfig::from_json(&format!("{{{base},\"log\":{{{bad}}}}}")).is_err(),
            "{bad}"
        );
    }
    // Top-level unknown fields are still refused, as before.
    assert!(SanitizerConfig::from_json(&format!("{{{base},\"nope\":1}}")).is_err());
}

#[test]
fn relative_log_file_is_resolved_against_the_config_folder() {
    let t = Tmp::new();
    let cfg = t.path().join("sanitizer.json");
    fs::write(
        &cfg,
        r#"{"jobs":[{"import":"in","library":"lib"}],"log":{"file":"logs/s.log"}}"#,
    )
    .unwrap();
    let c = SanitizerConfig::load(&cfg).unwrap();
    assert_eq!(c.log.unwrap().file.unwrap(), t.path().join("logs/s.log"));
}

#[test]
fn binary_writes_banner_and_results_to_the_log_file() {
    let t = Tmp::new();
    let import = t.path().join("import");
    let library = t.path().join("library");
    fs::create_dir_all(&import).unwrap();
    let log = t.path().join("logs/out.log");
    let st = std::process::Command::new(env!("CARGO_BIN_EXE_ue_asset_sanitizer"))
        .args(["--import"])
        .arg(&import)
        .arg("--library")
        .arg(&library)
        .arg("--once")
        .arg("--log-file")
        .arg(&log)
        .args(["--log-max-mb", "1", "--log-keep", "2", "--stable-secs", "0"])
        .output()
        .unwrap();
    assert!(st.status.success(), "{st:?}");
    let text = fs::read_to_string(&log).unwrap();
    assert!(
        text.contains("ue_asset_sanitizer ") && text.contains("starting (once)"),
        "{text}"
    );
    assert!(text.contains("job 1: import"), "{text}");
    assert!(
        text.contains("log file ") && text.contains("worst case 3 MiB"),
        "{text}"
    );
    assert!(text.contains("finished, exit code 0"), "{text}");
    // Nothing to do: no per-pass noise beyond the banner and the summary.
    assert!(text.lines().count() <= 10, "{text}");
    // Still prints to the console.
    assert!(String::from_utf8_lossy(&st.stdout).contains("starting (once)"));
}
