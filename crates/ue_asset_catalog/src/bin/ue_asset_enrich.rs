//! `ue_asset_enrich`: opt-in vision tagging of library thumbnails (see `enrich.rs`).

use std::process::ExitCode;
use ue_asset_catalog::enrich::{parse_args, real_backend_run, Parsed, USAGE};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = match parse_args(&args) {
        Ok(Parsed::Run(o)) => o,
        Ok(Parsed::Help) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("ue_asset_enrich: {e}\n\n{USAGE}");
            return ExitCode::from(1);
        }
    };
    eprintln!(
        "[ue_asset_enrich] model {} at {}{}",
        opts.model,
        opts.ollama_url,
        if opts.dry_run { " (dry run)" } else { "" }
    );
    match real_backend_run(&opts, &mut |l| println!("{l}")) {
        Ok(r) => {
            println!(
                "done: {} pack(s), {} described, {} failed, {} without thumbnail, {} already done{}",
                r.packs,
                r.described,
                r.failed,
                r.no_thumbnail,
                r.already_done,
                if opts.dry_run {
                    format!(", {} planned", r.planned.len())
                } else {
                    String::new()
                }
            );
            for p in &r.overlays_written {
                println!("wrote {}", p.display());
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("ue_asset_enrich: {e}");
            ExitCode::from(1)
        }
    }
}
