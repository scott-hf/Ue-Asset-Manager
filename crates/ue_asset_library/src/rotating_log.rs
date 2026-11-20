//! Small thread-safe line logger with a hard disk cap.
//!
//! * Every line is `<UTC RFC 3339> <LEVEL> <message>`, flushed immediately.
//! * The file rotates when it would exceed `max_bytes`: `log` -> `log.1` -> ... -> `log.<keep>`, the
//!   oldest is deleted, so the worst case is about `(keep + 1) * max_bytes`.
//! * Rotated files older than `max_age_days` (and numbered beyond `keep`) are deleted at open and on
//!   every rotation.
//! * A single line is capped (`max_line` bytes) so a hostile file name cannot blow the budget.
//! * It never panics and never fails the caller: when the file cannot be opened or written the line
//!   goes to stderr and the open is retried later. Another process appending to the same file (an old
//!   instance still exiting) is fine: the file is opened in append mode, the size is re-read before
//!   each write and rotation errors are ignored.

use crate::util::{rfc3339_utc, unix_secs};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn label(self) -> &'static str {
        match self {
            Level::Info => "INFO ",
            Level::Warn => "WARN ",
            Level::Error => "ERROR",
        }
    }
}

/// Guess a level from a message the pipeline produced (`WARN: ...` prefixes, failure words).
pub fn level_of(msg: &str) -> Level {
    let low = msg.to_ascii_lowercase();
    if low.contains("panic") || low.contains("corrupt") || low.contains("failed") {
        Level::Error
    } else if msg.contains("WARN:")
        || low.contains("needs review")
        || low.contains("unavailable")
        || low.contains("cannot ")
        || low.contains("could not")
        || low.contains("busy")
    {
        Level::Warn
    } else {
        Level::Info
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// Rotate when the file would grow beyond this (default 5 MiB).
    pub max_bytes: u64,
    /// Rotated files kept (`.1` .. `.keep`, default 5).
    pub keep: u32,
    /// Rotated files older than this many days are deleted (default 30).
    pub max_age_days: u64,
    /// Longest single line in bytes, excluding the timestamp/level prefix (default 4 KiB).
    pub max_line: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            max_bytes: 5 * 1024 * 1024,
            keep: 5,
            max_age_days: 30,
            max_line: 4096,
        }
    }
}

impl Options {
    /// Worst-case disk use: the live file plus `keep` rotated files.
    pub fn worst_case_bytes(&self) -> u64 {
        (u64::from(self.keep) + 1) * self.max_bytes
    }
}

struct Inner {
    file: Option<File>,
    /// Do not retry a failed open before this instant.
    retry_at: Option<Instant>,
}

pub struct RotatingLog {
    path: PathBuf,
    opts: Options,
    inner: Mutex<Inner>,
}

const REOPEN_DELAY: Duration = Duration::from_secs(30);

fn rotated(path: &Path, n: u32) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(format!(".{n}"));
    PathBuf::from(s)
}

fn to_stderr(line: &str) {
    let _ = std::io::stderr().write_all(line.as_bytes());
}

impl RotatingLog {
    /// Open (or create) the log. Never fails: an unusable path just logs to stderr and retries later.
    pub fn open(path: impl Into<PathBuf>, opts: Options) -> RotatingLog {
        let log = RotatingLog {
            path: path.into(),
            opts,
            inner: Mutex::new(Inner {
                file: None,
                retry_at: None,
            }),
        };
        {
            let mut g = log.lock();
            log.ensure_open(&mut g);
        }
        log.cleanup_rotated();
        log
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn options(&self) -> &Options {
        &self.opts
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn ensure_open(&self, g: &mut Inner) {
        if g.file.is_some() {
            return;
        }
        if g.retry_at.is_some_and(|t| Instant::now() < t) {
            return;
        }
        if let Some(dir) = self.path.parent() {
            if !dir.as_os_str().is_empty() {
                let _ = fs::create_dir_all(dir);
            }
        }
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(f) => {
                g.file = Some(f);
                g.retry_at = None;
            }
            Err(e) => {
                g.retry_at = Some(Instant::now() + REOPEN_DELAY);
                to_stderr(&format!(
                    "{} WARN  cannot open log file {}: {e}\n",
                    rfc3339_utc(unix_secs()),
                    self.path.display()
                ));
            }
        }
    }

    pub fn info(&self, msg: &str) {
        self.log(Level::Info, msg);
    }
    pub fn warn(&self, msg: &str) {
        self.log(Level::Warn, msg);
    }
    pub fn error(&self, msg: &str) {
        self.log(Level::Error, msg);
    }

    fn format_line(&self, level: Level, msg: &str) -> String {
        // One physical line: control characters (newlines, NUL, escapes) become spaces.
        let mut clean: String = msg
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        if clean.len() > self.opts.max_line {
            let mut cut = self.opts.max_line;
            while cut > 0 && !clean.is_char_boundary(cut) {
                cut -= 1;
            }
            clean.truncate(cut);
            clean.push_str("...[truncated]");
        }
        format!("{} {} {clean}\n", rfc3339_utc(unix_secs()), level.label())
    }

    /// Append one line. Never panics, never returns an error.
    pub fn log(&self, level: Level, msg: &str) {
        let line = self.format_line(level, msg);
        let mut g = self.lock();
        self.ensure_open(&mut g);
        let Some(f) = g.file.as_mut() else {
            to_stderr(&line);
            return;
        };
        // Re-read the size: another process may be appending too.
        let size = f.metadata().map(|m| m.len()).unwrap_or(0);
        if size > 0 && size + line.len() as u64 > self.opts.max_bytes {
            self.rotate(&mut g);
            self.ensure_open(&mut g);
        }
        let ok = match g.file.as_mut() {
            Some(f) => f.write_all(line.as_bytes()).and_then(|_| f.flush()).is_ok(),
            None => false,
        };
        if !ok {
            g.file = None;
            g.retry_at = Some(Instant::now() + REOPEN_DELAY);
            to_stderr(&line);
        }
    }

    /// `log` -> `.1` -> ... -> `.keep`; the oldest is dropped. Failures are ignored.
    fn rotate(&self, g: &mut Inner) {
        g.file = None; // close our handle first (Windows cannot rename an open file by default)
        let keep = self.opts.keep;
        if keep == 0 {
            if fs::remove_file(&self.path).is_err() {
                // Still in use: empty it instead so the cap holds.
                if let Ok(f) = OpenOptions::new().write(true).open(&self.path) {
                    let _ = f.set_len(0);
                }
            }
        } else {
            let _ = fs::remove_file(rotated(&self.path, keep));
            for n in (1..keep).rev() {
                let _ = fs::rename(rotated(&self.path, n), rotated(&self.path, n + 1));
            }
            if fs::rename(&self.path, rotated(&self.path, 1)).is_err() {
                // Could not move it (locked by another process): truncate to hold the cap.
                if let Ok(f) = OpenOptions::new().write(true).open(&self.path) {
                    let _ = f.set_len(0);
                }
            }
        }
        self.cleanup_rotated();
    }

    /// Delete rotated files numbered beyond `keep` or older than `max_age_days`.
    pub fn cleanup_rotated(&self) {
        let Some(dir) = self.path.parent().map(|p| {
            if p.as_os_str().is_empty() {
                Path::new(".")
            } else {
                p
            }
        }) else {
            return;
        };
        let Some(name) = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
        else {
            return;
        };
        let prefix = format!("{name}.");
        let Ok(rd) = fs::read_dir(dir) else {
            return;
        };
        let max_age = Duration::from_secs(self.opts.max_age_days.saturating_mul(86_400));
        let now = SystemTime::now();
        for e in rd.flatten() {
            let fname = e.file_name().to_string_lossy().to_string();
            let Some(num) = fname.strip_prefix(&prefix) else {
                continue;
            };
            let Ok(n) = num.parse::<u32>() else {
                continue;
            };
            let too_many = n == 0 || n > self.opts.keep;
            let too_old = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| now.duration_since(t).ok())
                .is_some_and(|age| age > max_age);
            if too_many || too_old {
                let _ = fs::remove_file(e.path());
            }
        }
    }
}
