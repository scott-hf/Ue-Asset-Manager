//! Slow (writes a ~4.4 GiB zip): library zips past 4 GiB must stay readable. Run with
//! `cargo test --release -p ue_asset_library --test big_zip -- --ignored`.

use std::fs;
use std::io::{Read, Write};
use ue_asset_library::uasset::synth::Synth;
use ue_asset_library::{build_index, sanitize_zip, scan_library, SanitizeOutcome};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

#[test]
#[ignore]
fn entries_past_4gib_stay_readable_after_sanitizing() {
    let dir =
        std::env::temp_dir().join(format!("ue_asset_library_tests/big-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let src = dir.join("Big Pack 5.3.zip");
    let asset = Synth::new("StaticMesh", "SM_Late")
        .name("/Game/Big/Meshes/SM_Late")
        .build();
    {
        let mut w = ZipWriter::new(std::io::BufWriter::new(fs::File::create(&src).unwrap()));
        let stored = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .large_file(true);
        w.start_file("BigPack/Meshes/SM_First.uasset", stored)
            .unwrap();
        w.write_all(
            &Synth::new("StaticMesh", "SM_First")
                .name("/Game/Big/Meshes/SM_First")
                .build(),
        )
        .unwrap();
        w.start_file("BigPack/filler.bin", stored).unwrap();
        let chunk = vec![0u8; 1 << 20];
        for _ in 0..4400 {
            w.write_all(&chunk).unwrap();
        }
        let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        w.start_file("BigPack/Meshes/SM_Late.uasset", deflated)
            .unwrap();
        w.write_all(&asset).unwrap();
        w.start_file(
            "BigPack/Meshes/SM_Stored.uasset",
            SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
        )
        .unwrap();
        w.write_all(
            &Synth::new("StaticMesh", "SM_Stored")
                .name("/Game/Big/Meshes/SM_Stored")
                .build(),
        )
        .unwrap();
        w.finish().unwrap();
    }
    let lib = dir.join("lib");
    fs::create_dir_all(&lib).unwrap();
    match sanitize_zip(&src, &lib).expect("sanitize") {
        SanitizeOutcome::Sanitized { .. } => {}
        o => panic!("{o:?}"),
    }
    let entry = scan_library(&lib).remove(0);
    let mut z = ZipArchive::new(fs::File::open(&entry.zip_path).unwrap()).unwrap();
    let mut late = Vec::new();
    z.by_name("Content/Big/Meshes/SM_Late.uasset")
        .unwrap()
        .read_to_end(&mut late)
        .unwrap();
    assert_eq!(late, asset);
    let idx = build_index(&entry).unwrap();
    assert_eq!(idx.packages.len(), 3);
    assert!(
        idx.packages.iter().all(|p| !p.header_unreadable),
        "{:?}",
        idx.packages
    );
    assert_eq!(idx.engine_version_min.as_deref(), Some("5.3"));
    let _ = fs::remove_dir_all(&dir);
}
