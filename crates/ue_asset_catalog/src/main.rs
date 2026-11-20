//! `ue_asset_mcp`: stdio MCP server over the local Unreal asset library.
//! Only protocol messages go to stdout; every log line goes to stderr.

use std::io::{self, BufReader};
use std::process::ExitCode;
use ue_asset_catalog::config::{parse_args, Mode, Parsed, USAGE};
use ue_asset_catalog::{mcp, selftest, App, Server};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cfg, mode) = match parse_args(&args) {
        Ok(Parsed::Run(cfg, mode)) => (cfg, mode),
        Ok(Parsed::Help) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(Parsed::Version) => {
            println!("ue_asset_mcp {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("ue_asset_mcp: {e}\n\n{USAGE}");
            return ExitCode::from(1);
        }
    };
    let mut app = App::new(cfg);
    {
        let st = app.catalog.stats();
        let r = &app.load_report;
        eprintln!(
            "[ue_asset_mcp] catalog: {} packs ({} indexed, {} failed, {} plugins skipped), {} assets, {} projects; load {} ms (cache {}, overlay {}, built {})",
            st.packs,
            st.indexed_packs,
            st.failed_packs,
            st.skipped_plugins,
            st.assets,
            app.projects.len(),
            r.elapsed_ms,
            r.from_cache,
            r.from_overlay,
            r.built
        );
        for w in r.warnings.iter().chain(app.warnings.iter()) {
            eprintln!("[ue_asset_mcp] warning: {w}");
        }
        for (name, why) in &r.failed {
            eprintln!("[ue_asset_mcp] index failed for {name}: {why}");
        }
    }
    if mode == Mode::SelfTest {
        return match selftest::run(&mut app, &mut io::stdout()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("ue_asset_mcp: {e}");
                ExitCode::from(1)
            }
        };
    }
    let mut server = Server::new(app);
    let stdin = io::stdin();
    let stdout = io::stdout();
    match mcp::serve(&mut server, BufReader::new(stdin.lock()), stdout.lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[ue_asset_mcp] i/o error, exiting: {e}");
            ExitCode::from(1)
        }
    }
}
