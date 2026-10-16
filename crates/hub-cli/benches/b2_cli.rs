//! Issue #56 (carried from #54 Review): a full CLI open at B2 scale —
//! 1M `TextEdit`s over 10k files of 2–200 KiB (the core B2 history,
//! `hub-core/benches/text.rs`), in a real repository with a working
//! directory, a v2 snapshot and every seed blob on disk. Times the release
//! `barocss-hub` binary end to end (process start, lock, snapshot read and
//! checksum pass, decode, seed `stat`s, the op-log tail, index, scan):
//! `status`, `record` with nothing to record, and `record` of one saved
//! text file (one `TextEdit`). Target (D-026): each ≤ 1 s. Also reports
//! the in-process `Repo` open for the breakdown.
//!
//! Run: `caffeinate -i cargo bench -p barocss-hub-cli --bench b2_cli [-- files edits]`.
//! Needs ~3 GiB of free disk under the temp dir (`B2_DIR` overrides); the
//! directory is removed at the end.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use hub_cli::store::LockMode;
use hub_cli::Repo;
use hub_core::text::{ElemId, Ins, TextEdit};
use hub_core::tree::{BlobId, Kind, NodeId, Op, OpId, Payload, ROOT};

struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

const WORDS: [&str; 8] = [
    "the ", "crdt ", "merges ", "lines ", "of ", "text ", "fugue ", "max ",
];

type B2File = (NodeId, OpId, OpId, u32, Option<(OpId, u32)>);

/// The core B2 history (same generator and seed as `case_b2`), with this
/// repository's replica as the creator of the files.
fn history(nfiles: usize, nedits: usize, me: u128) -> (Vec<Op>, HashMap<BlobId, Vec<u8>>) {
    let mut rng = Rng::new(54);
    let mut ops: Vec<Op> = Vec::with_capacity(2 * nfiles + nedits);
    let mut blobs = HashMap::new();
    let mut counter = 10u64;
    let mut next = |r: u128| {
        counter += 1;
        NodeId::new(counter, r)
    };
    let mut files: Vec<B2File> = Vec::new();
    let mut setup: Option<OpId> = None;
    for i in 0..nfiles {
        let n = next(me);
        ops.push(Op {
            id: n,
            deps: setup.into_iter().collect(),
            payload: Payload::Create {
                kind: Kind::File,
                parent: ROOT,
                name: format!("f{i}.txt"),
            },
        });
        let size = 2048 + rng.below(198 * 1024) as usize;
        let mut seed = Vec::with_capacity(size + 64);
        while seed.len() < size {
            for _ in 0..8 {
                seed.extend_from_slice(WORDS[rng.below(8) as usize].as_bytes());
            }
            seed.push(b'\n');
        }
        let b = BlobId::of(&seed);
        let s = next(me);
        ops.push(Op {
            id: s,
            deps: [n].into(),
            payload: Payload::SetContent { node: n, blob: b },
        });
        setup = Some(s);
        files.push((n, s, s, seed.len() as u32, None));
        blobs.insert(b, seed);
    }
    let mut mine: Vec<Option<OpId>> = vec![None; 20];
    for k in 0..nedits {
        let f = rng.below(nfiles as u64) as usize;
        let (n, last, epoch, slen, prev) = files[f];
        let r = 2 + (k % 20) as u128;
        let id = next(r);
        let mut deps: BTreeSet<OpId> = [last].into();
        deps.extend(mine[k % 20]);
        mine[k % 20] = Some(id);
        // Seed lines are 8 words + `\n`; the generator inserts a whole
        // line after a random seed byte, as the core bench does.
        let line = format!("edit {k} by {r}\n").into_bytes();
        let off = rng.below(slen as u64) as u32;
        let mut del = Vec::new();
        if k % 5 == 4 {
            if let Some((p, l)) = prev {
                del.push((ElemId { op: p, off: 0 }, l));
            }
        }
        let len = line.len() as u32;
        ops.push(Op {
            id,
            deps,
            payload: Payload::TextEdit(TextEdit {
                node: n,
                epoch,
                seen: [last].into(),
                del,
                ins: vec![Ins {
                    left: Some(ElemId { op: epoch, off }),
                    right: None,
                    bytes: line,
                }],
            }),
        });
        files[f] = (n, id, epoch, slen, Some((id, len)));
    }
    (ops, blobs)
}

fn time_cli(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> (f64, String) {
    let t = Instant::now();
    let o = Command::new(env!("CARGO_BIN_EXE_barocss-hub"))
        .args(args)
        .current_dir(dir)
        .envs(env.iter().copied())
        .output()
        .unwrap();
    let s = t.elapsed().as_secs_f64();
    assert!(
        o.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    (s, String::from_utf8_lossy(&o.stdout).into_owned())
}

fn best(n: usize, mut f: impl FnMut() -> f64) -> f64 {
    (0..n).map(|_| f()).fold(f64::MAX, f64::min)
}

fn main() {
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with("--"))
        .collect();
    let nfiles = args.first().and_then(|v| v.parse().ok()).unwrap_or(10_000);
    let nedits = args
        .get(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000_000);
    let base = std::env::var_os("B2_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let root = base.join(format!("hub-b2cli-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let root = std::fs::canonicalize(&root).unwrap();

    let t = Instant::now();
    let mut repo = Repo::init(&root).unwrap();
    let me = repo.replica_id.0;
    let (ops, blobs) = history(nfiles, nedits, me);
    let gen = t.elapsed().as_secs_f64();
    let t = Instant::now();
    {
        let b = repo.blobs().unwrap();
        let mut batch = b.batch_direct();
        for bytes in blobs.values() {
            batch.add(bytes).unwrap();
        }
        batch.commit().unwrap();
    }
    for chunk in ops.chunks(100_000) {
        repo.append_ops(chunk.iter().cloned()).unwrap();
    }
    let build = t.elapsed().as_secs_f64();
    drop(repo);
    drop(blobs);
    println!(
        "B2 CLI: {nfiles} files, {nedits} TextEdits, {} ops (generate {gen:.1} s, append {build:.1} s)",
        ops.len()
    );
    drop(ops);
    // First checkout: materializes 10k merged heads (derived blobs
    // written) and writes the snapshot (a writer command).
    let (co, _) = time_cli(&root, &["checkout"], &[]);
    println!("  first checkout (writes {nfiles} files + derived blobs): {co:.2} s");
    let snapshot = root.join(".hub/snapshot");
    let ssize = std::fs::metadata(&snapshot).map(|m| m.len()).unwrap_or(0);
    println!("  snapshot: {:.1} MiB", ssize as f64 / (1 << 20) as f64);

    let mut ok = ssize > 0;
    let t_open = best(3, || {
        let t = Instant::now();
        let r = Repo::open_dir_with(root.join(".hub"), LockMode::Shared).unwrap();
        let s = t.elapsed().as_secs_f64();
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
        s
    });
    println!(
        "  in-process open (snapshot read + checksum + decode + seed stats + tail): {t_open:.2} s"
    );
    let (_, st) = time_cli(&root, &["status"], &[]);
    assert!(st.contains("working directory: clean"), "{st}");
    let t_status = best(3, || time_cli(&root, &["status"], &[]).0);
    let t_rec0 = best(3, || time_cli(&root, &["record"], &[]).0);
    // One save of one text file: one TextEdit (diff, append, plan).
    let mut t_rec1 = f64::MAX;
    for k in 0..3 {
        let p = root.join(format!("f{k}.txt"));
        let mut b = std::fs::read(&p).unwrap();
        b.extend_from_slice(format!("saved line {k}\n").as_bytes());
        std::fs::write(&p, b).unwrap();
        let (s, out) = time_cli(&root, &["record"], &[]);
        assert!(out.contains("modified: f"), "{out}");
        t_rec1 = t_rec1.min(s);
    }
    for (name, s) in [
        ("status", t_status),
        ("record (nothing to record)", t_rec0),
        ("record (one text save)", t_rec1),
    ] {
        let hit = s <= 1.0;
        ok &= hit;
        println!(
            "  B2 CLI {name}: {s:.2} s (target 1 s){}",
            if hit { "" } else { " MISSED" }
        );
    }
    if std::env::var_os("B2_KEEP").is_none() {
        let _ = std::fs::remove_dir_all(&root);
    } else {
        println!("  kept {}", root.display());
    }
    if !ok {
        eprintln!("b2_cli: a target was missed (see above)");
        std::process::exit(1);
    }
}
