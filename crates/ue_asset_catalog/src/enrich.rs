//! `ue_asset_enrich`: describe the editor thumbnails of library assets with an Ollama vision model
//! and cache the result in `<zip>.vision.json` overlays. Opt-in, serial (one request at a time:
//! the shared Ollama box runs with `OLLAMA_NUM_PARALLEL=1`), resumable, never part of the MCP
//! server. A failure on one asset is logged and skipped.

use crate::catalog::{Catalog, LoadOptions, PackStatus};
use crate::vision::{self, VisionItem, VisionOverlay};
use std::path::PathBuf;
use ue_asset_library::LibraryEntry;

pub const USAGE: &str = "\
ue_asset_enrich - describe asset thumbnails with a vision model (Ollama) into <zip>.vision.json

USAGE:
  ue_asset_enrich --library <dir> [--library <dir> ...] [options]

OPTIONS:
  --library <dir>     Library folder (repeatable; a share root with a `library` folder works).
  --pack <id|name>    Only packs matching this id prefix, slug or name part.
  --limit <n>         Describe at most n assets in this run (default: all).
  --dry-run           Only list what would be done; no model calls, nothing written.
  --force             Redo assets that already have a description for this model and prompt.
  --ollama-url <url>  Ollama server (default env OLLAMA_URL or http://192.168.12.136:11434).
  --model <name>      Vision model (default env OLLAMA_MODEL or qwen3-vl:8b).
  --no-build          Do not build missing asset indexes (use overlays/cache only).
  --cache <file> | --no-cache
                      Catalog startup cache (default: the shared one).
  --help

Assets are described one at a time (120 s timeout each). Re-running resumes: assets already in
the overlay for the same model and prompt version are skipped.";

#[derive(Clone, Debug)]
pub struct EnrichOptions {
    pub libraries: Vec<PathBuf>,
    pub pack: Option<String>,
    pub limit: Option<usize>,
    pub dry_run: bool,
    pub force: bool,
    pub ollama_url: String,
    pub model: String,
    pub build_missing: bool,
    pub cache_path: Option<PathBuf>,
}

#[derive(Debug)]
pub enum Parsed {
    Run(EnrichOptions),
    Help,
}

pub fn parse_args(args: &[String]) -> Result<Parsed, String> {
    let mut o = EnrichOptions {
        libraries: Vec::new(),
        pack: None,
        limit: None,
        dry_run: false,
        force: false,
        ollama_url: std::env::var("OLLAMA_URL")
            .unwrap_or_else(|_| vision::DEFAULT_OLLAMA_URL.to_string()),
        model: std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| vision::DEFAULT_MODEL.to_string()),
        build_missing: true,
        cache_path: crate::install_log::default_cache_path(),
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |name: &str| -> Result<String, String> {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match a.as_str() {
            "--help" | "-h" => return Ok(Parsed::Help),
            "--library" => o.libraries.push(PathBuf::from(value("--library")?)),
            "--pack" => o.pack = Some(value("--pack")?),
            "--limit" => {
                o.limit = Some(
                    value("--limit")?
                        .parse()
                        .map_err(|_| "--limit needs a number".to_string())?,
                );
            }
            "--dry-run" => o.dry_run = true,
            "--force" => o.force = true,
            "--ollama-url" => o.ollama_url = value("--ollama-url")?,
            "--model" => o.model = value("--model")?,
            "--no-build" => o.build_missing = false,
            "--cache" => o.cache_path = Some(PathBuf::from(value("--cache")?)),
            "--no-cache" => o.cache_path = None,
            other => return Err(format!("unknown argument `{other}` (see --help)")),
        }
    }
    if o.libraries.is_empty() {
        return Err("at least one --library <dir> is required (see --help)".to_string());
    }
    Ok(Parsed::Run(o))
}

/// Reads one thumbnail (PNG/JPEG bytes) from a library entry.
pub type ThumbnailFn<'a> = dyn Fn(&LibraryEntry, &str) -> Result<Option<Vec<u8>>, String> + 'a;
/// Describes one image for a prompt.
pub type DescribeFn<'a> = dyn Fn(&str, &[u8]) -> Result<VisionItem, String> + 'a;

/// What to call; injectable so tests need neither a zip with thumbnails nor Ollama.
pub struct Backend<'a> {
    pub read_thumbnail: &'a ThumbnailFn<'a>,
    pub describe: &'a DescribeFn<'a>,
}

#[derive(Clone, Debug, Default)]
pub struct EnrichReport {
    pub packs: usize,
    pub already_done: usize,
    pub no_thumbnail: usize,
    pub described: usize,
    pub failed: usize,
    /// Dry run: `<pack id> <game_path>` lines.
    pub planned: Vec<String>,
    pub overlays_written: Vec<PathBuf>,
}

/// Run the enrichment. `log` receives progress lines.
pub fn run(
    opts: &EnrichOptions,
    backend: &Backend,
    log: &mut dyn FnMut(&str),
) -> Result<EnrichReport, String> {
    let (catalog, load) = Catalog::load(
        &opts.libraries,
        &LoadOptions {
            build_missing: opts.build_missing,
            cache_path: opts.cache_path.clone(),
            ..LoadOptions::default()
        },
    );
    for w in &load.warnings {
        log(&format!("warning: {w}"));
    }
    let wanted: Option<Vec<usize>> = opts.pack.as_deref().map(|p| {
        let mut v = catalog.match_packs(p);
        if let Some(i) = catalog.pack_by_id(p) {
            if !v.contains(&i) {
                v.push(i);
            }
        }
        v
    });
    if let Some(w) = &wanted {
        if w.is_empty() {
            return Err(format!(
                "no pack matches `{}`",
                opts.pack.clone().unwrap_or_default()
            ));
        }
    }

    let mut rep = EnrichReport::default();
    let mut budget = opts.limit.unwrap_or(usize::MAX);
    for (pi, pack) in catalog.packs.iter().enumerate() {
        if budget == 0 {
            break;
        }
        if pack.status != PackStatus::Indexed {
            continue;
        }
        if let Some(w) = &wanted {
            if !w.contains(&pi) {
                continue;
            }
        }
        let Some(index) = &pack.index else { continue };
        rep.packs += 1;
        let mut overlay = vision::load(&pack.entry)
            .filter(|o| o.is_current(&opts.model))
            .unwrap_or_else(|| VisionOverlay::new(&opts.model));
        for pkg in index.packages.iter().filter(|p| p.has_thumbnail) {
            if budget == 0 {
                break;
            }
            if !opts.force && overlay.items.contains_key(&pkg.game_path) {
                rep.already_done += 1;
                continue;
            }
            let bytes = match (backend.read_thumbnail)(&pack.entry, &pkg.game_path) {
                Ok(Some(b)) => b,
                Ok(None) => {
                    rep.no_thumbnail += 1;
                    continue;
                }
                Err(e) => {
                    log(&format!(
                        "{} {}: thumbnail not readable: {e}",
                        pack.id, pkg.game_path
                    ));
                    rep.failed += 1;
                    continue;
                }
            };
            budget -= 1;
            if opts.dry_run {
                log(&format!(
                    "would describe {} {} ({} bytes)",
                    pack.id,
                    pkg.game_path,
                    bytes.len()
                ));
                rep.planned.push(format!("{} {}", pack.id, pkg.game_path));
                continue;
            }
            let name = crate::tags::name_of(&pkg.game_path);
            let prompt = vision::build_prompt(name, pkg.class.as_deref());
            match (backend.describe)(&prompt, &bytes) {
                Ok(item) => {
                    log(&format!(
                        "{} {}: {} [{}]",
                        pack.id,
                        pkg.game_path,
                        item.description,
                        item.tags.join(", ")
                    ));
                    overlay.items.insert(pkg.game_path.clone(), item);
                    vision::save(&pack.entry, &overlay).map_err(|e| {
                        format!(
                            "cannot write {}: {e}",
                            vision::vision_path(&pack.entry).display()
                        )
                    })?;
                    let path = vision::vision_path(&pack.entry);
                    if !rep.overlays_written.contains(&path) {
                        rep.overlays_written.push(path);
                    }
                    rep.described += 1;
                }
                Err(e) => {
                    log(&format!("{} {}: skipped: {e}", pack.id, pkg.game_path));
                    rep.failed += 1;
                }
            }
        }
    }
    Ok(rep)
}

/// The real backend: thumbnails from the zip, descriptions from Ollama.
pub fn real_backend_run(
    opts: &EnrichOptions,
    log: &mut dyn FnMut(&str),
) -> Result<EnrichReport, String> {
    let read = |e: &LibraryEntry, g: &str| -> Result<Option<Vec<u8>>, String> {
        ue_asset_library::read_thumbnail(e, g).map_err(|e| e.to_string())
    };
    let url = opts.ollama_url.clone();
    let model = opts.model.clone();
    let describe =
        move |prompt: &str, img: &[u8]| vision::describe_image(&url, &model, prompt, img);
    run(
        opts,
        &Backend {
            read_thumbnail: &read,
            describe: &describe,
        },
        log,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{entry, index, pkg, Tmp};
    use std::cell::RefCell;
    use std::path::Path;
    use ue_asset_library::Kind;

    fn library() -> (Tmp, PathBuf) {
        let t = Tmp::new();
        let lib = t.dir("lib");
        let mut e = entry("Props", "aaaaaaaaaaaa0000", Some("5.3"), Kind::Content);
        e.zip_path = lib.join("props.aaaaaaaaaaaa.zip");
        e.sidecar_path = lib.join("props.aaaaaaaaaaaa.zip.json");
        std::fs::write(&e.zip_path, b"zip").unwrap();
        std::fs::write(&e.sidecar_path, serde_json::to_vec(&e.sidecar).unwrap()).unwrap();
        let mut thumb = pkg("/Game/Props/SM_Crate", "StaticMesh", &[]);
        thumb.has_thumbnail = true;
        let mut thumb2 = pkg("/Game/Props/SM_Barrel", "StaticMesh", &[]);
        thumb2.has_thumbnail = true;
        let none = pkg("/Game/Props/SM_Cooked", "StaticMesh", &[]);
        let idx = index("aaaaaaaaaaaa0000", vec![thumb2, thumb, none]);
        ue_asset_library::asset_index::write_index(&e, &idx).unwrap();
        (t, lib)
    }

    fn opts(lib: &Path) -> EnrichOptions {
        EnrichOptions {
            libraries: vec![lib.to_path_buf()],
            pack: None,
            limit: None,
            dry_run: false,
            force: false,
            ollama_url: "http://127.0.0.1:1".into(),
            model: "test-model".into(),
            build_missing: false,
            cache_path: None,
        }
    }

    #[test]
    fn dry_run_writes_nothing_and_calls_no_model() {
        let (_t, lib) = library();
        let calls = RefCell::new(0);
        let read = |_: &LibraryEntry, _: &str| Ok(Some(vec![1u8, 2, 3]));
        let describe = |_: &str, _: &[u8]| {
            *calls.borrow_mut() += 1;
            Err("must not be called".to_string())
        };
        let mut lines = Vec::new();
        let mut o = opts(&lib);
        o.dry_run = true;
        let rep = run(
            &o,
            &Backend {
                read_thumbnail: &read,
                describe: &describe,
            },
            &mut |l| lines.push(l.to_string()),
        )
        .unwrap();
        assert_eq!(*calls.borrow(), 0);
        assert_eq!(rep.planned.len(), 2);
        assert!(rep.overlays_written.is_empty());
        assert!(!lib.join("props.aaaaaaaaaaaa.zip.vision.json").exists());
        assert!(lines.iter().any(|l| l.starts_with("would describe")));
    }

    #[test]
    fn describes_resumes_limits_and_skips_failures() {
        let (_t, lib) = library();
        let read = |_: &LibraryEntry, _: &str| Ok(Some(vec![9u8; 4]));
        let seen = RefCell::new(Vec::<String>::new());
        let describe = |prompt: &str, _: &[u8]| {
            seen.borrow_mut().push(prompt.to_string());
            if prompt.contains("SM_Barrel") {
                Err("model said nothing".to_string())
            } else {
                Ok(VisionItem {
                    description: "A wooden crate.".into(),
                    tags: vec!["crate".into(), "wood".into()],
                    category: "prop".into(),
                })
            }
        };
        let backend = Backend {
            read_thumbnail: &read,
            describe: &describe,
        };
        let mut sink = |_: &str| {};
        // Barrel fails (skipped), crate succeeds.
        let rep = run(&opts(&lib), &backend, &mut sink).unwrap();
        assert_eq!((rep.described, rep.failed), (1, 1));
        let path = lib.join("props.aaaaaaaaaaaa.zip.vision.json");
        let o: VisionOverlay =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(o.model, "test-model");
        assert_eq!(o.prompt_version, vision::PROMPT_VERSION);
        assert_eq!(o.items.len(), 1);
        assert_eq!(o.items["/Game/Props/SM_Crate"].category, "prop");

        // Second run resumes: the crate is skipped, only the barrel is retried.
        seen.borrow_mut().clear();
        let rep = run(&opts(&lib), &backend, &mut sink).unwrap();
        assert_eq!(rep.already_done, 1);
        assert_eq!(seen.borrow().len(), 1);

        // A different model starts a fresh overlay; --limit stops early.
        let mut o2 = opts(&lib);
        o2.model = "other".into();
        o2.limit = Some(1);
        seen.borrow_mut().clear();
        let rep = run(&o2, &backend, &mut sink).unwrap();
        assert_eq!(seen.borrow().len(), 1);
        assert_eq!(rep.described + rep.failed, 1);
        let o: VisionOverlay = serde_json::from_str(&std::fs::read_to_string(&path).unwrap())
            .unwrap_or(VisionOverlay::new("x"));
        assert!(o.model == "other" || o.model == "test-model");
    }

    #[test]
    fn the_catalog_merges_the_overlay_after_enrichment() {
        let (_t, lib) = library();
        let read = |_: &LibraryEntry, _: &str| Ok(Some(vec![1u8]));
        let describe = |_: &str, _: &[u8]| {
            Ok(VisionItem {
                description: "A rusty oil drum.".into(),
                tags: vec!["drum".into(), "rust".into()],
                category: "prop".into(),
            })
        };
        let mut sink = |_: &str| {};
        run(
            &opts(&lib),
            &Backend {
                read_thumbnail: &read,
                describe: &describe,
            },
            &mut sink,
        )
        .unwrap();
        let (c, _) = Catalog::load(
            &[lib],
            &LoadOptions {
                build_missing: false,
                ..LoadOptions::default()
            },
        );
        let q = |t: &str| crate::search::SearchQuery {
            text: t.into(),
            ..Default::default()
        };
        assert_eq!(c.search(&q("drum")).total, 2);
        assert_eq!(c.search(&q("rusty")).total, 2);
        assert!(c
            .assets
            .iter()
            .any(|a| a.description.as_deref() == Some("A rusty oil drum.")));
    }

    #[test]
    fn parses_arguments() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let Ok(Parsed::Run(o)) = parse_args(&a(&[
            "--library",
            "/l",
            "--limit",
            "3",
            "--dry-run",
            "--pack",
            "props",
            "--model",
            "m",
            "--no-cache",
        ])) else {
            panic!()
        };
        assert_eq!((o.limit, o.dry_run, o.model.as_str()), (Some(3), true, "m"));
        assert!(o.cache_path.is_none());
        assert!(parse_args(&a(&[])).is_err());
        assert!(parse_args(&a(&["--limit", "x", "--library", "/l"])).is_err());
        assert!(matches!(parse_args(&a(&["--help"])), Ok(Parsed::Help)));
    }

    /// Real thumbnail + real Ollama on one real pack; run manually with `-- --ignored`.
    #[test]
    #[ignore]
    fn real_enrich_three_assets() {
        let dir = std::env::var("UE_ASSET_LIBRARY")
            .unwrap_or_else(|_| "//scott-server/ue_assets_library/library".into());
        let mut o = opts(&PathBuf::from(dir));
        o.ollama_url = vision::DEFAULT_OLLAMA_URL.into();
        o.model = vision::DEFAULT_MODEL.into();
        o.build_missing = true;
        o.limit = Some(3);
        o.pack = std::env::var("UE_ASSET_PACK").ok();
        let rep = real_backend_run(&o, &mut |l| println!("{l}")).unwrap();
        println!("{rep:?}");
    }
}
