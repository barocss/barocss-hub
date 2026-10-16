//! Issue #43, Remote-Sync §5.12: protocol properties and adversarial peers.
//! A scripted client speaks raw frames to a real `serve --stdio` process
//! (or to an in-process server on the in-memory transport).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Barrier, Mutex};
use std::time::Duration;

use hub_cli::session::{self, Access, Side};
use hub_cli::store::LockMode;
use hub_cli::wire::{self, Conn, Frame, Hello, Limits};
use hub_cli::{Code, Error, Repo};
use hub_core::sync::{RejectReason, SyncReplica};
use hub_core::tree::{BlobId, Kind, NodeId, Op, Payload, ReplicaId, ROOT};

const BIN: &str = env!("CARGO_BIN_EXE_barocss-hub");
const T: Duration = Duration::from_secs(20);

fn tmp(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "hub43w-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&p).unwrap();
    fs::canonicalize(p).unwrap()
}

fn ok(dir: &Path, args: &[&str]) -> String {
    let o = Command::new(BIN)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "hub {args:?}: {}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout).unwrap()
}

fn head(d: &Path) -> String {
    ok(d, &["status"])
        .lines()
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .to_string()
}

fn replica_of(d: &Path) -> ReplicaId {
    let t = fs::read_to_string(d.join(".hub/replica")).unwrap();
    ReplicaId(u128::from_str_radix(t.lines().nth(1).unwrap(), 16).unwrap())
}

fn files(d: &Path) -> BTreeMap<PathBuf, (Vec<u8>, std::time::SystemTime)> {
    let mut m = BTreeMap::new();
    let mut st = vec![d.to_path_buf()];
    while let Some(x) = st.pop() {
        for e in fs::read_dir(&x).unwrap() {
            let e = e.unwrap();
            let md = fs::symlink_metadata(e.path()).unwrap();
            if md.is_dir() {
                st.push(e.path());
                m.insert(e.path(), (Vec::new(), md.modified().unwrap()));
            } else {
                m.insert(
                    e.path(),
                    (fs::read(e.path()).unwrap(), md.modified().unwrap()),
                );
            }
        }
    }
    m
}

fn cp(from: &Path, to: &Path) {
    assert!(Command::new("cp")
        .arg("-R")
        .arg(from)
        .arg(to)
        .status()
        .unwrap()
        .success());
}

fn serve(root: &Path, ro: bool) -> (Child, Conn) {
    let mut c = Command::new(BIN);
    c.arg("serve").arg("--stdio");
    if ro {
        c.arg("--read-only");
    }
    let mut ch = c
        .arg(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let conn = Conn::from_io(ch.stdout.take().unwrap(), ch.stdin.take().unwrap());
    (ch, conn)
}

fn done(mut ch: Child) {
    let _ = ch.kill();
    let _ = ch.wait();
}

fn rerr(code: &str, msg: &str) -> Error {
    Error::remote(wire::code_of(code), msg.to_string())
}

/// Next frame that is not `Progress`; a fatal remote `Error` is an `Err`.
fn next(c: &mut Conn) -> Result<Frame, Error> {
    loop {
        match c.recv(T)? {
            Frame::Progress { .. } | Frame::Error { fatal: false, .. } => continue,
            Frame::Error {
                code,
                message,
                fatal: true,
            } => return Err(rerr(&code, &message)),
            f => return Ok(f),
        }
    }
}

fn hello_with(c: &mut Conn, h: Hello) -> Result<Hello, Error> {
    c.send_magic()?;
    c.send(&Frame::Hello(h))?;
    c.recv_magic(T)?;
    match next(c)? {
        Frame::Hello(h) => {
            c.theirs = h.limits;
            Ok(h)
        }
        f => panic!("expected Hello, got {f:?}"),
    }
}

/// The scripted peers of these tests speak the pre-#46 protocol (no
/// `digest` feature, so no `Sums` frame): they double as the old-peer
/// compatibility check for Issue #46.
fn old_hello(me: ReplicaId, ro: bool) -> Hello {
    let mut h = Hello::new(me, ro);
    h.features.retain(|f| f != wire::F_DIGEST);
    h
}

fn hello(c: &mut Conn, me: ReplicaId) -> Result<Hello, Error> {
    hello_with(c, old_hello(me, false))
}

/// The pull half, receiving (and ignoring) everything.
fn pull(c: &mut Conn) -> Result<(), Error> {
    c.send(&Frame::Have { frontier: vec![] })?;
    loop {
        match next(c)? {
            Frame::Ops { done, .. } => {
                c.send(&Frame::NeedBlobs(vec![]))?;
                if done {
                    break;
                }
            }
            f => panic!("pull: {f:?}"),
        }
    }
    c.send(&Frame::Done("ops".into()))?;
    c.send(&Frame::Committed(vec![], vec![]))
}

type BlobServe<'a> = dyn FnMut(&mut Conn, BlobId, u64) -> Result<(), Error> + 'a;

fn stream(c: &mut Conn, id: BlobId, bytes: &[u8], off: u64) -> Result<(), Error> {
    c.send(&Frame::BlobBegin {
        id,
        size: bytes.len() as u64,
        offset: off,
    })?;
    for ch in bytes[off as usize..].chunks(64 * 1024) {
        c.send(&Frame::chunk(ch.to_vec()))?;
    }
    c.send(&Frame::BlobEnd(id))
}

/// The push half: answer the server's `Have` with `ops`; blobs via `serve`.
fn push(c: &mut Conn, ops: Vec<Op>, serve: &mut BlobServe<'_>) -> Result<Vec<NodeId>, Error> {
    push_full(c, ops, serve).map(|(f, _)| f)
}

type Rejected = Vec<(NodeId, String)>;

/// [`push`], also returning the rejections of the server's `Committed`.
fn push_full(
    c: &mut Conn,
    ops: Vec<Op>,
    serve: &mut BlobServe<'_>,
) -> Result<(Vec<NodeId>, Rejected), Error> {
    match next(c)? {
        Frame::Have { .. } => {}
        f => panic!("push: expected Have, got {f:?}"),
    }
    c.send(&Frame::Ops {
        frontier: vec![],
        done: true,
        ops,
    })?;
    loop {
        match next(c)? {
            Frame::NeedBlobs(v) => {
                for (id, off) in v {
                    serve(c, id, off)?;
                }
            }
            Frame::Need(_) => c.send(&Frame::Ops {
                frontier: vec![],
                done: true,
                ops: vec![],
            })?,
            Frame::Done(p) if p == "ops" => break,
            f => panic!("push: {f:?}"),
        }
    }
    match next(c)? {
        Frame::Committed(f, r) => Ok((f, r)),
        f => panic!("push: expected Committed, got {f:?}"),
    }
}

fn finish(c: &mut Conn) -> Result<(), Error> {
    c.send(&Frame::Done("meta".into()))?;
    loop {
        match next(c)? {
            Frame::Checkpoints(_) => {}
            Frame::Done(_) => break,
            f => panic!("meta: {f:?}"),
        }
    }
    c.send(&Frame::Committed(vec![], vec![]))?;
    match next(c)? {
        Frame::Committed(..) => {}
        f => panic!("{f:?}"),
    }
    c.send(&Frame::Bye)?;
    assert_eq!(next(c)?, Frame::Bye);
    Ok(())
}

fn no_blobs(_: &mut Conn, _: BlobId, _: u64) -> Result<(), Error> {
    panic!("no blob expected")
}

fn create(c: u64, r: u128, deps: &[NodeId], name: &str) -> Op {
    Op {
        id: NodeId::new(c, r),
        deps: deps.iter().copied().collect(),
        payload: Payload::Create {
            kind: Kind::File,
            parent: ROOT,
            name: name.into(),
        },
    }
}

fn set(c: u64, r: u128, node: NodeId, blob: BlobId) -> Op {
    Op {
        id: NodeId::new(c, r),
        deps: [node].into_iter().collect(),
        payload: Payload::SetContent { node, blob },
    }
}

/// A repository with one file, as a served root.
fn server_repo(tag: &str) -> PathBuf {
    let t = tmp(tag);
    let d = t.join("srv");
    fs::create_dir_all(&d).unwrap();
    ok(&d, &["init"]);
    fs::write(d.join("s.txt"), "server\n").unwrap();
    ok(&d, &["record", "-m", "s"]);
    ok(&d, &["publish", "s"]);
    d
}

const X: u128 = 0x1111;
const Y: u128 = 0x2222;

// ------------------------------------------------------------ P22, R7

#[test]
fn p22_read_only_server_refuses_every_write_frame_and_is_untouched() {
    let d = server_repo("ro");
    let before = files(&d.join(".hub"));
    let writes: Vec<Frame> = vec![
        Frame::Ops {
            frontier: vec![],
            done: true,
            ops: vec![create(5, X, &[], "x")],
        },
        Frame::BlobBegin {
            id: BlobId::of(b"x"),
            size: 1,
            offset: 0,
        },
        Frame::chunk(b"x".to_vec()),
        Frame::Checkpoints(vec![]),
    ];
    for w in writes {
        let (ch, mut c) = serve(&d, true);
        let h = hello(&mut c, ReplicaId(X)).unwrap();
        assert!(h.has("read-only"));
        pull(&mut c).unwrap();
        c.send(&w).unwrap();
        // The server may already be sending its meta frames.
        let e = loop {
            match next(&mut c) {
                Ok(_) => continue,
                Err(e) => break e,
            }
        };
        assert_eq!(e.code(), Code::Forbidden, "{w:?}: {e}");
        done(ch);
        assert_eq!(files(&d.join(".hub")), before, "{w:?} changed the repo");
    }
    // A well-behaved client: pull only, no Have from the server.
    let (ch, mut c) = serve(&d, true);
    hello(&mut c, ReplicaId(X)).unwrap();
    pull(&mut c).unwrap();
    c.send(&Frame::Done("meta".into())).unwrap();
    loop {
        match next(&mut c).unwrap() {
            Frame::Checkpoints(v) => assert_eq!(v.len(), 1),
            Frame::Done(_) => break,
            f => panic!("{f:?}"),
        }
    }
    c.send(&Frame::Committed(vec![], vec![])).unwrap();
    assert!(matches!(next(&mut c).unwrap(), Frame::Committed(..)));
    c.send(&Frame::Bye).unwrap();
    assert_eq!(next(&mut c).unwrap(), Frame::Bye);
    done(ch);
    assert_eq!(files(&d.join(".hub")), before);
}

// ------------------------------------------------------------ P8, P18, P20

#[test]
fn p8_p18_p20_hello_checks_persist_nothing() {
    let d = server_repo("hello");
    let srv = replica_of(&d);
    let before = files(&d.join(".hub"));
    let bad = |mut h: Hello| {
        let (ch, mut c) = serve(&d, false);
        let r = (|| {
            hello_with(&mut c, h.clone())?;
            next(&mut c)
        })();
        done(ch);
        h.features.clear();
        r.unwrap_err()
    };
    let mut h = old_hello(ReplicaId(X), false);
    h.limits.frame = 1;
    assert_eq!(bad(h).code(), Code::Protocol);
    let mut h = old_hello(ReplicaId(X), false);
    h.limits.chunk = 0;
    assert_eq!(bad(h).code(), Code::Protocol);
    let mut h = old_hello(ReplicaId(X), false);
    h.limits.ops = 0;
    assert_eq!(bad(h).code(), Code::Protocol);
    let mut h = old_hello(ReplicaId(X), false);
    h.proto_min = 2;
    h.proto_max = 2;
    let e = bad(h);
    assert_eq!(e.code(), Code::VersionMismatch);
    assert!(e.0.contains("1..1") && e.0.contains("2..2"), "{e}");
    // P20: a client announcing the server's own replica id.
    let e = bad(old_hello(srv, false));
    assert_eq!(e.code(), Code::DuplicateReplica);
    assert_eq!(
        files(&d.join(".hub")),
        before,
        "a refused Hello wrote something"
    );
    // No magic at all: the server closes.
    let (mut ch, mut c) = serve(&d, false);
    c.send_raw(b"garbage-not-magic").ok();
    let st = ch.wait().unwrap();
    assert!(!st.success());
    let _ = c;
    // An op authored under the server's id is evidence: persisted warning.
    let (ch, mut c) = serve(&d, false);
    hello(&mut c, ReplicaId(X)).unwrap();
    pull(&mut c).unwrap();
    let e = push(
        &mut c,
        vec![create(90, srv.0, &[], "forged")],
        &mut no_blobs,
    )
    .unwrap_err();
    assert_eq!(e.code(), Code::DuplicateReplica);
    done(ch);
    let w = ok(&d, &["warnings"]);
    assert!(w.contains("duplicate-replica"), "{w}");
    assert!(!ok(&d, &["ls"]).contains("forged"));
}

// ------------------------------------------------------------ P6, P18, P21

fn blob_case(d: &Path, serve_fn: &mut BlobServe<'_>) -> Result<(), Error> {
    let (ch, mut c) = serve(d, false);
    let r = (|| {
        hello(&mut c, ReplicaId(X))?;
        pull(&mut c)?;
        let big = BlobId::of(&vec![3u8; 200_000]);
        let n = create(10, X, &[], "blobfile");
        let s = set(11, X, n.id, big);
        push(&mut c, vec![n, s], serve_fn)?;
        finish(&mut c)
    })();
    done(ch);
    r
}

fn no_wire_leftovers(d: &Path, id: &BlobId) {
    let b = d.join(".hub/blobs");
    assert!(!b.join(id.hex()).exists());
    for e in fs::read_dir(&b).unwrap() {
        let n = e.unwrap().file_name().into_string().unwrap();
        assert!(!n.starts_with("rx-"), "{n} left behind");
    }
}

#[test]
fn p6_p18_blob_framing_and_integrity() {
    let bytes = vec![3u8; 200_000];
    let id = BlobId::of(&bytes);
    let d = server_repo("blobs");
    let log0 = fs::read(d.join(".hub/ops.log")).unwrap();
    type Case<'a> = (&'a str, Code, Box<BlobServe<'a>>);
    let cases: Vec<Case> = vec![
        (
            "unsolicited id",
            Code::Protocol,
            Box::new(|c: &mut Conn, _, _| stream(c, BlobId::of(b"other"), b"other", 0)),
        ),
        (
            "offset > size",
            Code::Protocol,
            Box::new(|c: &mut Conn, id, _| {
                c.send(&Frame::BlobBegin {
                    id,
                    size: 5,
                    offset: 9,
                })
            }),
        ),
        (
            "chunks past size",
            Code::Protocol,
            Box::new(|c: &mut Conn, id, _| {
                c.send(&Frame::BlobBegin {
                    id,
                    size: 3,
                    offset: 0,
                })?;
                c.send(&Frame::chunk(vec![1u8; 10]))?;
                c.send(&Frame::BlobEnd(id))
            }),
        ),
        (
            "end before size",
            Code::Protocol,
            Box::new(|c: &mut Conn, id, _| {
                c.send(&Frame::BlobBegin {
                    id,
                    size: 30,
                    offset: 0,
                })?;
                c.send(&Frame::chunk(vec![1u8; 10]))?;
                c.send(&Frame::BlobEnd(id))
            }),
        ),
        (
            "flipped chunk byte",
            Code::Corrupt,
            Box::new(|c: &mut Conn, id, _| {
                c.send(&Frame::BlobBegin {
                    id,
                    size: 200_000,
                    offset: 0,
                })?;
                let mut f = Frame::chunk(vec![3u8; 200_000 / 2]);
                if let Frame::Chunk { data, .. } = &mut f {
                    data[7] ^= 1;
                }
                c.send(&f)?;
                c.send(&Frame::chunk(vec![3u8; 200_000 / 2]))?;
                c.send(&Frame::BlobEnd(id))
            }),
        ),
    ];
    for (name, code, mut f) in cases {
        let e = blob_case(&d, &mut *f).unwrap_err();
        assert_eq!(e.code(), code, "{name}: {e}");
        no_wire_leftovers(&d, &id);
        assert_eq!(fs::read(d.join(".hub/ops.log")).unwrap(), log0, "{name}");
    }
    // Chunks hash correctly but the content is not the blob: CorruptBlob
    // warning, ops pending, the session succeeds (like receive_blob).
    let wrong = vec![4u8; 200_000];
    blob_case(&d, &mut |c, id, off| stream(c, id, &wrong, off)).unwrap();
    no_wire_leftovers(&d, &id);
    let w = ok(&d, &["warnings"]);
    assert!(
        w.contains("corrupt-blob") && w.contains("pending op"),
        "{w}"
    );
    // The right bytes later: applied, warning's op no longer stuck.
    blob_case(&d, &mut |c, id, off| stream(c, id, &bytes, off)).unwrap();
    assert!(ok(&d, &["ls"]).contains("blobfile"));
    assert!(d.join(".hub/blobs").join(id.hex()).exists());
}

#[test]
fn p21_part_from_another_peer_is_retried_from_zero_once() {
    let bytes = vec![3u8; 200_000];
    let id = BlobId::of(&bytes);
    // A kept prefix that came from a different (lying) peer.
    let d = server_repo("part");
    fs::create_dir_all(d.join(".hub/blobs")).unwrap();
    fs::write(
        d.join(".hub/blobs").join(format!("part-{}", id.hex())),
        vec![9u8; 70_000],
    )
    .unwrap();
    let offsets = Arc::new(Mutex::new(Vec::new()));
    let o2 = offsets.clone();
    blob_case(&d, &mut |c, id, off| {
        o2.lock().unwrap().push(off);
        stream(c, id, &bytes, off)
    })
    .unwrap();
    assert_eq!(*offsets.lock().unwrap(), vec![70_000, 0]);
    let w = ok(&d, &["warnings"]);
    assert!(!w.contains("corrupt-blob"), "{w}");
    assert!(ok(&d, &["ls"]).contains("blobfile"));
    // Both transfers wrong: exactly one CorruptBlob naming the peer.
    let d = server_repo("part2");
    fs::create_dir_all(d.join(".hub/blobs")).unwrap();
    fs::write(
        d.join(".hub/blobs").join(format!("part-{}", id.hex())),
        vec![9u8; 70_000],
    )
    .unwrap();
    let wrong = vec![5u8; 200_000];
    blob_case(&d, &mut |c, id, off| stream(c, id, &wrong, off)).unwrap();
    let ws = hub_cli::sync::read_warnings(&d.join(".hub")).unwrap();
    let cb: Vec<_> = ws
        .iter()
        .filter(|w| w.reason == RejectReason::CorruptBlob)
        .collect();
    assert_eq!(cb.len(), 1, "{ws:?}");
    assert_eq!(cb[0].peer, ReplicaId(X));
}

// ------------------------------------------------------------ P14

#[test]
fn p14_malicious_ops_are_validated_exactly_like_local_sync() {
    let d = server_repo("p14");
    let repo = Repo::open_dir_with(d.join(".hub"), LockMode::Shared).unwrap();
    let existing = repo.store.values().next().unwrap().clone();
    let store = repo.store.clone();
    let srv = repo.replica_id;
    drop(repo);
    let a = create(20, X, &[], "a");
    // Equivocation: an existing id with another payload (here authored
    // under the server's own id: since #46 also a DuplicateReplica, which
    // ends the session; checked separately below).
    let equiv = Op {
        payload: Payload::Delete { node: ROOT },
        ..existing.clone()
    };
    let evil = vec![
        a.clone(),
        // Lamport violation: a dep not < the op.
        create(21, X, &[NodeId::new(30, X)], "lam"),
        create(30, X, &[a.id], "dep-of-lam"),
        // §2.2-invalid name: accepted, an invalid-op conflict.
        create(31, X, &[a.id], "bad/name"),
        create(32, X, &[a.id], &"n".repeat(5000)),
    ];
    // Local reference: the same ops into an in-memory replica.
    let mut local = SyncReplica::from_parts(
        srv,
        store,
        hub_core::tree::BlobStore::new(),
        vec![],
        vec![],
        vec![],
    );
    for o in &evil {
        local.receive(ReplicaId(X), o.clone());
    }
    let key = |ws: &[hub_core::sync::RejectWarning]| -> BTreeSet<(String, Option<NodeId>)> {
        ws.iter()
            .map(|w| (format!("{:?}", w.reason), w.rejected))
            .collect()
    };
    let (ch, mut c) = serve(&d, false);
    hello(&mut c, ReplicaId(X)).unwrap();
    pull(&mut c).unwrap();
    push(&mut c, evil.clone(), &mut no_blobs).unwrap();
    finish(&mut c).unwrap();
    done(ch);
    let got = hub_cli::sync::read_warnings(&d.join(".hub")).unwrap();
    assert_eq!(key(&got), key(&local.warnings()));
    assert_eq!(key(&got).len(), 1);
    let r = Repo::open_dir_with(d.join(".hub"), LockMode::Shared).unwrap();
    let mine: BTreeSet<NodeId> = r.store.keys().copied().collect();
    let theirs: BTreeSet<NodeId> = local.store().keys().copied().collect();
    assert_eq!(mine, theirs);
    assert_eq!(r.conflicts(), local.conflicts());
    assert!(format!("{:?}", r.conflicts()).contains("Invalid"));
    drop(r);
    // Issue #46: the equivocating op under the server's id, validated as
    // the in-memory replica does: equivocation + duplicate-replica, the
    // session ends with `duplicate-replica`, no op is appended.
    local.receive(ReplicaId(X), equiv.clone());
    let log0 = fs::read(d.join(".hub/ops.log")).unwrap();
    let (ch, mut c) = serve(&d, false);
    hello(&mut c, ReplicaId(X)).unwrap();
    pull(&mut c).unwrap();
    let e = push(&mut c, vec![equiv.clone()], &mut no_blobs).unwrap_err();
    done(ch);
    assert_eq!(e.code(), Code::DuplicateReplica, "{e:?}");
    let got = hub_cli::sync::read_warnings(&d.join(".hub")).unwrap();
    assert_eq!(key(&got), key(&local.warnings()));
    assert!(key(&got).contains(&("Equivocation".into(), Some(equiv.id))));
    assert!(key(&got).contains(&("DuplicateReplica".into(), Some(equiv.id))));
    assert_eq!(fs::read(d.join(".hub/ops.log")).unwrap(), log0);
}

// ------------------------------------------------------------ P16

#[test]
fn p16_concurrent_serve_sessions_merge_pending_warnings_and_equivocation() {
    for round in 0..3 {
        let d = server_repo(&format!("p16-{round}"));
        let bar = Arc::new(Barrier::new(2));
        let e_id = 40u64;
        let run = |me: u128, ops: Vec<Op>, bar: Arc<Barrier>, d: PathBuf| {
            std::thread::spawn(move || {
                let (ch, mut c) = serve(&d, false);
                let r = (|| {
                    hello(&mut c, ReplicaId(me))?;
                    pull(&mut c)?;
                    bar.wait();
                    push(&mut c, ops, &mut no_blobs)?;
                    finish(&mut c)
                })();
                done(ch);
                r
            })
        };
        // Same op id (author Z) with different payloads from X and Y; X
        // also sends an op with a missing dep, Y a Lamport violation.
        let z = 0x3333u128;
        let ex = create(e_id, z, &[], "from-x");
        let ey = create(e_id, z, &[], "from-y");
        let pend = create(50, X, &[NodeId::new(49, 0x9999)], "pending");
        let lam = create(51, Y, &[NodeId::new(60, Y)], "lamport");
        let lamdep = create(60, Y, &[], "lamdep");
        let hx = run(X, vec![ex, pend.clone()], bar.clone(), d.clone());
        let hy = run(Y, vec![ey, lamdep, lam.clone()], bar.clone(), d.clone());
        hx.join().unwrap().unwrap();
        hy.join().unwrap().unwrap();
        let hub = d.join(".hub");
        let pending = hub_cli::sync::read_pending(&hub).unwrap();
        assert!(
            pending.iter().any(|(o, _)| o.id == pend.id),
            "lost pending op"
        );
        let ws = hub_cli::sync::read_warnings(&hub).unwrap();
        assert!(ws
            .iter()
            .any(|w| w.reason == RejectReason::LamportViolation && w.rejected == Some(lam.id)));
        let eq: Vec<_> = ws
            .iter()
            .filter(|w| w.reason == RejectReason::Equivocation)
            .collect();
        assert_eq!(eq.len(), 1, "{ws:?}");
        let r = Repo::open_dir_with(hub.clone(), LockMode::Shared).unwrap();
        let kept = &r.store[&NodeId::new(e_id, z)];
        let Payload::Create { name, .. } = &kept.payload else {
            panic!()
        };
        // The warning names the peer whose copy lost (persisted later).
        let loser = if name == "from-x" { Y } else { X };
        assert_eq!(eq[0].peer, ReplicaId(loser));
    }
}

// ------------------------------------------------------------ P19

#[test]
fn p19_record_sizes() {
    let d = server_repo("p19");
    let long = "n".repeat(4097);
    let o = Command::new(BIN)
        .args(["mkdir", &long, "--json"])
        .current_dir(&d)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&o.stderr).contains("\"invalid-path\""));
    let o = Command::new(BIN)
        .args(["commit", "-m", &"m".repeat(65 * 1024)])
        .current_dir(&d)
        .output()
        .unwrap();
    assert!(!o.status.success());
    // The longest legal op record fits in one frame at the floor.
    let big = create(9, X, &[NodeId::new(8, X); 1], &"%".repeat(4096));
    assert!(hub_cli::store::encode_op(&big).len() < 256 * 1024);
    // A pre-#43 oversized record: `limit` naming the op, not a hang.
    let mut repo = Repo::discover(&d).unwrap();
    let huge = create(1000, X, &[], &"h".repeat(300_000));
    repo.append_op(huge.clone()).unwrap();
    drop(repo);
    let (ch, mut c) = serve(&d, false);
    let mut h = old_hello(ReplicaId(X), false);
    h.limits = Limits {
        frame: wire::FRAME_FLOOR,
        chunk: wire::CHUNK_FLOOR,
        ops: 1,
    };
    c.ours = h.limits;
    hello_with(&mut c, h).unwrap();
    c.send(&Frame::Have { frontier: vec![] }).unwrap();
    let e = (|| -> Result<(), Error> {
        loop {
            match next(&mut c)? {
                Frame::Ops { .. } => c.send(&Frame::NeedBlobs(vec![]))?,
                f => panic!("{f:?}"),
            }
        }
    })()
    .unwrap_err();
    done(ch);
    assert_eq!(e.code(), Code::Limit);
    assert!(e.0.contains(&hub_cli::store::fmt_id(huge.id)), "{e}");
}

// ------------------------------------------------------------ P2, P3, P5, P9

/// A client repo (with a private and a published checkpoint and a
/// 3-chunk blob) and a bare server holding other ops.
fn scenario(tag: &str) -> PathBuf {
    let t = tmp(tag);
    let (a, b) = (t.join("a"), t.join("b"));
    for x in [&a, &b] {
        fs::create_dir_all(x).unwrap();
        ok(x, &["init"]);
    }
    ok(&t, &["init", "--bare", "srv"]);
    fs::write(a.join("big.bin"), vec![1u8; 700_000]).unwrap();
    fs::write(a.join("a.txt"), "a\n").unwrap();
    ok(&a, &["record", "-m", "private-secret-msg"]);
    fs::write(a.join("a.txt"), "a2\n").unwrap();
    ok(&a, &["record", "-m", "public"]);
    ok(&a, &["publish", "public"]);
    fs::write(b.join("b.txt"), "b\n").unwrap();
    ok(&b, &["record", "-m", "bpub"]);
    ok(&b, &["publish", "bpub"]);
    ok(&b, &["sync", t.join("srv").to_str().unwrap()]);
    t
}

struct Run {
    client: Result<session::Report, Error>,
    server: Result<(), Error>,
    out: Vec<Vec<u8>>,
    back: Vec<Vec<u8>>,
    server_blob_bytes: u64,
}

/// One in-process session: client `a` (held), server `srv` (per step).
fn run_session(t: &Path, cut: Option<(bool, usize)>) -> Run {
    let repo = Repo::open_dir_with(t.join("a/.hub"), LockMode::Exclusive).unwrap();
    let mut cs = Side::new(Access::Held(Box::new(repo)), false).unwrap();
    let mut ss = Side::new(Access::Path(t.join("srv/.hub")), false).unwrap();
    let (mut x, mut y, _cut) = wire::pair();
    let (tx, ty) = (
        Arc::new(Mutex::new(Vec::new())),
        Arc::new(Mutex::new(Vec::new())),
    );
    x.transcript = Some(tx.clone());
    y.transcript = Some(ty.clone());
    match cut {
        Some((true, k)) => x.cut_after(k),
        Some((false, k)) => y.cut_after(k),
        None => {}
    }
    let th = std::thread::spawn(move || {
        let r = session::server(&mut y, &mut ss);
        (r, ss.blob_bytes_in)
    });
    let client = session::client(&mut x, &mut cs);
    drop(x);
    drop(cs);
    let (server, server_blob_bytes) = th.join().unwrap();
    let out = tx.lock().unwrap().clone();
    let back = ty.lock().unwrap().clone();
    Run {
        client,
        server,
        out,
        back,
        server_blob_bytes,
    }
}

fn copy_scenario(base: &Path, tag: &str) -> PathBuf {
    let t = tmp(tag);
    for n in ["a", "srv"] {
        cp(&base.join(n), &t.join(n));
    }
    t
}

#[test]
fn p2_p9_transcript_is_deterministic_and_private() {
    let base = scenario("p2");
    let (t1, t2) = (copy_scenario(&base, "p2a"), copy_scenario(&base, "p2b"));
    let r1 = run_session(&t1, None);
    let r2 = run_session(&t2, None);
    r1.client.as_ref().unwrap();
    r1.server.as_ref().unwrap();
    assert_eq!(r1.out, r2.out);
    assert_eq!(r1.back, r2.back);
    for f in r1.out.iter().chain(&r1.back) {
        let s = String::from_utf8_lossy(f);
        assert!(
            !s.contains("private-secret-msg"),
            "private checkpoint on the wire"
        );
    }
    assert!(r1
        .out
        .iter()
        .any(|f| String::from_utf8_lossy(f).contains("public")));
    assert_eq!(head(&t1.join("a")), head(&t1.join("srv")));
}

/// P3/P4: cut after every frame, in each direction; both repositories stay
/// valid, nothing already on disk is lost, and a rerun converges.
#[test]
fn p3_p4_disconnect_at_every_frame_then_rerun_converges() {
    let base = scenario("p3");
    let reft = copy_scenario(&base, "p3ref");
    let r = run_session(&reft, None);
    r.client.unwrap();
    let want = head(&reft.join("a"));
    let (nc, ns) = (r.out.len(), r.back.len());
    assert!(nc > 10 && ns > 5, "{nc} {ns}");
    let count = |d: &Path| {
        Repo::open_dir_with(d.join(".hub"), LockMode::Shared)
            .unwrap()
            .store
            .len()
    };
    for (client_side, n) in [(true, nc), (false, ns)] {
        for k in 1..n {
            let t = copy_scenario(&base, &format!("p3-{client_side}-{k}"));
            let (a0, s0) = (count(&t.join("a")), count(&t.join("srv")));
            let r = run_session(&t, Some((client_side, k)));
            assert!(r.client.is_err(), "cut {client_side} {k} did not fail");
            // Valid, openable, nothing lost.
            ok(&t.join("a"), &["status"]);
            ok(&t.join("srv"), &["status"]);
            assert!(count(&t.join("a")) >= a0 && count(&t.join("srv")) >= s0);
            let r2 = run_session(&t, None);
            r2.client
                .unwrap_or_else(|e| panic!("rerun after cut {client_side} {k}: {e}"));
            r2.server.unwrap();
            assert_eq!(head(&t.join("a")), want, "cut {client_side} {k}");
            assert_eq!(head(&t.join("srv")), want);
            let _ = fs::remove_dir_all(&t);
        }
    }
}

#[test]
fn p5_blob_resume_transfers_only_the_rest() {
    let base = scenario("p5");
    let reft = copy_scenario(&base, "p5ref");
    let r = run_session(&reft, None);
    r.client.unwrap();
    // The client frame index in the middle of the big blob's chunks.
    let chunks: Vec<usize> = r
        .out
        .iter()
        .enumerate()
        .filter(|(_, f)| f[0] == wire::CHUNK)
        .map(|(i, _)| i + 1)
        .collect();
    assert!(chunks.len() >= 3, "{chunks:?}");
    let t = copy_scenario(&base, "p5");
    let r = run_session(&t, Some((true, chunks[1])));
    assert!(r.client.is_err());
    let parts: Vec<_> = fs::read_dir(t.join("srv/.hub/blobs"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("part-"))
        .collect();
    assert_eq!(parts.len(), 1, "no part kept");
    let kept = parts[0].metadata().unwrap().len();
    assert!(kept > 0);
    let r2 = run_session(&t, None);
    r2.client.unwrap();
    // Only the bytes after the kept prefix travel again.
    // (plus the few bytes of the small text blobs, 5 in total).
    assert_eq!(r2.server_blob_bytes, 700_000 - kept + 5);
    assert_eq!(head(&t.join("a")), head(&reft.join("a")));
}

// ------------------------------------------------------------ P11

#[test]
fn p11_garbage_frames_touch_nothing_outside_the_repo() {
    let d = server_repo("p11");
    let parent = d.parent().unwrap().to_path_buf();
    let outside = |p: &Path| -> BTreeMap<PathBuf, (Vec<u8>, std::time::SystemTime)> {
        files(p)
            .into_iter()
            .filter(|(k, _)| !k.starts_with(&d))
            .collect()
    };
    let before = outside(&parent);
    let log0 = fs::read(d.join(".hub/ops.log")).unwrap();
    let mut seed = 0x9e3779b97f4a7c15u64;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for i in 0..40 {
        let (ch, mut c) = serve(&d, false);
        hello(&mut c, ReplicaId(X)).unwrap();
        let n = (rnd() % 300) as usize;
        let mut body: Vec<u8> = (0..n).map(|_| rnd() as u8).collect();
        if body.is_empty() {
            body.push(0);
        }
        if i % 3 == 0 {
            body[0] = (rnd() % 0x12) as u8;
        }
        let _ = c.send_raw(&body);
        let _ = next(&mut c);
        done(ch);
    }
    assert_eq!(outside(&parent), before);
    assert_eq!(fs::read(d.join(".hub/ops.log")).unwrap(), log0);
    ok(&d, &["status"]);
}

/// §5.2.5 `max_blob_size`: a larger blob gets a non-fatal `Error limit`;
/// its chunks are discarded, its op stays pending, the session goes on.
#[test]
fn max_blob_size_is_non_fatal_for_that_blob() {
    let d = server_repo("maxblob");
    let mut c = Command::new(BIN);
    let mut ch = c
        .args(["serve", "--stdio"])
        .arg(&d)
        .env("HUB_TEST_MAX_BLOB_SIZE", "1000")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut conn = Conn::from_io(ch.stdout.take().unwrap(), ch.stdin.take().unwrap());
    let bytes = vec![3u8; 200_000];
    let id = BlobId::of(&bytes);
    hello(&mut conn, ReplicaId(X)).unwrap();
    pull(&mut conn).unwrap();
    let n = create(10, X, &[], "blobfile");
    let s = set(11, X, n.id, id);
    let small = create(12, X, &[], "small");
    push(&mut conn, vec![n, s, small], &mut |c, id, off| {
        stream(c, id, &bytes, off)
    })
    .unwrap();
    finish(&mut conn).unwrap();
    done(ch);
    assert!(!d.join(".hub/blobs").join(id.hex()).exists());
    let w = ok(&d, &["warnings"]);
    assert!(w.contains("pending op 11."), "{w}");
    assert!(ok(&d, &["ls"]).contains("small"));
}

// ------------------------------------------------------------ Issue #45

fn hello_features(me: ReplicaId, rejected_ops: bool) -> Hello {
    let mut h = old_hello(me, false);
    if !rejected_ops {
        h.features.retain(|f| f != wire::F_REJECTED_OPS);
    }
    h
}

/// Issue #45: the server lists the pushed ops it rejected (equivocation,
/// Lamport violation) in `Committed`, only to a client that negotiated
/// `rejected-ops`; an old client gets the plain frame.
#[test]
fn rejected_ops_are_reported_in_committed_only_when_negotiated() {
    let d = server_repo("rej");
    let (ch, mut c) = serve(&d, false);
    let h = hello_with(&mut c, hello_features(ReplicaId(X), true)).unwrap();
    assert!(h.has("rejected-ops"));
    pull(&mut c).unwrap();
    let (_, r) = push_full(&mut c, vec![create(5, X, &[], "a")], &mut no_blobs).unwrap();
    assert!(r.is_empty(), "{r:?}");
    finish(&mut c).unwrap();
    done(ch);
    let bad = vec![
        create(5, X, &[], "b"),                  // equivocation
        create(9, X, &[], "d"),                  // fine
        create(7, X, &[NodeId::new(9, X)], "v"), // dep 9 is not < 7
    ];
    for feature in [true, false] {
        let (ch, mut c) = serve(&d, false);
        hello_with(&mut c, hello_features(ReplicaId(X), feature)).unwrap();
        pull(&mut c).unwrap();
        let (_, r) = push_full(&mut c, bad.clone(), &mut no_blobs).unwrap();
        if feature {
            assert_eq!(
                r,
                vec![
                    (NodeId::new(5, X), "equivocation".to_string()),
                    (NodeId::new(7, X), "lamport-violation".to_string()),
                ]
            );
        } else {
            assert!(
                r.is_empty(),
                "an old client must get a plain Committed: {r:?}"
            );
        }
        finish(&mut c).unwrap();
        done(ch);
    }
    // The server's warnings are human text with structured fields.
    let w = ok(&d, &["warnings"]);
    assert!(
        w.contains(&format!(
            "equivocation: replica {} used op id 5.{:032x} for two different changes",
            &format!("{X:032x}")[..8],
            X
        )),
        "{w}"
    );
    assert!(
        w.contains("this repository kept: create file \"a\" in 0."),
        "{w}"
    );
    assert!(w.contains("; it dropped: create file \"b\" in 0."), "{w}");
    assert!(
        w.contains("What to do: two repositories share a replica id"),
        "{w}"
    );
    assert!(w.contains("lamport-violation: op 7."), "{w}");
    assert!(
        !w.contains("Create {") && !w.contains("NodeId"),
        "Debug text: {w}"
    );
    let j = ok(&d, &["warnings", "--json"]);
    assert!(j.contains("\"reason\":\"equivocation\""), "{j}");
    assert!(j.contains("\"kept\":\"create file \\\"a\\\" in 0."), "{j}");
    assert!(
        j.contains("\"received\":\"create file \\\"b\\\" in 0."),
        "{j}"
    );
    assert!(j.contains("\"action\":"), "{j}");
}

/// A scripted server over the in-memory transport: advertises
/// `rejected-ops` or not, and answers the push with `rej`.
fn scripted_server(mut y: Conn, feature: bool, rej: Rejected) -> Result<usize, Error> {
    y.send_magic()?;
    y.send(&Frame::Hello(hello_features(ReplicaId(Y), feature)))?;
    y.recv_magic(T)?;
    match next(&mut y)? {
        Frame::Hello(h) => y.theirs = h.limits,
        f => panic!("{f:?}"),
    }
    assert!(matches!(next(&mut y)?, Frame::Have { .. }));
    y.send(&Frame::Ops {
        frontier: vec![],
        done: true,
        ops: vec![],
    })?;
    loop {
        match next(&mut y)? {
            Frame::NeedBlobs(_) => {}
            Frame::Done(p) if p == "ops" => break,
            f => panic!("{f:?}"),
        }
    }
    assert!(matches!(next(&mut y)?, Frame::Committed(..)));
    y.send(&Frame::Have { frontier: vec![] })?;
    let mut got = 0;
    loop {
        match next(&mut y)? {
            Frame::Ops { done, ops, .. } => {
                got += ops.len();
                if done {
                    break;
                }
            }
            f => panic!("{f:?}"),
        }
    }
    y.send(&Frame::Done("ops".into()))?;
    y.send(&Frame::Committed(vec![], rej))?;
    loop {
        match next(&mut y)? {
            Frame::Checkpoints(_) => {}
            Frame::Done(p) if p == "meta" => break,
            f => panic!("{f:?}"),
        }
    }
    y.send(&Frame::Done("meta".into()))?;
    assert!(matches!(next(&mut y)?, Frame::Committed(..)));
    y.send(&Frame::Committed(vec![], vec![]))?;
    assert_eq!(next(&mut y)?, Frame::Bye);
    y.send(&Frame::Bye)?;
    Ok(got)
}

/// The client side: rejections reach the report (and so `sync`, `--json`
/// and `watch`); against an old server `rejected_reported` is false.
#[test]
fn client_reports_rejections_and_old_servers_are_marked_unreported() {
    let t = tmp("rejc");
    let d = t.join("c");
    fs::create_dir_all(&d).unwrap();
    ok(&d, &["init"]);
    fs::write(d.join("f.txt"), "x\n").unwrap();
    ok(&d, &["record"]);
    let mine = replica_of(&d);
    let id = NodeId {
        counter: 2,
        replica: mine,
    };
    for feature in [true, false] {
        let rej: Rejected = if feature {
            vec![(id, "equivocation".into())]
        } else {
            vec![]
        };
        let repo = Repo::open_dir_with(d.join(".hub"), LockMode::Exclusive).unwrap();
        let mut side = Side::new(Access::Held(Box::new(repo)), false).unwrap();
        let (mut x, y, _cut) = wire::pair();
        let r2 = rej.clone();
        let th = std::thread::spawn(move || scripted_server(y, feature, r2));
        let rep = session::client(&mut x, &mut side).unwrap();
        drop(x);
        assert!(th.join().unwrap().unwrap() > 0);
        assert_eq!(rep.rejected, rej);
        assert_eq!(rep.rejected_reported, feature);
    }
    let lines = hub_cli::sync::rejected_lines(&[(id, "equivocation".into())]);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].starts_with(&format!(
        "the remote rejected your op 2.{:032x} (equivocation): ",
        mine.0
    )));
    assert!(lines[0].contains("barocss-hub reid"), "{}", lines[0]);
    let j = hub_cli::json::rejected_j(&[(id, "lamport-violation".into())]).render();
    assert!(
        j.starts_with(&format!(
            "[{{\"op\":\"2.{:032x}\",\"reason\":\"lamport-violation\",\"message\":",
            mine.0
        )),
        "{j}"
    );
}

/// Decoding: extra `R` lines only in the documented form.
#[test]
fn committed_rejection_lines_are_strict() {
    let id = NodeId::new(3, X);
    let f = Frame::Committed(vec![id], vec![(id, "equivocation".into())]);
    let (mut a, mut b, _cut) = wire::pair();
    a.send(&f).unwrap();
    assert_eq!(b.recv(T).unwrap(), f);
}

/// Not a test on its own: with `HUB45_SCRIPTED_SERVER` set it is the
/// remote end of a fake ssh (stdin, and fd 3 for the protocol output, since
/// the test harness writes to stdout) and runs [`scripted_server`],
/// rejecting op `2.<HUB45_SCRIPTED_SERVER>` when the value is not `-`.
#[test]
fn zz_helper_scripted_server_over_stdio() {
    let Some(v) = std::env::var_os("HUB45_SCRIPTED_SERVER") else {
        return;
    };
    let v = v.into_string().unwrap();
    let out = fs::OpenOptions::new()
        .write(true)
        .open("/dev/fd/3")
        .unwrap();
    let y = Conn::from_io(std::io::stdin(), out);
    let (feature, rej) = if v == "-" {
        (false, vec![])
    } else {
        // `<replica hex>[/<counter>,...[/<reason>]]` (Issue #50).
        let mut p = v.split('/');
        let r = u128::from_str_radix(p.next().unwrap(), 16).unwrap();
        let cs: Vec<u64> = p
            .next()
            .map(|c| c.split(',').map(|n| n.parse().unwrap()).collect())
            .unwrap_or(vec![2]);
        let why = p.next().unwrap_or("equivocation").to_string();
        (
            true,
            cs.into_iter()
                .map(|c| (NodeId::new(c, r), why.clone()))
                .collect(),
        )
    };
    scripted_server(y, feature, rej).unwrap();
}

/// D-045: `sync` exits [`hub_cli::json::EXIT_REJECTED`] (3) when the
/// remote rejected some of its ops, with the normal report (human and
/// `--json`); a sync without rejections exits 0.
#[test]
fn sync_exits_3_when_the_remote_rejects_ops() {
    assert_eq!(hub_cli::json::EXIT_REJECTED, 3);
    let t = tmp("rej3");
    let d = t.join("c");
    fs::create_dir_all(&d).unwrap();
    ok(&d, &["init"]);
    fs::write(d.join("f.txt"), "x\n").unwrap();
    ok(&d, &["record"]);
    let mine = format!("{:x}", replica_of(&d).0);
    let ssh = t.join("fake-ssh");
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\nexec '{}' --exact zz_helper_scripted_server_over_stdio --nocapture --test-threads=1 3>&1 1>&2\n",
            std::env::current_exe().unwrap().display()
        ),
    )
    .unwrap();
    Command::new("chmod").arg("+x").arg(&ssh).status().unwrap();
    let run = |args: &[&str], rej: &str| {
        Command::new(BIN)
            .args(args)
            .current_dir(&d)
            .env("HUB_SSH_COMMAND", &ssh)
            .env("HUB45_SCRIPTED_SERVER", rej)
            .env("BAROCSS_HUB_CONFIG_DIR", t.join("cfg"))
            .env_remove("BAROCSS_HUB_SERVE_ROOT")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    let o = run(&["remote", "add", "origin", "ssh://localhost/srv"], "-");
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let o = run(&["sync"], &mine);
    let err = String::from_utf8_lossy(&o.stderr);
    assert_eq!(o.status.code(), Some(3), "{err}");
    assert!(
        err.contains("warning: the remote rejected your op 2."),
        "{err}"
    );
    assert!(String::from_utf8_lossy(&o.stdout).contains("head "));
    let o = run(&["sync", "--json"], &mine);
    assert!(
        o.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    // Issue #50: the repeat is already reported: exit 0, still listed.
    assert_eq!(o.status.code(), Some(0));
    let j = String::from_utf8(o.stdout).unwrap();
    assert!(j.contains("\"rejected\":[{\"op\":\"2."), "{j}");
    assert!(j.contains("\"already_reported\":true"), "{j}");
    assert!(j.contains("\"rejected_reported\":true"), "{j}");
    // An old server (no rejected-ops feature): nothing reported, exit 0.
    let o = run(&["sync", "--json"], "-");
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(String::from_utf8_lossy(&o.stdout).contains("\"rejected_reported\":false"));
}

/// Issue #50 (D-050): a rejection is reported once per remote. Repeats
/// exit 0 without a warning; a new op id, a different payload under the
/// same id, or a cleared memory is reported again (exit 3);
/// `duplicate-replica` is never suppressed; a crash while writing the
/// memory never suppresses a report that was not printed. With
/// `HUB_TEST_NO_REJECTION_DEDUP` (the mutation check) the repeat exits 3.
#[test]
fn rejections_are_reported_once_per_remote() {
    let t = tmp("rej50");
    let d = t.join("c");
    fs::create_dir_all(&d).unwrap();
    ok(&d, &["init"]);
    fs::write(d.join("f.txt"), "x\n").unwrap();
    ok(&d, &["record"]);
    fs::write(d.join("g.txt"), "y\n").unwrap();
    ok(&d, &["record"]);
    let mine = format!("{:x}", replica_of(&d).0);
    let ssh = t.join("fake-ssh");
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\nexec '{}' --exact zz_helper_scripted_server_over_stdio --nocapture --test-threads=1 3>&1 1>&2\n",
            std::env::current_exe().unwrap().display()
        ),
    )
    .unwrap();
    Command::new("chmod").arg("+x").arg(&ssh).status().unwrap();
    let run_env = |args: &[&str], rej: &str, env: &[(&str, &str)]| {
        let mut c = Command::new(BIN);
        c.args(args)
            .current_dir(&d)
            .env("HUB_SSH_COMMAND", &ssh)
            .env("HUB45_SCRIPTED_SERVER", rej)
            .env("BAROCSS_HUB_CONFIG_DIR", t.join("cfg"))
            .env_remove("BAROCSS_HUB_SERVE_ROOT")
            .env_remove("HUB_TEST_NO_REJECTION_DEDUP")
            .env_remove("HUB_TEST_CRASH_STAGE")
            .stdin(Stdio::null());
        for (k, v) in env {
            c.env(k, v);
        }
        c.output().unwrap()
    };
    let run = |args: &[&str], rej: &str| run_env(args, rej, &[]);
    let code = |o: &std::process::Output| o.status.code();
    let err = |o: &std::process::Output| String::from_utf8_lossy(&o.stderr).into_owned();
    let o = run(&["remote", "add", "origin", "ssh://localhost/srv"], "-");
    assert!(o.status.success(), "{}", err(&o));
    let r2 = format!("{mine}/2");
    // First report: exit 3 with the warning.
    let o = run(&["sync"], &r2);
    assert_eq!(code(&o), Some(3), "{}", err(&o));
    assert!(err(&o).contains("rejected your op 2."));
    let mem = d.join(".hub/reported-rejections");
    assert!(mem.exists());
    // Repeats: exit 0, no warning, twice.
    for _ in 0..2 {
        let o = run(&["sync"], &r2);
        assert_eq!(code(&o), Some(0), "{}", err(&o));
        assert!(!err(&o).contains("rejected"), "{}", err(&o));
    }
    // Mutation check: without the dedup the repeat exits 3 again. The hook
    // is debug-only (#49/#55): a release binary ignores it and stays quiet.
    let o = run_env(&["sync"], &r2, &[("HUB_TEST_NO_REJECTION_DEDUP", "1")]);
    let want = if cfg!(debug_assertions) { 3 } else { 0 };
    assert_eq!(code(&o), Some(want), "{}", err(&o));
    // `warnings` lists it, marked, in human and --json form.
    let w = ok(&d, &["warnings"]);
    assert!(w.contains("reported rejection (suppressed): op 2."), "{w}");
    let wj = ok(&d, &["warnings", "--json"]);
    assert!(wj.contains("\"reported_rejections\":[{\"op\":\"2."), "{wj}");
    // A new rejection next to the known one: exit 3, only the new one warned.
    let o = run(&["sync"], &format!("{mine}/2,3"));
    assert_eq!(code(&o), Some(3));
    assert!(err(&o).contains("rejected your op 3."), "{}", err(&o));
    assert!(!err(&o).contains("rejected your op 2."), "{}", err(&o));
    let o = run(&["sync"], &format!("{mine}/2,3"));
    assert_eq!(code(&o), Some(0), "{}", err(&o));
    // A different payload under the same op id (the memory holds another
    // hash): reported.
    let text = fs::read_to_string(&mem).unwrap();
    let body = text.lines().nth(1).unwrap().to_string();
    let hash = body.split(' ').find(|w| w.len() == 64).unwrap().to_string();
    let other = "0".repeat(64);
    let lines: Vec<String> = text
        .lines()
        .skip(1)
        .map(|l| {
            let b = l.split('\t').next().unwrap();
            hub_cli::store::frame(&b.replace(&hash, &other))
        })
        .collect();
    fs::write(
        &mem,
        format!("{}\n{}", hub_cli::reported::HEADER, lines.concat()),
    )
    .unwrap();
    let o = run(&["sync"], &r2);
    assert_eq!(code(&o), Some(3), "{}", err(&o));
    assert!(err(&o).contains("rejected your op 2."));
    // Clear (one remote, then all): reported again.
    assert!(ok(&d, &["warnings", "--clear-reported", "origin"]).contains("forgot"));
    let o = run(&["sync"], &r2);
    assert_eq!(code(&o), Some(3));
    assert!(ok(&d, &["warnings", "--clear-reported"]).contains("forgot"));
    assert!(!ok(&d, &["warnings"]).contains("reported rejection"));
    let o = run(&["sync"], &r2);
    assert_eq!(code(&o), Some(3));
    // duplicate-replica is never suppressed.
    let dup = format!("{mine}/2/duplicate-replica");
    for _ in 0..2 {
        let o = run(&["sync"], &dup);
        assert_eq!(code(&o), Some(3));
        assert!(err(&o).contains("(duplicate-replica)"), "{}", err(&o));
    }
    // Crash safety: kill -9 at each write step of the memory.
    for stage in ["reported-tmp", "reported-renamed"] {
        ok(&d, &["warnings", "--clear-reported"]);
        let r4 = format!("{mine}/2,3");
        let o = run_env(&["sync"], &r4, &[("HUB_TEST_CRASH_STAGE", stage)]);
        assert!(!o.status.success(), "{stage}: crash expected");
        // The report was printed before the crash.
        assert!(
            err(&o).contains("rejected your op 3."),
            "{stage}: {}",
            err(&o)
        );
        // The memory is whole: old or new, never torn; `warnings` works.
        ok(&d, &["warnings"]);
        let o = run(&["sync"], &r4);
        let c = code(&o);
        match stage {
            // Not renamed: nothing remembered, so reported again.
            "reported-tmp" => assert_eq!(c, Some(3), "{}", err(&o)),
            // Renamed: remembered (the report was shown).
            _ => assert_eq!(c, Some(0), "{}", err(&o)),
        }
    }
}

/// Issue #50: `watch --json` emits a `rejected` event once per rejection,
/// not on every sync cycle.
#[test]
fn watch_emits_each_rejection_once() {
    let t = tmp("rej50w");
    let d = t.join("c");
    fs::create_dir_all(&d).unwrap();
    ok(&d, &["init"]);
    fs::write(d.join("f.txt"), "x\n").unwrap();
    ok(&d, &["record"]);
    let mine = format!("{:x}", replica_of(&d).0);
    let ssh = t.join("fake-ssh");
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\nexec '{}' --exact zz_helper_scripted_server_over_stdio --nocapture --test-threads=1 3>&1 1>&2\n",
            std::env::current_exe().unwrap().display()
        ),
    )
    .unwrap();
    Command::new("chmod").arg("+x").arg(&ssh).status().unwrap();
    let cmd = |args: &[&str]| {
        let mut c = Command::new(BIN);
        c.args(args)
            .current_dir(&d)
            .env("HUB_SSH_COMMAND", &ssh)
            .env("HUB45_SCRIPTED_SERVER", format!("{mine}/2"))
            .env("BAROCSS_HUB_CONFIG_DIR", t.join("cfg"))
            .env_remove("BAROCSS_HUB_SERVE_ROOT")
            .env_remove("HUB_TEST_NO_REJECTION_DEDUP")
            .stdin(Stdio::null());
        c
    };
    let o = cmd(&["remote", "add", "origin", "ssh://localhost/srv"])
        .output()
        .unwrap();
    assert!(o.status.success());
    // Run a watcher for >= 3 s; `nodedup` = the mutation check.
    let watch = |nodedup: bool| {
        let log = t.join(format!("watch-{nodedup}.out"));
        let mut c = cmd(&["watch", "--json", "--sync", "100", "--poll", "50"]);
        if nodedup {
            c.env("HUB_TEST_NO_REJECTION_DEDUP", "1");
        }
        let mut child = c
            .stdout(fs::File::create(&log).unwrap())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let t0 = std::time::Instant::now();
        while !(d.join(".hub/reported-rejections").exists()
            && t0.elapsed() > Duration::from_secs(3))
        {
            assert!(t0.elapsed() < Duration::from_secs(60));
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = child.kill();
        let _ = child.wait();
        let s = fs::read_to_string(&log).unwrap();
        s.matches("\"event\":\"rejected\"").count()
    };
    assert_eq!(watch(false), 1);
    // Already remembered: a new watcher stays quiet...
    assert_eq!(watch(false), 0);
    // ...and without the dedup every cycle repeats it. The hook is
    // debug-only (#49/#55): a release watcher ignores it and stays quiet.
    let n = watch(true);
    if cfg!(debug_assertions) {
        assert!(n >= 2, "{n}");
    } else {
        assert_eq!(n, 0);
    }
}
