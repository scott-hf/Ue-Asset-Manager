//! Command line and config file of `ue_asset_mcp`.

use crate::bundles::BundleConfig;
use crate::catalog::LoadOptions;
use crate::install_log;
use crate::tools::AppConfig;
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

fn number(v: &str, flag: &str) -> Result<u64, String> {
    v.trim()
        .parse()
        .map_err(|_| format!("{flag} needs a number (got `{v}`)"))
}

pub const USAGE: &str = "\
ue_asset_mcp - stdio MCP server over the local Unreal asset library

USAGE:
  ue_asset_mcp --library <dir> [--library <dir> ...] [--project <uproject-or-root> ...] [options]

OPTIONS:
  --library <dir>       Library folder with the sanitized zips (repeatable). A share root with a
                        `library` subfolder is accepted.
  --project <path>      A .uproject (or project root) that imports may write into (repeatable).
  --config <file>       JSON file {\"libraries\": [...], \"projects\": [...]} merged with the flags.
  --cache <file>        Startup cache file (default: <user cache dir>/ue_asset_catalog/catalog.json).
  --no-cache            Do not read or write the startup cache.
  --no-build            Never build missing asset indexes from the zips (use overlays/cache only).
  --write-overlays      Also write built indexes as <zip>.assets.json next to the zips (default:
                        the library is never written; built indexes live in the local cache).
  --install-log <file>  Install log to append to (default: the app's install_log.json).
  --no-install-log      Do not write the install log.
  --exclude-pack <slug> Hide a pack family (its slug, see list_packs) from search_assets (repeatable).
  --portal-url <url>    Pipeline portal for the generated-assets source (default
                        http://192.168.12.201:13000; plain http, read-only list calls).
  --no-generated        Do not read generated assets from the portal.
  --allow-generation    Enable the request_generation tool (otherwise it refuses).
  --portal-token-file <file>
                        Bearer token file for request_generation (default: env HF_PORTAL_TOKEN, then
                        the HF infrastructure token file). Never logged or returned.
  --max-generations <n> Generation jobs per server session (default 5).
  --selftest            Load the library, print stats and sample searches, then exit.
  --http <ip:port>      Serve MCP over HTTP (POST /mcp, GET /health) instead of stdio. Needs a concrete
                        interface address (0.0.0.0 and :: are refused); implies --read-only.
  --token-file <file>   Bearer token for --http (required; exit 2 if missing or empty). Never logged.
  --allowed-host <h:p>  Extra accepted Host header value (repeatable; default: the --http address).
  --bundle-dir <dir>    With --http: server-local folder for import bundles; turns on the prepare_import
                        tool and GET /bundles/<id>. The server writes only here (never the library).
  --bundle-ttl-secs <n> Seconds a bundle stays downloadable, from publication (default 900).
  --max-bundle-bytes <n>  Cap of one bundle, uncompressed (default 268435456).
  --max-cache-bytes <n>   Cap of the bundle folder, published plus being built (default 2147483648).
  --max-builds <n>        Bundles built at the same time (default 1).
  --max-hour-bytes <n>    Uncompressed bytes bundled per rolling hour, all clients (default 1073741824;
                          alias --max-session-bytes).
  --read-only           Hide and refuse every tool that writes or changes state (import_assets,
                        plan_import, list_projects, refresh_catalog; the generation tools unless
                        --allow-generation). Cannot be combined with --write-overlays.
  --version, --help

Without --http the server speaks MCP (JSON-RPC 2.0, one message per line) on stdin/stdout; logs go
to stderr.";

/// `<cache dir>/ue_asset_catalog/<name>` next to the catalog cache.
fn cache_dir_file(name: &str) -> Option<PathBuf> {
    install_log::default_cache_path().and_then(|p| p.parent().map(|d| d.join(name)))
}

#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    libraries: Vec<String>,
    #[serde(default)]
    projects: Vec<String>,
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Parsed {
    Run(AppConfig, Mode),
    Help,
    Version,
}

/// `--http` settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpConfig {
    pub addr: SocketAddr,
    pub token_file: Option<PathBuf>,
    pub allowed_hosts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Serve,
    SelfTest,
    Http(HttpConfig),
}

/// Parse the arguments after the program name.
pub fn parse_args(args: &[String]) -> Result<Parsed, String> {
    let mut libraries: Vec<PathBuf> = Vec::new();
    let mut projects: Vec<PathBuf> = Vec::new();
    let mut cache: Option<PathBuf> = None;
    let mut use_cache = true;
    let mut build = true;
    let mut write_overlays = false;
    let mut exclude_packs: Vec<String> = Vec::new();
    let mut portal: Option<String> = Some(crate::generated::DEFAULT_PORTAL_URL.to_string());
    let mut generation = crate::generation::GenerationConfig::default();
    let mut log: Option<PathBuf> = None;
    let mut use_log = true;
    let mut mode = Mode::Serve;
    let mut configs: Vec<PathBuf> = Vec::new();
    let mut http: Option<SocketAddr> = None;
    let mut token_file: Option<PathBuf> = None;
    let mut allowed_hosts: Vec<String> = Vec::new();
    let mut read_only = false;
    let mut bundle_dir: Option<PathBuf> = None;
    let mut bundle = BundleConfig::new(PathBuf::new());
    let mut bundle_flags = false;

    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |name: &str| -> Result<String, String> {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match a.as_str() {
            "--help" | "-h" => return Ok(Parsed::Help),
            "--version" | "-V" => return Ok(Parsed::Version),
            "--library" => libraries.push(PathBuf::from(value("--library")?)),
            "--project" => projects.push(PathBuf::from(value("--project")?)),
            "--config" => configs.push(PathBuf::from(value("--config")?)),
            "--cache" => cache = Some(PathBuf::from(value("--cache")?)),
            "--no-cache" => use_cache = false,
            "--no-build" => build = false,
            "--write-overlays" => write_overlays = true,
            "--exclude-pack" => exclude_packs.push(value("--exclude-pack")?),
            "--portal-url" => {
                let u = value("--portal-url")?;
                generation.portal_url = u.clone();
                portal = Some(u);
            }
            "--no-generated" => portal = None,
            "--allow-generation" => generation.allowed = true,
            "--portal-token-file" => {
                generation.token_file = Some(PathBuf::from(value("--portal-token-file")?));
            }
            "--max-generations" => {
                generation.max_submissions = value("--max-generations")?
                    .parse()
                    .map_err(|_| "--max-generations needs a number".to_string())?;
            }
            "--install-log" => log = Some(PathBuf::from(value("--install-log")?)),
            "--no-install-log" => use_log = false,
            "--selftest" => mode = Mode::SelfTest,
            "--read-only" => read_only = true,
            "--bundle-dir" => {
                bundle_dir = Some(PathBuf::from(value("--bundle-dir")?));
                bundle_flags = true;
            }
            "--bundle-ttl-secs" => {
                let n: u64 = number(&value("--bundle-ttl-secs")?, "--bundle-ttl-secs")?;
                if n == 0 {
                    return Err("--bundle-ttl-secs must be at least 1".to_string());
                }
                bundle.ttl = Duration::from_secs(n);
                bundle_flags = true;
            }
            "--max-bundle-bytes" => {
                bundle.max_bundle_bytes =
                    number(&value("--max-bundle-bytes")?, "--max-bundle-bytes")?;
                bundle_flags = true;
            }
            "--max-cache-bytes" => {
                bundle.max_cache_bytes = number(&value("--max-cache-bytes")?, "--max-cache-bytes")?;
                bundle_flags = true;
            }
            "--max-builds" => {
                let n: u64 = number(&value("--max-builds")?, "--max-builds")?;
                if n == 0 {
                    return Err("--max-builds must be at least 1".to_string());
                }
                bundle.max_builds = n as usize;
                bundle_flags = true;
            }
            "--max-hour-bytes" | "--max-session-bytes" => {
                bundle.max_hour_bytes = number(&value(a)?, a)?;
                bundle_flags = true;
            }
            "--http" => {
                let v = value("--http")?;
                let addr: SocketAddr = v
                    .parse()
                    .map_err(|_| format!("--http needs <ip>:<port> (got `{v}`)"))?;
                if addr.ip().is_unspecified() {
                    return Err(
                        "--http must bind one concrete interface address, never 0.0.0.0 or ::"
                            .to_string(),
                    );
                }
                http = Some(addr);
            }
            "--token-file" => token_file = Some(PathBuf::from(value("--token-file")?)),
            "--allowed-host" => allowed_hosts.push(value("--allowed-host")?.to_ascii_lowercase()),
            other => return Err(format!("unknown argument `{other}` (see --help)")),
        }
    }
    for c in configs {
        let text = std::fs::read_to_string(&c)
            .map_err(|e| format!("cannot read config {}: {e}", c.display()))?;
        let f: ConfigFile = serde_json::from_str(text.trim_start_matches('\u{feff}'))
            .map_err(|e| format!("bad config {}: {e}", c.display()))?;
        libraries.extend(f.libraries.into_iter().map(PathBuf::from));
        projects.extend(f.projects.into_iter().map(PathBuf::from));
    }
    if libraries.is_empty() {
        return Err("at least one --library <dir> is required (see --help)".to_string());
    }
    if http.is_some() {
        if mode == Mode::SelfTest {
            return Err("--selftest cannot be combined with --http".to_string());
        }
        read_only = true; // the hosted profile is always read-only
    } else if token_file.is_some() || !allowed_hosts.is_empty() || bundle_flags {
        return Err("--token-file, --allowed-host and the bundle flags need --http".to_string());
    }
    let bundle = match bundle_dir {
        Some(dir) if http.is_some() => {
            bundle.dir = dir;
            Some(bundle)
        }
        _ => None,
    };
    if read_only && write_overlays {
        return Err(
            "--read-only (and --http) cannot be combined with --write-overlays".to_string(),
        );
    }
    if let Some(addr) = http {
        if allowed_hosts.is_empty() {
            allowed_hosts.push(addr.to_string());
        }
        mode = Mode::Http(HttpConfig {
            addr,
            token_file,
            allowed_hosts,
        });
    }
    let cache_path = if use_cache {
        cache.or_else(install_log::default_cache_path)
    } else {
        None
    };
    let install_log = if use_log {
        log.or_else(install_log::default_path)
    } else {
        None
    };
    Ok(Parsed::Run(
        AppConfig {
            libraries,
            projects,
            load: LoadOptions {
                build_missing: build,
                cache_path,
                threads: 4,
                write_overlays,
            },
            install_log,
            portal: portal.map(|u| {
                let mut p = crate::generated::PortalConfig::new(&u);
                p.cache_path = cache_dir_file("generated.json");
                p
            }),
            exclude_packs,
            generation,
            read_only,
            no_auto_refresh: matches!(mode, Mode::Http(_)),
            bundle,
        },
        mode,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn parses_flags() {
        let Ok(Parsed::Run(cfg, mode)) = parse_args(&a(&[
            "--library",
            "/l1",
            "--library",
            "/l2",
            "--project",
            "/p/P.uproject",
            "--no-cache",
            "--no-build",
            "--install-log",
            "/tmp/log.json",
            "--selftest",
            "--exclude-pack",
            "junk-pack",
        ])) else {
            panic!("expected Run");
        };
        assert_eq!(cfg.libraries.len(), 2);
        assert_eq!(cfg.projects.len(), 1);
        assert!(cfg.load.cache_path.is_none());
        assert!(!cfg.load.build_missing);
        assert_eq!(cfg.install_log.unwrap(), PathBuf::from("/tmp/log.json"));
        assert_eq!(mode, Mode::SelfTest);
        assert_eq!(cfg.exclude_packs, ["junk-pack"]);
    }

    #[test]
    fn errors_and_help() {
        assert!(parse_args(&a(&[])).is_err());
        assert!(parse_args(&a(&["--library"])).is_err());
        assert!(parse_args(&a(&["--bogus"])).is_err());
        assert!(matches!(parse_args(&a(&["--help"])), Ok(Parsed::Help)));
        assert!(matches!(
            parse_args(&a(&["--version"])),
            Ok(Parsed::Version)
        ));
    }
}
