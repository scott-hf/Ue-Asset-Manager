//! Claim leases, heartbeat and automatic reclaim of stale claims.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use ue_asset_library::lease::{
    new_body, read_body, reclaim_stale, start_heartbeat, write_atomic, LeaseParams,
};
use ue_asset_library::pipeline::{try_claim_set_with, ClaimResult, Config, Dirs};

static N: AtomicU64 = AtomicU64::new(0);

struct Tmp(PathBuf);
impl Tmp {
    fn new() -> Tmp {
        let n = N.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!(
            "ue_asset_library_lease/{}-{}-{}",
            std::process::id(),
            nanos,
            n
        ));
        fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn setup(t: &Tmp) -> (Config, Dirs) {
    let import = t.0.join("import");
    let lib = t.0.join("library");
    fs::create_dir_all(&import).unwrap();
    fs::create_dir_all(&lib).unwrap();
    let mut cfg = Config::new(&import, &lib);
    cfg.lease_grace_secs = 0;
    cfg.claim_grace_secs = 0;
    let dirs = Dirs::new(&import);
    dirs.create_all().unwrap();
    (cfg, dirs)
}

fn my_host() -> String {
    new_body(vec![], &LeaseParams::default()).host
}

/// A claim as another (possibly dead) worker left it, with its volumes in `processing/`.
fn forge(dirs: &Dirs, vols: &[&str], host: &str, pid: u32, expires_in: i64) {
    for v in vols {
        fs::write(dirs.processing.join(v), b"data").unwrap();
    }
    let body = serde_json::json!({
        "schema": 2, "host": host, "pid": pid, "proc_start": null,
        "run_id": "someone-else", "started_unix": now(), "heartbeat_unix": now(),
        "lease_expires_unix": now() + expires_in, "lease_secs": 300,
        "volumes": vols,
    });
    fs::write(
        dirs.processing.join(format!("{}.claim", vols[0])),
        body.to_string(),
    )
    .unwrap();
}

const DEAD_PID: u32 = u32::MAX - 1;

fn retries(dirs: &Dirs, name: &str) -> Option<String> {
    fs::read_to_string(dirs.processing.join(format!("{name}.retries"))).ok()
}

#[test]
fn dead_owner_is_reclaimed_exactly_once() {
    let t = Tmp::new();
    let (cfg, dirs) = setup(&t);
    // Lease still valid, but the owner process is gone on this very host.
    forge(&dirs, &["Pack 5.0.zip"], &my_host(), DEAD_PID, 1000);
    // A partial library output of that worker.
    let tmp_file = cfg.library_dir.join(".tmp-partial.zip");
    fs::write(&tmp_file, b"x").unwrap();
    let mut cfg2 = cfg.clone();
    cfg2.tmp_age_secs = 0;
    cfg2.lease_secs = 0;
    let r = reclaim_stale(&cfg2, &dirs, false);
    assert_eq!(
        r.requeued,
        vec!["Pack 5.0.zip".to_string()],
        "{:?}",
        r.actions
    );
    assert!(dirs.root.join("Pack 5.0.zip").is_file());
    assert!(!dirs.processing.join("Pack 5.0.zip").exists());
    assert!(!dirs.processing.join("Pack 5.0.zip.claim").exists());
    assert_eq!(retries(&dirs, "Pack 5.0.zip").as_deref(), Some("1"));
    assert!(!tmp_file.exists(), "partial output cleaned");
    // Nothing left to reclaim the second time.
    let r2 = reclaim_stale(&cfg2, &dirs, false);
    assert!(r2.requeued.is_empty() && r2.failed.is_empty());
    assert_eq!(retries(&dirs, "Pack 5.0.zip").as_deref(), Some("1"));
}

#[test]
fn expiry_rules_other_host_and_live_owner() {
    let t = Tmp::new();
    let (cfg, dirs) = setup(&t);
    // Other host, lease expired: reclaimed. Other host, lease valid: left alone.
    forge(&dirs, &["old.zip"], "some-other-host", 1, -10);
    forge(&dirs, &["fresh.zip"], "some-other-host", 1, 100);
    // Same host, owner alive (this test process), lease expired: never reclaimed.
    forge(&dirs, &["live.zip"], &my_host(), std::process::id(), -1000);
    let r = reclaim_stale(&cfg, &dirs, false);
    assert_eq!(r.requeued, vec!["old.zip".to_string()], "{:?}", r.actions);
    assert!(dirs.processing.join("fresh.zip.claim").is_file());
    assert!(dirs.processing.join("live.zip.claim").is_file());
    assert!(dirs.processing.join("live.zip").is_file());
}

#[test]
fn heartbeat_keeps_a_claim_alive_until_it_stops() {
    let t = Tmp::new();
    let (cfg, dirs) = setup(&t);
    let src = dirs.root.join("Pack.zip");
    fs::write(&src, b"data").unwrap();
    let lease = LeaseParams {
        lease_secs: 2,
        heartbeat_secs: 1,
        grace_secs: 0,
    };
    let claim = match try_claim_set_with(&dirs, std::slice::from_ref(&src), &lease) {
        ClaimResult::Claimed(c) => c,
        o => panic!("{o:?}"),
    };
    // Make it look like another host's job (so only the lease decides).
    let mut b = read_body(&claim.claim_path).unwrap();
    b.host = "some-other-host".into();
    b.run_id = "other-run".into();
    write_atomic(&claim.claim_path, &b).unwrap();
    let mut cfg = cfg;
    cfg.lease_secs = 2;
    let hb = start_heartbeat(claim.claim_path.clone(), lease);
    std::thread::sleep(std::time::Duration::from_millis(3600));
    let r = reclaim_stale(&cfg, &dirs, false);
    assert!(
        r.requeued.is_empty(),
        "live heartbeat must keep the claim: {:?}",
        r.actions
    );
    assert!(dirs.processing.join("Pack.zip").is_file());
    let hb_body = read_body(&claim.claim_path).unwrap();
    assert_eq!(
        hb_body.host, "some-other-host",
        "heartbeat keeps the owner identity"
    );
    drop(hb);
    std::thread::sleep(std::time::Duration::from_millis(2300));
    let r = reclaim_stale(&cfg, &dirs, false);
    assert_eq!(r.requeued, vec!["Pack.zip".to_string()], "{:?}", r.actions);
    assert!(dirs.root.join("Pack.zip").is_file());
}

#[test]
fn two_reclaimers_race_exactly_one_wins() {
    for round in 0..5 {
        let t = Tmp::new();
        let (cfg, dirs) = setup(&t);
        forge(&dirs, &["Race.zip"], "some-other-host", 1, -50);
        let barrier = Arc::new(Barrier::new(4));
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let (cfg, dirs, b) = (cfg.clone(), dirs.clone(), barrier.clone());
                std::thread::spawn(move || {
                    b.wait();
                    reclaim_stale(&cfg, &dirs, false).requeued.len()
                })
            })
            .collect();
        let wins: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
        assert_eq!(wins, 1, "round {round}");
        assert!(dirs.root.join("Race.zip").is_file());
        assert_eq!(retries(&dirs, "Race.zip").as_deref(), Some("1"));
    }
}

#[test]
fn volume_set_is_reclaimed_together() {
    let t = Tmp::new();
    let (cfg, dirs) = setup(&t);
    let vols = [
        "Big[5.0-5.3].7z.001",
        "Big[5.0-5.3].7z.002",
        "Big[5.0-5.3].7z.003",
    ];
    forge(&dirs, &vols, "some-other-host", 1, -50);
    let r = reclaim_stale(&cfg, &dirs, false);
    assert_eq!(r.requeued, vec![vols[0].to_string()]);
    for v in vols {
        assert!(dirs.root.join(v).is_file(), "{v}");
        assert!(!dirs.processing.join(v).exists(), "{v}");
    }
    assert_eq!(retries(&dirs, vols[0]).as_deref(), Some("1"));
}

#[test]
fn retry_cap_moves_the_set_to_failed() {
    let t = Tmp::new();
    let (mut cfg, dirs) = setup(&t);
    cfg.max_reclaims = 2;
    let vols = ["Loop.7z.001", "Loop.7z.002"];
    for round in 1..=3 {
        forge(&dirs, &vols, "some-other-host", 1, -50);
        let r = reclaim_stale(&cfg, &dirs, false);
        if round < 3 {
            assert_eq!(r.requeued.len(), 1, "round {round}: {:?}", r.actions);
            for v in vols {
                assert!(dirs.root.join(v).is_file());
                fs::remove_file(dirs.root.join(v)).unwrap(); // forge() recreates them
            }
        } else {
            assert_eq!(r.failed, vec![vols[0].to_string()], "{:?}", r.actions);
            for v in vols {
                assert!(dirs.failed.join(v).is_file(), "{v}");
            }
            let note = fs::read_to_string(dirs.failed.join("Loop.7z.001.error.txt")).unwrap();
            assert!(note.starts_with("STALE_RETRIES_EXCEEDED"), "{note}");
            assert!(retries(&dirs, vols[0]).is_none(), "counter reset");
        }
    }
}

#[test]
fn reclaim_lock_blocks_while_fresh_and_is_taken_over_when_stale() {
    let t = Tmp::new();
    let (cfg, dirs) = setup(&t);
    forge(&dirs, &["Half.zip"], "some-other-host", 1, -50);
    let lock = dirs.processing.join("Half.zip.reclaim");
    // Another reclaimer holds the lock right now: leave the claim alone.
    fs::write(
        &lock,
        serde_json::json!({"run_id": "x", "ts": now()}).to_string(),
    )
    .unwrap();
    let mut cfg = cfg;
    cfg.lease_secs = 100;
    let r = reclaim_stale(&cfg, &dirs, false);
    assert!(r.requeued.is_empty(), "{:?}", r.actions);
    assert!(dirs.processing.join("Half.zip.claim").is_file());
    // That reclaimer crashed long ago: take over and finish.
    fs::write(
        &lock,
        serde_json::json!({"run_id": "x", "ts": now() - 10_000}).to_string(),
    )
    .unwrap();
    let r = reclaim_stale(&cfg, &dirs, false);
    assert_eq!(r.requeued, vec!["Half.zip".to_string()], "{:?}", r.actions);
    assert!(dirs.root.join("Half.zip").is_file());
    assert!(!lock.exists());
    assert!(!dirs.processing.join("Half.zip.claim").exists());
}
