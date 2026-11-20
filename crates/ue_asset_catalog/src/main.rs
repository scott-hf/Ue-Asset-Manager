//! `ue_asset_mcp`: MCP server over the local Unreal asset library, on stdio (default) or, with
//! `--http`, a read-only HTTP service. On stdio only protocol messages go to stdout; every log
//! line goes to stderr.

use std::io::{self, BufReader};
use std::net::TcpListener;
use std::process::ExitCode;
use ue_asset_catalog::catalog::Catalog;
use ue_asset_catalog::config::{parse_args, HttpConfig, Mode, Parsed, USAGE};
use ue_asset_catalog::http_server::{self, Limits, StartOptions};
use ue_asset_catalog::tools::{load_catalog, AppConfig};
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
    if let Mode::Http(http) = &mode {
        return run_http(cfg, http);
    }
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

/// The hosted, read-only HTTP service. Exit 2: no usable token file.
fn run_http(cfg: AppConfig, http: &HttpConfig) -> ExitCode {
    let Some(token_file) = &http.token_file else {
        eprintln!("ue_asset_mcp: --http needs --token-file <file> with the bearer token");
        return ExitCode::from(2);
    };
    let token = match http_server::read_token_file(token_file) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("ue_asset_mcp: {e}");
            return ExitCode::from(2);
        }
    };
    let listener = match TcpListener::bind(http.addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("ue_asset_mcp: cannot bind {}: {e}", http.addr);
            return ExitCode::from(1);
        }
    };
    let loader_cfg = cfg.clone();
    let app = App::with_catalog(Catalog::empty(), cfg);
    let running = match http_server::start(
        listener,
        app,
        Box::new(move || load_catalog(&loader_cfg)),
        StartOptions {
            token,
            allowed_hosts: http.allowed_hosts.clone(),
            limits: Limits::default(),
        },
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ue_asset_mcp: cannot start the HTTP server: {e}");
            return ExitCode::from(1);
        }
    };
    eprintln!(
        "[ue_asset_mcp] listening on http://{} (read-only; catalog loading in the background)",
        running.local_addr
    );
    running.wait();
    ExitCode::SUCCESS
}
