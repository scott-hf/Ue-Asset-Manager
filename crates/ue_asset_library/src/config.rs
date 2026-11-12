//! `sanitizer.json` parsing and the multi-job runner.
//!
//! ```json
//! {"jobs":[{"import":"D:/lib/import","library":"D:/lib/library"}],
//!  "poll_secs":30,"stable_secs":5,"stale_mins":120}
//! ```

use crate::pipeline::{reconcile, run_once, run_pass, Config, PassReport, State};
use serde::{Deserialize, Serialize};
use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};

pub const DEFAULT_CONFIG_NAME: &str = "sanitizer.json";

fn default_poll() -> u64 {
    30
}
fn default_stable() -> u64 {
    5
}
fn default_stale() -> u64 {
    120
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct JobConfig {
    /// Inbox folder: zips dropped directly in it (and in an optional `unprocessed/`
    /// subfolder) are processed; the state folders are created inside. Must exist.
    pub import: PathBuf,
    /// Library folder (created when missing).
    pub library: PathBuf,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SanitizerConfig {
    pub jobs: Vec<JobConfig>,
    #[serde(default = "default_poll")]
    pub poll_secs: u64,
    #[serde(default = "default_stable")]
    pub stable_secs: u64,
    #[serde(default = "default_stale")]
    pub stale_mins: u64,
}

impl SanitizerConfig {
    /// Parse and validate JSON text. Relative paths are left as written.
    pub fn from_json(text: &str) -> Result<Self, String> {
        let text = text.trim_start_matches('\u{feff}');
        let cfg: SanitizerConfig =
            serde_json::from_str(text).map_err(|e| format!("invalid config: {e}"))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Read a config file; relative job paths are resolved against the file's folder
    /// (scheduled tasks start in an unpredictable working directory).
    pub fn load(path: &Path) -> Result<Self, String> {
        let text =
            fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let mut cfg = Self::from_json(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        if let Some(base) = path.parent() {
            for j in &mut cfg.jobs {
                if j.import.is_relative() {
                    j.import = base.join(&j.import);
                }
                if j.library.is_relative() {
                    j.library = base.join(&j.library);
                }
            }
        }
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), String> {
        if self.jobs.is_empty() {
            return Err("invalid config: \"jobs\" must list at least one job".into());
        }
        for (i, j) in self.jobs.iter().enumerate() {
            if j.import.as_os_str().is_empty() || j.library.as_os_str().is_empty() {
                return Err(format!(
                    "invalid config: job {i} needs non-empty \"import\" and \"library\""
                ));
            }
            if j.import == j.library {
                return Err(format!(
                    "invalid config: job {i} uses the same folder for import and library"
                ));
            }
        }
        if self.poll_secs == 0 {
            return Err("invalid config: \"poll_secs\" must be at least 1".into());
        }
        Ok(())
    }

    /// One pipeline [`Config`] per job.
    pub fn pipeline_configs(&self, requeue: bool) -> Vec<Config> {
        self.jobs
            .iter()
            .map(|j| {
                let mut c = Config::new(&j.import, &j.library);
                c.stable_secs = self.stable_secs;
                c.stale_mins = self.stale_mins;
                c.requeue = requeue;
                c
            })
            .collect()
    }
}

/// `sanitizer.json` next to the running executable, if it exists.
pub fn default_config_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let p = exe.parent()?.join(DEFAULT_CONFIG_NAME);
    p.is_file().then_some(p)
}

/// One job with its own state. A failure (unreachable share, panic) is returned
/// as `Err` and never affects other jobs.
pub struct JobRunner {
    pub cfg: Config,
    state: State,
    reconciled: bool,
    last_err: Option<String>,
}

impl JobRunner {
    pub fn new(cfg: Config) -> Self {
        JobRunner {
            cfg,
            state: State::default(),
            reconciled: false,
            last_err: None,
        }
    }

    pub fn label(&self) -> String {
        self.cfg.import_dir.display().to_string()
    }

    fn preflight(&self) -> Result<(), String> {
        let md = fs::metadata(&self.cfg.import_dir).map_err(|e| {
            format!(
                "import folder {} unavailable: {e}",
                self.cfg.import_dir.display()
            )
        })?;
        if !md.is_dir() {
            return Err(format!(
                "import path {} is not a folder",
                self.cfg.import_dir.display()
            ));
        }
        fs::create_dir_all(&self.cfg.library_dir).map_err(|e| {
            format!(
                "library folder {} unavailable: {e}",
                self.cfg.library_dir.display()
            )
        })?;
        Ok(())
    }

    fn guarded<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T, String>) -> Result<T, String> {
        match catch_unwind(AssertUnwindSafe(|| f(self))) {
            Ok(r) => r,
            Err(_) => Err("internal error (panic) while processing this job".into()),
        }
    }

    fn startup(&mut self, log: &mut dyn FnMut(&str)) -> Result<(), String> {
        self.preflight()?;
        if !self.reconciled {
            for a in reconcile(&self.cfg) {
                log(&format!("reconcile: {a}"));
            }
            self.reconciled = true;
        }
        Ok(())
    }

    /// One-shot: reconcile, scan, wait one stability interval when needed, scan again.
    pub fn once(&mut self, log: &mut dyn FnMut(&str)) -> Result<PassReport, String> {
        self.guarded(|me| {
            me.startup(log)?;
            Ok(run_once(&me.cfg, log))
        })
    }

    /// One watch tick. Availability changes are logged once, not on every poll.
    pub fn tick(&mut self, log: &mut dyn FnMut(&str)) -> Result<PassReport, String> {
        let r = self.guarded(|me| {
            me.startup(log)?;
            Ok(run_pass(&me.cfg, &mut me.state, log))
        });
        match &r {
            Err(e) if self.last_err.as_deref() != Some(e.as_str()) => {
                log(&format!("job unavailable: {e}"));
                self.last_err = Some(e.clone());
            }
            Ok(_) if self.last_err.is_some() => {
                log("job available again");
                self.last_err = None;
            }
            _ => {}
        }
        r
    }
}

/// Run every job once, independently. Results are in job order.
pub fn run_all_once(
    cfgs: &[Config],
    log: &mut dyn FnMut(&str),
) -> Vec<(String, Result<PassReport, String>)> {
    let mut out = Vec::new();
    for cfg in cfgs {
        let mut job = JobRunner::new(cfg.clone());
        let label = job.label();
        let mut job_log = |m: &str| log(&format!("[{label}] {m}"));
        let r = job.once(&mut job_log);
        if let Err(e) = &r {
            job_log(&format!("job failed: {e}"));
        }
        out.push((label, r));
    }
    out
}
