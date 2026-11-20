//! Sanitizer service: turns zips dropped in `<import>/` (or `<import>/unprocessed/`) into library entries.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use ue_asset_library::config::{
    default_config_path, run_all_once, JobConfig, JobRunner, SanitizerConfig,
};
use ue_asset_library::pipeline::Config;
use ue_asset_library::rotating_log::{level_of, Level, Options, RotatingLog};

/// Seconds between "alive" lines in the log (watch mode).
const HEARTBEAT_LOG_SECS: u64 = 600;

type Logger = Option<Arc<RotatingLog>>;

/// Console output that never panics (a scheduled task has no console).
fn out(msg: &str) {
    let _ = writeln!(std::io::stdout(), "{msg}");
}

/// Console + log file; the level is guessed from the message.
fn emit(logger: &Logger, msg: &str) {
    out(msg);
    if let Some(l) = logger {
        l.log(level_of(msg), msg);
    }
}

fn emit_level(logger: &Logger, level: Level, msg: &str) {
    out(msg);
    if let Some(l) = logger {
        l.log(level, msg);
    }
}

const USAGE: &str = "usage:\n  \
ue_asset_sanitizer [--config <sanitizer.json>] [--once|--watch] [options]\n  \
ue_asset_sanitizer --import <dir> --library <dir> [--once|--watch] [options]   (single ad-hoc job)\n\
options: [--log-file <path>] [--log-max-mb N] [--log-keep N] [--log-max-age-days N] [--poll-secs N] [--stable-secs N] [--stale-mins N] [--lease-secs N] [--heartbeat-secs N] [--max-reclaims N] [--max-attempts N] [--min-extract-ratio R] [--requeue]\n\
Without --import/--library the config is --config, else sanitizer.json next to the executable.\n\
--import/--library cannot be combined with --config. Numeric flags override the config values.\n\
Logging: --log-file (or a \"log\" object in the config) writes timestamped lines to a rotating\n\
file (defaults 5 MiB, keep 5, 30 days; worst case (keep+1)*max MiB); flags win over the config. Without a file only the console is used.\n\
exit codes: 0 ok, 1 usage/fatal error, 2 some zips need review or failed, 3 some job was unavailable";

struct Args {
    config: Option<PathBuf>,
    import: Option<PathBuf>,
    library: Option<PathBuf>,
    watch: bool,
    poll: Option<u64>,
    stable: Option<u64>,
    stale: Option<u64>,
    lease: Option<u64>,
    heartbeat: Option<u64>,
    max_reclaims: Option<u64>,
    max_attempts: Option<u64>,
    min_extract_ratio: Option<f64>,
    requeue: bool,
    log_file: Option<PathBuf>,
    log_max_mb: Option<u64>,
    log_keep: Option<u32>,
    log_max_age_days: Option<u64>,
}

fn take_value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next().ok_or_else(|| format!("{flag} needs a value"))
}

fn take_num(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<u64, String> {
    take_value(args, flag)?
        .parse()
        .map_err(|_| format!("{flag} needs a number"))
}

fn parse() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let mut a = Args {
        config: None,
        import: None,
        library: None,
        watch: false,
        poll: None,
        stable: None,
        stale: None,
        lease: None,
        heartbeat: None,
        max_reclaims: None,
        max_attempts: None,
        min_extract_ratio: None,
        requeue: false,
        log_file: None,
        log_max_mb: None,
        log_keep: None,
        log_max_age_days: None,
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => a.config = Some(PathBuf::from(take_value(&mut args, "--config")?)),
            "--import" => a.import = Some(PathBuf::from(take_value(&mut args, "--import")?)),
            "--library" => a.library = Some(PathBuf::from(take_value(&mut args, "--library")?)),
            "--once" => a.watch = false,
            "--watch" => a.watch = true,
            "--poll-secs" => a.poll = Some(take_num(&mut args, "--poll-secs")?.max(1)),
            "--stable-secs" => a.stable = Some(take_num(&mut args, "--stable-secs")?),
            "--stale-mins" => a.stale = Some(take_num(&mut args, "--stale-mins")?),
            "--lease-secs" => a.lease = Some(take_num(&mut args, "--lease-secs")?.max(2)),
            "--heartbeat-secs" => {
                a.heartbeat = Some(take_num(&mut args, "--heartbeat-secs")?.max(1))
            }
            "--max-reclaims" => a.max_reclaims = Some(take_num(&mut args, "--max-reclaims")?),
            "--max-attempts" => {
                a.max_attempts = Some(take_num(&mut args, "--max-attempts")?.max(1))
            }
            "--min-extract-ratio" => {
                a.min_extract_ratio = Some(
                    take_value(&mut args, "--min-extract-ratio")?
                        .parse()
                        .map_err(|_| "--min-extract-ratio needs a number like 0.95".to_string())?,
                )
            }
            "--requeue" => a.requeue = true,
            "--log-file" => a.log_file = Some(PathBuf::from(take_value(&mut args, "--log-file")?)),
            "--log-max-mb" => {
                let v = take_num(&mut args, "--log-max-mb")?;
                if !(1..=1024).contains(&v) {
                    return Err("--log-max-mb must be 1..=1024".into());
                }
                a.log_max_mb = Some(v);
            }
            "--log-keep" => {
                let v = take_num(&mut args, "--log-keep")?;
                if v > 100 {
                    return Err("--log-keep must be at most 100".into());
                }
                a.log_keep = Some(v as u32);
            }
            "--log-max-age-days" => {
                a.log_max_age_days = Some(take_num(&mut args, "--log-max-age-days")?.max(1))
            }
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(a)
}

fn build_config(a: &Args) -> Result<SanitizerConfig, String> {
    let adhoc = a.import.is_some() || a.library.is_some();
    let mut cfg = if adhoc {
        if a.config.is_some() {
            return Err(
                "--import/--library cannot be combined with --config; use one or the other".into(),
            );
        }
        let (Some(import), Some(library)) = (a.import.clone(), a.library.clone()) else {
            return Err("--import and --library must be given together".into());
        };
        SanitizerConfig {
            jobs: vec![JobConfig { import, library }],
            poll_secs: 30,
            stable_secs: 5,
            stale_mins: 120,
            prefer_engine_versions: ue_asset_library::config::default_prefer_engine_versions(),
            lease_secs: 300,
            heartbeat_secs: 60,
            max_reclaims: 3,
            max_attempts: 5,
            retry_min_age_secs: 600,
            min_extract_success_ratio: 0.95,
            log: None,
        }
    } else {
        let path = a
            .config
            .clone()
            .or_else(default_config_path)
            .ok_or("no config: pass --config <file>, put sanitizer.json next to the executable, or use --import/--library")?;
        SanitizerConfig::load(&path)?
    };
    if let Some(v) = a.poll {
        cfg.poll_secs = v;
    }
    if let Some(v) = a.stable {
        cfg.stable_secs = v;
    }
    if let Some(v) = a.stale {
        cfg.stale_mins = v;
    }
    if let Some(v) = a.lease {
        cfg.lease_secs = v;
    }
    if let Some(v) = a.heartbeat {
        cfg.heartbeat_secs = v;
    }
    if let Some(v) = a.max_reclaims {
        cfg.max_reclaims = v as u32;
    }
    if let Some(v) = a.max_attempts {
        cfg.max_attempts = v as u32;
    }
    if let Some(v) = a.min_extract_ratio {
        if !(v > 0.0 && v <= 1.0) {
            return Err("--min-extract-ratio must be in (0, 1]".into());
        }
        cfg.min_extract_success_ratio = v;
    }
    if cfg.heartbeat_secs * 2 > cfg.lease_secs {
        return Err("heartbeat-secs must be at most half of lease-secs".into());
    }
    Ok(cfg)
}

fn make_logger(a: &Args, cfg: &SanitizerConfig) -> Logger {
    let conf = cfg.log.clone().unwrap_or_default();
    let file = a.log_file.clone().or(conf.file)?;
    let mut o = Options::default();
    if let Some(mb) = a.log_max_mb.or(conf.max_mb) {
        o.max_bytes = mb * 1024 * 1024;
    }
    if let Some(k) = a.log_keep.or(conf.keep) {
        o.keep = k;
    }
    if let Some(d) = a.log_max_age_days.or(conf.max_age_days) {
        o.max_age_days = d;
    }
    Some(Arc::new(RotatingLog::open(file, o)))
}

/// Archive-looking files waiting in the inbox folders (cheap: names only).
fn inbox_items(cfgs: &[Config]) -> usize {
    let mut n = 0;
    for c in cfgs {
        for dir in [c.import_dir.clone(), c.import_dir.join("unprocessed")] {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            n += rd
                .flatten()
                .filter(|e| {
                    let name = e.file_name().to_string_lossy().to_ascii_lowercase();
                    !name.starts_with('.')
                        && [".zip", ".rar", ".7z", ".001"]
                            .iter()
                            .any(|x| name.ends_with(x))
                })
                .count();
        }
    }
    n
}

fn info(logger: &Logger, msg: &str) {
    emit_level(logger, Level::Info, msg);
}

fn main() -> ExitCode {
    let args = match parse() {
        Ok(v) => v,
        Err(m) => {
            if !m.is_empty() {
                eprintln!("error: {m}");
            }
            eprintln!("{USAGE}");
            return ExitCode::from(if m.is_empty() { 0 } else { 1 });
        }
    };
    let cfg = match build_config(&args) {
        Ok(c) => c,
        Err(m) => {
            eprintln!("error: {m}");
            eprintln!("{USAGE}");
            return ExitCode::from(1);
        }
    };
    let logger = make_logger(&args, &cfg);
    if let Some(l) = &logger {
        // Panics (also in other threads) end up in the log too.
        let l = l.clone();
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |pi| {
            l.error(&format!("panic: {pi}"));
            prev(pi);
        }));
    }
    let cfgs = cfg.pipeline_configs(args.requeue);

    // Startup banner.
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "?".into());
    info(
        &logger,
        &format!(
            "ue_asset_sanitizer {} starting ({}), exe {exe}, pid {}",
            env!("CARGO_PKG_VERSION"),
            if args.watch { "watch" } else { "once" },
            std::process::id()
        ),
    );
    if args.import.is_none() {
        let path = args.config.clone().or_else(default_config_path);
        info(
            &logger,
            &format!(
                "config {}: {} job(s)",
                path.map(|p| p.display().to_string()).unwrap_or_default(),
                cfg.jobs.len()
            ),
        );
    }
    for (i, j) in cfg.jobs.iter().enumerate() {
        info(
            &logger,
            &format!(
                "job {}: import {} -> library {}",
                i + 1,
                j.import.display(),
                j.library.display()
            ),
        );
    }
    info(
        &logger,
        &format!(
            "settings: poll {}s, stable {}s, stale {} min, lease {}s, heartbeat {}s, attempts {}, requeue {}",
            cfg.poll_secs,
            cfg.stable_secs,
            cfg.stale_mins,
            cfg.lease_secs,
            cfg.heartbeat_secs,
            cfg.max_attempts,
            args.requeue
        ),
    );
    match &logger {
        Some(l) => {
            let o = l.options();
            info(
                &logger,
                &format!(
                    "log file {}: max {} MiB, keep {}, max age {} days, worst case {} MiB",
                    l.path().display(),
                    o.max_bytes / (1024 * 1024),
                    o.keep,
                    o.max_age_days,
                    o.worst_case_bytes() / (1024 * 1024)
                ),
            );
        }
        None => out("log file: none (console only)"),
    }
    let mut log = |m: &str| emit(&logger, m);

    if !args.watch {
        let results = run_all_once(&cfgs, &mut log);
        let mut code = 0;
        for (label, r) in results {
            match r {
                Ok(rep) => {
                    for l in &rep.loose_files {
                        emit(
                            &logger,
                            &format!("[{label}] ignored non-zip file (left in place): {l}"),
                        );
                    }
                    emit(
                        &logger,
                        &format!(
                            "[{label}] done: {} sanitized, {} duplicate, {} needs-review, {} failed, {} deferred, {} busy",
                            rep.sanitized.len(),
                            rep.duplicates.len(),
                            rep.needs_review.len(),
                            rep.failed.len(),
                            rep.deferred.len(),
                            rep.busy.len()
                        ),
                    );
                    if rep.exit_code() != 0 && code == 0 {
                        code = 2;
                    }
                }
                Err(_) => code = 3,
            }
        }
        info(&logger, &format!("finished, exit code {code}"));
        return ExitCode::from(code);
    }

    info(
        &logger,
        &format!("watching {} job(s), poll {}s", cfgs.len(), cfg.poll_secs),
    );

    // "alive" line every 10 minutes from its own thread, so a hung pass shows as a growing
    // "last pass" instead of silence.
    let start = Instant::now();
    let last_pass = Arc::new(AtomicU64::new(0));
    {
        let (logger, last_pass, cfgs) = (logger.clone(), last_pass.clone(), cfgs.clone());
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(HEARTBEAT_LOG_SECS));
            let ago = start
                .elapsed()
                .as_secs()
                .saturating_sub(last_pass.load(Ordering::Relaxed));
            info(
                &logger,
                &format!(
                    "alive, {} jobs, last pass {ago}s ago, inbox items {}",
                    cfgs.len(),
                    inbox_items(&cfgs)
                ),
            );
        });
    }

    let mut runners: Vec<JobRunner> = cfgs.into_iter().map(JobRunner::new).collect();
    let mut warned: Vec<(usize, String)> = Vec::new();
    loop {
        for (i, job) in runners.iter_mut().enumerate() {
            let label = job.label();
            let mut job_log = |m: &str| emit(&logger, &format!("[{label}] {m}"));
            if let Ok(rep) = job.tick(&mut job_log) {
                for l in rep.loose_files {
                    if !warned.contains(&(i, l.clone())) {
                        job_log(&format!("ignored non-zip file (left in place): {l}"));
                        warned.push((i, l));
                    }
                }
            }
        }
        last_pass.store(start.elapsed().as_secs(), Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(cfg.poll_secs));
    }
}
