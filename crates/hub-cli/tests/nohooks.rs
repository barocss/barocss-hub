//! Issue #57: a binary built without the test-only `test-hooks` feature
//! (what `cargo build`, `cargo install` and the dist build produce) neither
//! contains nor honors any `HUB_TEST_*` hook. This test builds that binary
//! (same profile as the test run, separate target dir, `--offline`), checks
//! its bytes, and runs it with every hook set; the suite's own binary (built
//! with the feature) is run alongside to show the same settings do bite
//! there.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use hub_cli::wire::{self, Conn, Frame, Hello};
use hub_core::tree::{BlobId, Kind, NodeId, Op, Payload, ReplicaId, ROOT};

const HOOKED: &str = env!("CARGO_BIN_EXE_barocss-hub");
const T: Duration = Duration::from_secs(20);

/// Every hook the binary has ever read (README "Test hooks"): 12 that were
/// live in release builds plus the 2 debug-only ones.
const HOOKS: &[&str] = &[
    "HUB_TEST_CRASH_STAGE",
    "HUB_TEST_CRASH_IN_SYNC",
    "HUB_TEST_CRASH_IN_CHECKOUT",
    "HUB_TEST_CRASH_IN_BLOB_PUT",
    "HUB_TEST_FLUSH_LOG",
    "HUB_TEST_SYNC_STATS",
    "HUB_TEST_BLOB_BARRIER_LOG",
    "HUB_TEST_MAX_BLOB_SIZE",
    "HUB_TEST_SESSION_MS",
    "HUB_TEST_WIRE_TIMEOUT_MS",
    "HUB_TEST_FORCE_CASE_INSENSITIVE",
    "HUB_TEST_NO_TEXT_FEATURE",
    "HUB_TEST_NO_REJECTION_DEDUP",
    "HUB_TEST_RESCUE_EDIT_AFTER_SYNC",
];

/// The binary without `test-hooks`, built once per test process.
fn plain_bin() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("nohooks");
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let mut c = Command::new(cargo);
        c.args([
            "build",
            "--offline",
            "--locked",
            "-q",
            "--bin",
            "barocss-hub",
        ])
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("--target-dir")
        .arg(&target)
        .env_remove("CARGO_TARGET_DIR");
        let profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            c.arg("--release");
            "release"
        };
        let o = c.output().unwrap();
        assert!(
            o.status.success(),
            "building the hook-free binary failed: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        target
            .join(profile)
            .join(format!("barocss-hub{}", std::env::consts::EXE_SUFFIX))
    })
}

fn tmp(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "hub57-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&p).unwrap();
    fs::canonicalize(p).unwrap()
}

fn run(bin: &Path, dir: &Path, args: &[&str], env: &[(String, String)]) -> Output {
    let mut c = Command::new(bin);
    c.args(args).current_dir(dir);
    for h in HOOKS {
        c.env_remove(h);
    }
    for (k, v) in env {
        c.env(k, v);
    }
    c.output().unwrap()
}

fn ok(bin: &Path, dir: &Path, args: &[&str], env: &[(String, String)]) -> String {
    let o = run(bin, dir, args, env);
    assert!(
        o.status.success(),
        "{} {args:?}: {}{}",
        bin.display(),
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout).unwrap()
}

/// All 14 hooks, set to values that abort, log, shrink limits or flip
/// protocol features in a hooked binary.
fn all_hooks(logs: &Path) -> Vec<(String, String)> {
    let l = |n: &str| logs.join(n).display().to_string();
    [
        ("HUB_TEST_CRASH_STAGE", "ops-appended".to_string()),
        ("HUB_TEST_CRASH_IN_SYNC", "1".into()),
        ("HUB_TEST_CRASH_IN_CHECKOUT", "1".into()),
        ("HUB_TEST_CRASH_IN_BLOB_PUT", "1".into()),
        ("HUB_TEST_FLUSH_LOG", l("flush")),
        ("HUB_TEST_SYNC_STATS", l("stats")),
        ("HUB_TEST_BLOB_BARRIER_LOG", l("barrier")),
        ("HUB_TEST_MAX_BLOB_SIZE", "1000".into()),
        ("HUB_TEST_SESSION_MS", "1".into()),
        ("HUB_TEST_WIRE_TIMEOUT_MS", "1".into()),
        ("HUB_TEST_FORCE_CASE_INSENSITIVE", "1".into()),
        ("HUB_TEST_NO_TEXT_FEATURE", "1".into()),
        ("HUB_TEST_NO_REJECTION_DEDUP", "1".into()),
        ("HUB_TEST_RESCUE_EDIT_AFTER_SYNC", "late".into()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

#[test]
fn plain_binary_contains_no_hook_names() {
    let bytes = fs::read(plain_bin()).unwrap();
    let hit = bytes.windows(9).position(|w| w == b"HUB_TEST_");
    assert!(
        hit.is_none(),
        "hook-free binary contains HUB_TEST_ at byte {hit:?}"
    );
    // Control: the suite's binary does contain them (the last two only in
    // debug builds).
    let hooked = fs::read(HOOKED).unwrap();
    let n = if cfg!(debug_assertions) {
        HOOKS.len()
    } else {
        12
    };
    for h in &HOOKS[..n] {
        assert!(
            hooked.windows(h.len()).any(|w| w == h.as_bytes()),
            "hooked binary lacks {h}"
        );
    }
}

/// Local CLI with every hook set: record, sync, checkout succeed (no
/// abort), no instrumentation log is written, the trees converge. The
/// hooked binary, given the crash stage or the flush log alone, aborts or
/// logs.
#[test]
fn plain_binary_ignores_every_hook_locally() {
    let bin = plain_bin();
    let t = tmp("local");
    let logs = t.join("logs");
    fs::create_dir_all(&logs).unwrap();
    let env = all_hooks(&logs);
    let (a, b) = (t.join("a"), t.join("b"));
    for d in [&a, &b] {
        fs::create_dir_all(d).unwrap();
        ok(bin, d, &["init"], &env);
    }
    fs::write(a.join("f.txt"), "one\n").unwrap();
    fs::write(a.join("big.bin"), vec![7u8; 5000]).unwrap();
    ok(bin, &a, &["record", "-m", "base"], &env);
    ok(bin, &a, &["publish", "base"], &env);
    let s = ok(bin, &b, &["sync", a.to_str().unwrap()], &env);
    assert!(s.contains("received"), "{s}");
    ok(bin, &b, &["checkout"], &env);
    assert_eq!(fs::read(b.join("f.txt")).unwrap(), b"one\n");
    assert_eq!(fs::read(b.join("big.bin")).unwrap(), vec![7u8; 5000]);
    let left: Vec<_> = fs::read_dir(&logs).unwrap().collect();
    assert!(left.is_empty(), "hook logs written: {left:?}");
    assert!(!a.join("late").exists() && !b.join("late").exists());

    // Control: the same settings bite in the hooked binary.
    let c = t.join("c");
    fs::create_dir_all(&c).unwrap();
    ok(Path::new(HOOKED), &c, &["init"], &[]);
    fs::write(c.join("f.txt"), "one\n").unwrap();
    let crash = [("HUB_TEST_CRASH_STAGE".into(), "ops-appended".into())];
    let o = run(Path::new(HOOKED), &c, &["record", "-m", "x"], &crash);
    assert!(!o.status.success(), "hooked binary did not abort");
    let flush = [(
        "HUB_TEST_FLUSH_LOG".into(),
        logs.join("flush").display().to_string(),
    )];
    ok(Path::new(HOOKED), &c, &["record", "-m", "x"], &flush);
    assert!(
        logs.join("flush").exists(),
        "hooked binary wrote no flush log"
    );
}

fn serve(bin: &Path, root: &Path, env: &[(String, String)]) -> (Child, Conn) {
    let mut c = Command::new(bin);
    c.args(["serve", "--stdio"]).arg(root);
    for h in HOOKS {
        c.env_remove(h);
    }
    for (k, v) in env {
        c.env(k, v);
    }
    let mut ch = c
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let conn = Conn::from_io(ch.stdout.take().unwrap(), ch.stdin.take().unwrap());
    (ch, conn)
}

fn next(c: &mut Conn) -> Frame {
    loop {
        match c.recv(T).unwrap() {
            Frame::Progress { .. } | Frame::Error { fatal: false, .. } => continue,
            f => return f,
        }
    }
}

fn hello(c: &mut Conn) -> Hello {
    let mut h = Hello::new(ReplicaId(0x5757), false);
    h.features.retain(|f| f != wire::F_DIGEST);
    c.send_magic().unwrap();
    c.send(&Frame::Hello(h)).unwrap();
    c.recv_magic(T).unwrap();
    match next(c) {
        Frame::Hello(h) => {
            c.theirs = h.limits;
            h
        }
        f => panic!("expected Hello, got {f:?}"),
    }
}

/// `serve --stdio` with every hook set, a slow client (past the 1 ms
/// session and wire timeouts) and a blob over the 1000-byte hook limit:
/// the session completes, the blob is stored, and `Hello` carries `text`.
#[test]
fn plain_binary_ignores_every_hook_on_the_wire() {
    let bin = plain_bin();
    let t = tmp("wire");
    let logs = t.join("logs");
    fs::create_dir_all(&logs).unwrap();
    let env = all_hooks(&logs);
    let d = t.join("srv");
    fs::create_dir_all(&d).unwrap();
    ok(bin, &d, &["init"], &[]);
    fs::write(d.join("s.txt"), "server\n").unwrap();
    ok(bin, &d, &["record", "-m", "s"], &[]);
    ok(bin, &d, &["publish", "s"], &[]);

    let (mut ch, mut c) = serve(bin, &d, &env);
    std::thread::sleep(Duration::from_millis(200));
    let h = hello(&mut c);
    assert!(h.features.iter().any(|f| f == wire::F_TEXT), "{h:?}");
    std::thread::sleep(Duration::from_millis(50));
    // Pull half: take nothing.
    c.send(&Frame::Have { frontier: vec![] }).unwrap();
    loop {
        match next(&mut c) {
            Frame::Ops { done, .. } => {
                c.send(&Frame::NeedBlobs(vec![])).unwrap();
                if done {
                    break;
                }
            }
            f => panic!("pull: {f:?}"),
        }
    }
    c.send(&Frame::Done("ops".into())).unwrap();
    c.send(&Frame::Committed(vec![], vec![])).unwrap();
    // Push half: one 200 kB blob.
    let bytes = vec![3u8; 200_000];
    let id = BlobId::of(&bytes);
    let n = NodeId::new(10, 0x5757);
    let ops = vec![
        Op {
            id: n,
            deps: Default::default(),
            payload: Payload::Create {
                kind: Kind::File,
                parent: ROOT,
                name: "blobfile".into(),
            },
        },
        Op {
            id: NodeId::new(11, 0x5757),
            deps: [n].into_iter().collect(),
            payload: Payload::SetContent { node: n, blob: id },
        },
    ];
    match next(&mut c) {
        Frame::Have { .. } => {}
        f => panic!("expected Have, got {f:?}"),
    }
    c.send(&Frame::Ops {
        frontier: vec![],
        done: true,
        ops,
    })
    .unwrap();
    loop {
        match next(&mut c) {
            Frame::NeedBlobs(v) => {
                for (bid, off) in v {
                    assert_eq!(bid, id);
                    c.send(&Frame::BlobBegin {
                        id,
                        size: bytes.len() as u64,
                        offset: off,
                    })
                    .unwrap();
                    for chunk in bytes[off as usize..].chunks(64 * 1024) {
                        c.send(&Frame::chunk(chunk.to_vec())).unwrap();
                    }
                    c.send(&Frame::BlobEnd(id)).unwrap();
                }
            }
            Frame::Need(_) => c
                .send(&Frame::Ops {
                    frontier: vec![],
                    done: true,
                    ops: vec![],
                })
                .unwrap(),
            Frame::Done(p) if p == "ops" => break,
            f => panic!("push: {f:?}"),
        }
    }
    match next(&mut c) {
        Frame::Committed(..) => {}
        f => panic!("expected Committed, got {f:?}"),
    }
    let _ = ch.kill();
    let _ = ch.wait();
    assert!(d.join(".hub/blobs").join(id.hex()).exists(), "blob refused");
    let left: Vec<_> = fs::read_dir(&logs).unwrap().collect();
    assert!(left.is_empty(), "hook logs written: {left:?}");

    // Control: the hooked binary drops `text` from its Hello.
    let only = [("HUB_TEST_NO_TEXT_FEATURE".into(), "1".into())];
    let (mut ch, mut c) = serve(Path::new(HOOKED), &d, &only);
    let h = hello(&mut c);
    assert!(!h.features.iter().any(|f| f == wire::F_TEXT), "{h:?}");
    let _ = ch.kill();
    let _ = ch.wait();
}
