//! Sanitizer service: turns zips dropped in `<import>/unprocessed/` into library entries.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;
use ue_asset_library::config::{
    default_config_path, run_all_once, JobConfig, JobRunner, SanitizerConfig,
};

const USAGE: &str = "usage:\n  \
ue_asset_sanitizer [--config <sanitizer.json>] [--once|--watch] [options]\n  \
ue_asset_sanitizer --import <dir> --library <dir> [--once|--watch] [options]   (single ad-hoc job)\n\
options: [--poll-secs N] [--stable-secs N] [--stale-mins N] [--requeue]\n\
Without --import/--library the config is --config, else sanitizer.json next to the executable.\n\
--import/--library cannot be combined with --config. Numeric flags override the config values.\n\
exit codes: 0 ok, 1 usage/fatal error, 2 some zips need review or failed, 3 some job was unavailable";

struct Args {
    config: Option<PathBuf>,
    import: Option<PathBuf>,
    library: Option<PathBuf>,
    watch: bool,
    poll: Option<u64>,
    stable: Option<u64>,
    stale: Option<u64>,
    requeue: bool,
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
        requeue: false,
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
            "--requeue" => a.requeue = true,
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
        }
    } else {
        let path = a
            .config
            .clone()
            .or_else(default_config_path)
            .ok_or("no config: pass --config <file>, put sanitizer.json next to the executable, or use --import/--library")?;
        let cfg = SanitizerConfig::load(&path)?;
        println!("config {}: {} job(s)", path.display(), cfg.jobs.len());
        cfg
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
    Ok(cfg)
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
    let cfgs = cfg.pipeline_configs(args.requeue);
    println!(
        "settings: poll {}s, stable {}s, stale {} min, requeue {}",
        cfg.poll_secs, cfg.stable_secs, cfg.stale_mins, args.requeue
    );
    let mut log = |m: &str| println!("{m}");

    if !args.watch {
        let results = run_all_once(&cfgs, &mut log);
        let mut code = 0;
        for (label, r) in results {
            match r {
                Ok(rep) => {
                    for l in &rep.loose_files {
                        println!("[{label}] ignored non-zip file in unprocessed/: {l}");
                    }
                    println!(
                        "[{label}] done: {} sanitized, {} duplicate, {} needs-review, {} failed, {} deferred, {} busy",
                        rep.sanitized.len(),
                        rep.duplicates.len(),
                        rep.needs_review.len(),
                        rep.failed.len(),
                        rep.deferred.len(),
                        rep.busy.len()
                    );
                    if rep.exit_code() != 0 && code == 0 {
                        code = 2;
                    }
                }
                Err(_) => code = 3,
            }
        }
        return ExitCode::from(code);
    }

    println!("watching {} job(s), poll {}s", cfgs.len(), cfg.poll_secs);
    let mut runners: Vec<JobRunner> = cfgs.into_iter().map(JobRunner::new).collect();
    let mut warned: Vec<(usize, String)> = Vec::new();
    loop {
        for (i, job) in runners.iter_mut().enumerate() {
            let label = job.label();
            let mut job_log = |m: &str| println!("[{label}] {m}");
            if let Ok(rep) = job.tick(&mut job_log) {
                for l in rep.loose_files {
                    if !warned.contains(&(i, l.clone())) {
                        job_log(&format!("ignored non-zip file in unprocessed/: {l}"));
                        warned.push((i, l));
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_secs(cfg.poll_secs));
    }
}
