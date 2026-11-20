//! `ue_asset_mcp --selftest`: load the real library, print stats and a few sample searches.

use crate::tools::App;
use serde_json::{json, Value};
use std::io::{self, Write};
use std::time::Instant;

pub const SAMPLE_QUERIES: &[&str] = &["grass", "rock", "door"];

pub fn run(app: &mut App, out: &mut impl Write) -> io::Result<()> {
    let st = app.catalog.stats();
    let rep = &app.load_report;
    writeln!(out, "== catalog ==")?;
    writeln!(
        out,
        "pack files: {} ({} indexed, {} failed, {} plugins skipped), families: {}",
        st.packs, st.indexed_packs, st.failed_packs, st.skipped_plugins, st.families
    )?;
    writeln!(
        out,
        "assets (deduplicated across releases): {}  (packages before dedup: {})",
        st.assets, st.packages_total
    )?;
    writeln!(out, "distinct tags: {}", st.tags)?;
    writeln!(out, "assets by kind: {:?}", st.by_kind)?;
    writeln!(
        out,
        "load: {} ms (cache {}, overlay {}, built {}, cache written: {})",
        rep.elapsed_ms, rep.from_cache, rep.from_overlay, rep.built, rep.cache_written
    )?;
    for (name, why) in rep.failed.iter().take(10) {
        writeln!(out, "  index failed: {name}: {why}")?;
    }
    for w in &rep.warnings {
        writeln!(out, "  warning: {w}")?;
    }
    writeln!(out, "\n== top tags ==")?;
    let top = app
        .call_tool("list_tags", &json!({"limit": 15}))
        .map(|o| o.text());
    if let Some(Ok(v)) = top.map(|t| serde_json::from_str::<Value>(&t)) {
        for t in v["tags"].as_array().into_iter().flatten() {
            writeln!(
                out,
                "  {:>6}  {}",
                t["count"],
                t["tag"].as_str().unwrap_or("")
            )?;
        }
    }
    for q in SAMPLE_QUERIES {
        let t0 = Instant::now();
        let o = app
            .call_tool("search_assets", &json!({"query": q, "limit": 5}))
            .map(|o| o.text())
            .unwrap_or_default();
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        let v: Value = serde_json::from_str(&o).unwrap_or(Value::Null);
        writeln!(
            out,
            "\n== search \"{q}\": {} matches ({ms:.1} ms) ==",
            v["total"]
        )?;
        for h in v["hits"].as_array().into_iter().flatten() {
            writeln!(
                out,
                "  {:<8} {:<26} {}  [{}] {}",
                h["kind"].as_str().unwrap_or(""),
                h["class"].as_str().unwrap_or("?"),
                h["game_path"].as_str().unwrap_or(""),
                h["pack"].as_str().unwrap_or(""),
                h["engine_versions"]
            )?;
        }
    }
    Ok(())
}
