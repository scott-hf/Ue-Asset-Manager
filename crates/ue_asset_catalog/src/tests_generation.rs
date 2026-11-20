//! Tool-level tests for the generated-assets source and `request_generation`, against a local
//! fake portal. Nothing here talks to the real portal or submits a real job.

use crate::catalog::Catalog;
use crate::generated::PortalConfig;
use crate::generation::GenerationConfig;
use crate::http::fake;
use crate::testutil::Tmp;
use crate::tools::{App, AppConfig};
use serde_json::{json, Value};

fn call(app: &mut App, name: &str, args: Value) -> (bool, String) {
    let o = app.call_tool(name, &args).expect("known tool");
    (o.is_error, o.text())
}

fn lists() -> Vec<String> {
    let mesh = json!({"data": [{
        "jobId": "mesh-1", "title": "Mossy Stone Lantern", "state": "completed", "stage": "done",
        "targetProject": "PrjEfz", "outputPath": "F:/out/lantern",
        "unrealObjectPath": "/Game/Generated/SM_Lantern",
        "resultData": {"importedAssets": [{"class": "StaticMesh", "objectPath": "/Game/Generated/SM_Lantern", "packageFile": "Content/Generated/SM_Lantern.uasset"}],
                       "meshStats": {"trianglesAfter": 5000, "finalLongestCm": 60.0}},
        "customData": {"geometryComplexity": "moderate"}
    }], "meta": {"totalPage": 1}});
    let audio = json!({"data": [
        {"jobId": "aud-1", "title": "Door creak", "stage": "done", "audioType": "SFX", "prompt": "a wooden door creaking", "durationSeconds": 4.0, "unrealObjectPath": "/Game/Audio/SW_Creak"},
        {"jobId": "aud-2", "title": "Pending", "stage": "audio_review", "audioType": "Music"}
    ]});
    vec![
        fake::ok_json(&mesh.to_string()),
        fake::ok_json(&audio.to_string()),
    ]
}

#[test]
fn generated_assets_are_searchable_but_not_importable() {
    let t = Tmp::new();
    let srv = fake::serve(lists());
    let mut portal = PortalConfig::new(&srv.url);
    portal.cache_path = Some(t.path().join("generated.json"));
    let mut app = App::with_catalog(
        Catalog::empty(),
        AppConfig {
            portal: Some(portal),
            ..AppConfig::default()
        },
    );
    app.load_generated();
    assert_eq!(app.generated_report.meshes, 1);
    assert_eq!(app.generated_report.sounds, 1); // the audio_review row is not an entry

    let (err, text) = call(&mut app, "search_assets", json!({"query": "lantern"}));
    assert!(!err, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["total"], 1);
    let hit = &v["hits"][0];
    assert_eq!(hit["id"], "gen:mesh-1");
    assert_eq!(hit["source"], "generated");
    assert_eq!(hit["project"], "PrjEfz");
    assert!(hit["tags"].to_string().contains("lantern"));

    let (_, text) = call(&mut app, "search_assets", json!({"query": "creaking"}));
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap()["hits"][0]["id"],
        "gen:aud-1"
    );
    let (_, text) = call(&mut app, "search_assets", json!({"query": "pending"}));
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap()["total"], 0);

    let (err, text) = call(&mut app, "get_asset", json!({"id": "gen:mesh-1"}));
    assert!(!err, "{text}");
    let d: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(d["importable"], false);
    assert_eq!(d["object_path"], "/Game/Generated/SM_Lantern");
    assert_eq!(d["output_path"], "F:/out/lantern");
    assert!(d["tags"].to_string().contains("source:generated"));
    let (err, _) = call(&mut app, "get_asset", json!({"id": "gen:nope"}));
    assert!(err);
    let (err, text) = call(&mut app, "get_thumbnail", json!({"id": "gen:mesh-1"}));
    assert!(!err && text.contains("generated asset"));

    let (_, text) = call(&mut app, "list_packs", json!({}));
    let p: Value = serde_json::from_str(&text).unwrap();
    assert!(p["packs"].to_string().contains("Generated meshes"));

    // Importing a generated asset is refused with a pointer to where it lives.
    let proj = t.dir("proj");
    std::fs::write(proj.join("P.uproject"), r#"{"EngineAssociation":"5.4"}"#).unwrap();
    let mut app2 = App::with_catalog(
        Catalog::empty(),
        AppConfig {
            projects: vec![proj.join("P.uproject")],
            portal: app.cfg.portal.clone(),
            ..AppConfig::default()
        },
    );
    // Portal is gone now (one-shot fake): the cache from the first load answers.
    app2.cfg.portal.as_mut().unwrap().base_url = "http://127.0.0.1:1".into();
    app2.cfg.portal.as_mut().unwrap().timeout = std::time::Duration::from_millis(300);
    app2.load_generated();
    assert!(app2.generated_report.from_cache);
    let (err, text) = call(
        &mut app2,
        "plan_import",
        json!({"project": "P", "asset_ids": ["gen:mesh-1"]}),
    );
    assert!(err);
    assert!(
        text.contains("generated asset") && text.contains("/Game/Generated/SM_Lantern"),
        "{text}"
    );
}

#[test]
fn generation_is_refused_unless_allowed() {
    let mut app = App::with_catalog(Catalog::empty(), AppConfig::default());
    let (err, text) = call(
        &mut app,
        "request_generation",
        json!({"kind": "audio", "target_project": "PrjEfz", "prompt": "pop"}),
    );
    assert!(err);
    assert!(text.contains("--allow-generation"), "{text}");
}

#[test]
fn generation_validates_caps_and_never_returns_the_token() {
    let t = Tmp::new();
    let token_file = t.path().join("token.txt");
    std::fs::write(&token_file, "TOP-SECRET-TOKEN\r\n").unwrap();
    let srv = fake::serve(vec![
        fake::ok_json(r#"{"data":{"jobId":"aud-100"}}"#),
        fake::ok_json(r#"{"data":{"jobId":"aud-101"}}"#),
    ]);
    let mut app = App::with_catalog(
        Catalog::empty(),
        AppConfig {
            generation: GenerationConfig {
                allowed: true,
                portal_url: srv.url.clone(),
                token_file: Some(token_file),
                max_submissions: 2,
            },
            ..AppConfig::default()
        },
    );
    // Missing target project: refused before any network call.
    let (err, text) = call(
        &mut app,
        "request_generation",
        json!({"kind": "audio", "prompt": "pop"}),
    );
    assert!(err && text.contains("target_project"), "{text}");
    assert!(srv.requests.lock().unwrap().is_empty());
    // Mesh without an image: refused.
    let (err, text) = call(
        &mut app,
        "request_generation",
        json!({"kind": "mesh", "target_project": "P", "prompt": "a lamp"}),
    );
    assert!(err && text.contains("image"), "{text}");
    // Too many variations for the cap: refused without a network call.
    let (err, text) = call(
        &mut app,
        "request_generation",
        json!({"kind": "audio", "target_project": "P", "prompt": "pop", "variations": 3}),
    );
    assert!(err && text.contains("at most 2"), "{text}");
    assert!(srv.requests.lock().unwrap().is_empty());

    for (i, id) in ["aud-100", "aud-101"].iter().enumerate() {
        let (err, text) = call(
            &mut app,
            "request_generation",
            json!({"kind": "audio", "target_project": "PrjEfz", "prompt": "pop"}),
        );
        assert!(!err, "{text}");
        assert!(text.contains(id), "{text}");
        assert!(!text.contains("TOP-SECRET-TOKEN"));
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["submissions_used"], i + 1);
    }
    // The cap is reached.
    let (err, text) = call(
        &mut app,
        "request_generation",
        json!({"kind": "audio", "target_project": "PrjEfz", "prompt": "pop"}),
    );
    assert!(err && text.contains("at most 2"), "{text}");
    let reqs = srv.requests.lock().unwrap().clone();
    assert_eq!(reqs.len(), 2);
    assert!(reqs[0].contains("Authorization: Bearer TOP-SECRET-TOKEN"));
}

#[test]
fn generation_status_reads_the_public_queue() {
    let body = json!({"data": [{"jobId": "j9", "state": "running", "stage": "audio_generate"}]})
        .to_string();
    let srv = fake::serve(vec![
        fake::ok_json(&json!({"data": []}).to_string()),
        fake::ok_json(&body),
    ]);
    let mut app = App::with_catalog(
        Catalog::empty(),
        AppConfig {
            generation: GenerationConfig {
                portal_url: srv.url.clone(),
                ..GenerationConfig::default()
            },
            ..AppConfig::default()
        },
    );
    let (err, text) = call(&mut app, "generation_status", json!({"job_id": "j9"}));
    assert!(!err, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["found"], true);
    assert_eq!(v["job"]["queue"], "audio");
    assert_eq!(v["job"]["state"], "running");
}
