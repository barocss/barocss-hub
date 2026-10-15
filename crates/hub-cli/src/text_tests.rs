//! Issue #54: every encode path carries a `TextEdit` exactly (ops.log,
//! pending.log, the reported-rejections hash; the wire `Ops` body is in
//! `wire::tests`, the rescue manifest in `rescue::text_manifest_tests`),
//! and the lazy format v2 header.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use hub_core::text::{ElemId, Ins, TextEdit};
use hub_core::tree::{BlobId, Kind, NodeId, Op, Payload, ROOT};

use crate::store::LockMode;
use crate::{snapshot, store, sync, Repo};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hub54-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// A file `f.txt` (created by `create`) and one TextEdit inserting
/// `hello\n` on its empty epoch.
fn text_ops(repo: &Repo) -> (Op, Op) {
    let create = repo.prepare(Payload::Create {
        kind: Kind::File,
        parent: ROOT,
        name: "f.txt".into(),
    });
    let te = Op {
        id: NodeId::new(create.id.counter + 1, repo.replica_id.0),
        deps: [create.id].into(),
        payload: Payload::TextEdit(TextEdit {
            node: create.id,
            epoch: create.id,
            seen: BTreeSet::new(),
            del: vec![],
            ins: vec![Ins {
                left: None,
                right: None,
                bytes: b"hello\n".to_vec(),
            }],
        }),
    };
    (create, te)
}

#[test]
fn ops_log_round_trips_a_text_edit_and_upgrades_lazily() {
    let root = tmp("opslog");
    let mut repo = Repo::init(&root).unwrap();
    let dir = repo.dir.clone();
    let (create, te) = text_ops(&repo);
    repo.append_op(create.clone()).unwrap();
    // No text op yet: still v1.
    assert!(!repo.is_v2());
    assert!(fs::read_to_string(dir.join("replica"))
        .unwrap()
        .starts_with("barocss-hub/replica/v1\n"));
    repo.append_op(te.clone()).unwrap();
    assert!(repo.is_v2());
    assert!(fs::read_to_string(dir.join("replica"))
        .unwrap()
        .starts_with("barocss-hub/replica/v2\n"));
    assert_eq!(
        fs::read_to_string(dir.join("format")).unwrap(),
        store::FORMAT_V2
    );
    let want = BlobId::of(b"hello\n");
    assert_eq!(repo.state()[&create.id].content, Some(want));
    drop(repo);
    for snap in [false, true] {
        snapshot::TEST_OFF.with(|c| c.set(!snap));
        snapshot::TEST_MIN.with(|c| c.set(Some(0)));
        let mut r = Repo::open_dir_with(dir.clone(), LockMode::Exclusive).unwrap();
        assert_eq!(r.store[&te.id], te);
        assert_eq!(r.state()[&create.id].content, Some(want));
        assert!(r.replica.text_valid().contains(&te.id));
        assert!(r.is_v2());
        // A snapshot written now is v2; reopening from it gives the same.
        let _ = r.maybe_snapshot();
        drop(r);
        let r = Repo::open_dir_with(dir.clone(), LockMode::Shared).unwrap();
        assert_eq!(r.state()[&create.id].content, Some(want));
        assert_eq!(
            r.conflicts(),
            hub_core::tree::evaluate(r.store.values()).conflicts
        );
    }
    snapshot::TEST_OFF.with(|c| c.set(false));
    snapshot::TEST_MIN.with(|c| c.set(None));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn pending_log_round_trips_a_text_edit() {
    let root = tmp("pending");
    let repo = Repo::init(&root).unwrap();
    let te = crate::wire::tests::golden_text_op();
    let peer = hub_core::tree::ReplicaId(0x5);
    let body = sync::encode_pending(&te, peer);
    fs::write(
        repo.dir.join("pending.log"),
        format!("{}\n{}", sync::PENDING_HEADER, store::frame(&body)),
    )
    .unwrap();
    assert_eq!(sync::read_pending(&repo.dir).unwrap(), vec![(te, peer)]);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reported_hash_is_the_text_record_hash() {
    use sha2::{Digest, Sha256};
    let te = crate::wire::tests::golden_text_op();
    let want: String = Sha256::digest(store::encode_op(&te).as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(crate::reported::payload_hash(&te), want);
    // Another payload under the same id hashes differently (a new
    // equivocation is reported again, #50).
    let mut other = te.clone();
    if let Payload::TextEdit(t) = &mut other.payload {
        t.del.push((
            ElemId {
                op: t.epoch,
                off: 9,
            },
            1,
        ));
    }
    assert_ne!(crate::reported::payload_hash(&other), want);
}

/// Issue #54 guard: an authored record over `TEXT_OP_MAX` is refused with
/// `limit`, nothing written; the same edit split into a chain appends in
/// one batch and reopens to the same text.
#[test]
fn oversized_record_is_refused_and_a_chain_fits() {
    let root = tmp("limit");
    let mut repo = Repo::init(&root).unwrap();
    let (create, mut te) = text_ops(&repo);
    repo.append_op(create.clone()).unwrap();
    let big: Vec<u8> = "한 line\n".repeat(store::TEXT_OP_MAX / 4).into_bytes();
    if let Payload::TextEdit(t) = &mut te.payload {
        t.ins[0].bytes = big.clone();
    }
    let log0 = fs::read(repo.dir.join("ops.log")).unwrap();
    let e = repo.append_op(te.clone()).unwrap_err();
    assert_eq!(e.code(), crate::Code::Limit);
    assert_eq!(fs::read(repo.dir.join("ops.log")).unwrap(), log0);
    assert!(!repo.store.contains_key(&te.id));
    let Payload::TextEdit(t) = te.payload.clone() else {
        unreachable!()
    };
    let size = |o: &Op| store::encode_op(o).len();
    let chain =
        hub_core::text::split_text_edit(te.id, te.deps.clone(), t, &size, store::TEXT_OP_MAX)
            .unwrap();
    assert!(chain.len() > 1);
    repo.append_ops(chain.clone()).unwrap();
    let dir = repo.dir.clone();
    drop(repo);
    let r = Repo::open_dir_with(dir, LockMode::Shared).unwrap();
    assert_eq!(r.state()[&create.id].content, Some(BlobId::of(&big)));
    let _ = fs::remove_dir_all(&root);
}
