//! Issue #53: Text-CRDT §7.11 benches B1 and B3 on the `hub-core` text
//! CRDT, in process. A core bench rather than a `hub-bench` case because
//! `hub-bench` never links product code (it drives the `hub` binary).
//! Run: `caffeinate -i cargo bench -p barocss-hub-core --bench text`
//! (release; `-- b1 [N]` or `-- b3` for one case).
//!
//! * B1: a deterministic synthetic equivalent of the automerge-perf editing
//!   trace (260k keystrokes typing a ~100 KB document with cursor
//!   locality), converted to line level (one `TextEdit` per keystroke that
//!   replaces the touched line, inserts a new line or deletes one), and the
//!   char-level form for reference. Replay = integrate every op into a
//!   fresh doc. Target (line level): ≤ 0.5 s, ≤ 100 MB.
//! * B3: 20 editors × 500 line edits on one 10k-line file in 20 sync
//!   rounds; three random causal delivery orders into fresh docs.
//!   Target: convergence, p99 integrate ≤ 1 ms per op.
//! * B2 + B5 (Issue #54): 1M `TextEdit`s over 10k files of 2–200 KiB
//!   (uniform), through the incremental `Replica`: full replay (open
//!   without a snapshot), snapshot v2 encode, size (B5), file read +
//!   decode (open with a snapshot, B2 target ≤ 1 s) and one more edit
//!   on the reopened replica (the record step's core cost, ≤ 1 s).
//!   `-- b2 [files] [edits]`.

use std::collections::BTreeSet;
use std::time::Instant;

use hub_core::text::{diff_line, ElemId, Ins, TextDoc, TextEdit};
use hub_core::tree::{evaluate_with, BlobId, Kind, NodeId, Op, OpId, Payload, Replica, ROOT};

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Counting allocator: current and peak bytes allocated by the process.
struct Counting;
static CUR: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            let c = CUR.fetch_add(l.size(), Ordering::Relaxed) + l.size();
            PEAK.fetch_max(c, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        CUR.fetch_sub(l.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static A: Counting = Counting;

/// Peak is measured relative to the allocation level at reset.
static BASE: AtomicUsize = AtomicUsize::new(0);
fn alloc_peak_reset() {
    let c = CUR.load(Ordering::Relaxed);
    BASE.store(c, Ordering::Relaxed);
    PEAK.store(c, Ordering::Relaxed);
}
fn alloc_peak() -> usize {
    PEAK.load(Ordering::Relaxed) - BASE.load(Ordering::Relaxed)
}

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

fn edit(node: NodeId, epoch: OpId, del: Vec<(ElemId, u32)>, ins: Vec<Ins>) -> TextEdit {
    TextEdit {
        node,
        epoch,
        seen: [epoch].into_iter().collect(),
        del,
        ins,
    }
}

/// Synthetic trace, line level: (ops, final text).
fn b1_line_trace(n_ops: usize, seed: u64) -> (Vec<(OpId, TextEdit)>, Vec<u8>) {
    let node = NodeId::new(1, 1);
    let epoch = node; // the Create's empty epoch
    let mut rng = Rng::new(seed);
    let mut doc = TextDoc::new(epoch, b"");
    // Model: current lines as (first element, bytes) — each line is one
    // visible run of one op.
    let mut lines: Vec<(ElemId, Vec<u8>)> = Vec::new();
    let mut cur = 0usize;
    let mut ops = Vec::with_capacity(n_ops);
    let all = |_: OpId| true;
    for k in 0..n_ops {
        let id = NodeId::new(2 + k as u64, 1);
        let r = rng.below(100);
        if lines.is_empty() || r < 6 {
            // New line after the cursor line (Enter).
            let left = if lines.is_empty() {
                None
            } else {
                let (e, b) = &lines[cur];
                Some(ElemId {
                    op: e.op,
                    off: e.off + b.len() as u32 - 1,
                })
            };
            let bytes = b"\n".to_vec();
            let right = doc.successor_at(left, &all);
            let t = edit(
                node,
                epoch,
                vec![],
                vec![Ins {
                    left,
                    right,
                    bytes: bytes.clone(),
                }],
            );
            doc.apply(id, &t);
            let at = if lines.is_empty() { 0 } else { cur + 1 };
            lines.insert(at, (ElemId { op: id, off: 0 }, bytes));
            cur = at;
            ops.push((id, t));
            continue;
        }
        if r < 8 && lines.len() > 1 {
            // Delete the cursor line.
            let (e, b) = lines.remove(cur);
            let t = edit(node, epoch, vec![(e, b.len() as u32)], vec![]);
            doc.apply(id, &t);
            cur = cur.min(lines.len() - 1);
            ops.push((id, t));
            continue;
        }
        if r < 20 {
            // Cursor jump with locality.
            let d = rng.below(7) as isize - 3;
            cur = (cur as isize + d).clamp(0, lines.len() as isize - 1) as usize;
        }
        // Keystroke: replace the cursor line by its typed / backspaced form.
        let (e, b) = lines[cur].clone();
        let mut nb = b.clone();
        let body = nb.len() - 1; // keep the final '\n'
        if rng.below(10) < 9 || body == 0 {
            let w = WORDS[rng.below(WORDS.len() as u64) as usize].as_bytes();
            let c = w[rng.below(w.len() as u64) as usize];
            let p = if body == 0 {
                0
            } else {
                body - rng.below(3.min(body as u64 + 1)) as usize
            };
            nb.insert(p.min(body), c);
        } else {
            nb.remove(body - 1);
        }
        let left = Some(ElemId {
            op: e.op,
            off: e.off + b.len() as u32 - 1,
        });
        let right = doc.successor_at(left, &all);
        let t = edit(
            node,
            epoch,
            vec![(e, b.len() as u32)],
            vec![Ins {
                left,
                right,
                bytes: nb.clone(),
            }],
        );
        doc.apply(id, &t);
        lines[cur] = (ElemId { op: id, off: 0 }, nb);
        ops.push((id, t));
    }
    let text: Vec<u8> = lines.iter().flat_map(|(_, b)| b.iter().copied()).collect();
    assert_eq!(doc.materialize_head(), text, "generator model");
    (ops, text)
}

/// Same keystroke model at character level: one op per typed or deleted
/// character.
fn b1_char_trace(n_ops: usize, seed: u64) -> (Vec<(OpId, TextEdit)>, Vec<u8>) {
    let node = NodeId::new(1, 1);
    let epoch = node;
    let mut rng = Rng::new(seed);
    let mut doc = TextDoc::new(epoch, b"");
    let mut chars: Vec<(ElemId, u8)> = Vec::new();
    let mut cur = 0usize;
    let mut ops = Vec::with_capacity(n_ops);
    let all = |_: OpId| true;
    for k in 0..n_ops {
        let id = NodeId::new(2 + k as u64, 1);
        if rng.below(100) < 15 {
            let d = rng.below(41) as isize - 20;
            cur = (cur as isize + d).clamp(0, chars.len() as isize) as usize;
        }
        if rng.below(10) == 0 && cur > 0 {
            let (e, _) = chars.remove(cur - 1);
            cur -= 1;
            let t = edit(node, epoch, vec![(e, 1)], vec![]);
            doc.apply(id, &t);
            ops.push((id, t));
            continue;
        }
        let c = if rng.below(40) == 0 {
            b'\n'
        } else {
            let w = WORDS[rng.below(WORDS.len() as u64) as usize].as_bytes();
            w[rng.below(w.len() as u64) as usize]
        };
        let left = cur.checked_sub(1).map(|i| chars[i].0);
        let right = doc.successor_at(left, &all);
        let t = edit(
            node,
            epoch,
            vec![],
            vec![Ins {
                left,
                right,
                bytes: vec![c],
            }],
        );
        doc.apply(id, &t);
        chars.insert(cur, (ElemId { op: id, off: 0 }, c));
        cur += 1;
        ops.push((id, t));
    }
    let text: Vec<u8> = chars.iter().map(|x| x.1).collect();
    assert_eq!(doc.materialize_head(), text, "generator model");
    (ops, text)
}

fn replay(ops: &[(OpId, TextEdit)]) -> (f64, f64, Vec<u8>, usize) {
    alloc_peak_reset();
    let t0 = Instant::now();
    let mut doc = TextDoc::new(NodeId::new(1, 1), b"");
    for (id, t) in ops {
        doc.apply(*id, t);
    }
    let secs = t0.elapsed().as_secs_f64();
    let mb = alloc_peak() as f64 / 1e6;
    let text = doc.materialize_head();
    (secs, mb, text, doc.runs())
}

fn case_b1(n: usize) -> bool {
    let mut ok = true;
    for (name, (ops, want)) in [
        ("line-level", b1_line_trace(n, 0xB1)),
        ("char-level", b1_char_trace(n, 0xB1)),
    ] {
        let (secs, mb, text, runs) = replay(&ops);
        let same = text == want;
        let pass = same && (name != "line-level" || (secs <= 0.5 && mb <= 100.0));
        ok &= same && (name != "line-level" || pass);
        println!(
            "B1 {name}: {n} ops, final {} bytes, {runs} runs; replay {:.3} s, peak alloc {:.1} MB; {}{}",
            text.len(),
            secs,
            mb,
            if same { "text matches" } else { "TEXT MISMATCH" },
            if name == "line-level" {
                if pass {
                    " (target <= 0.5 s, <= 100 MB: met)"
                } else {
                    " (target <= 0.5 s, <= 100 MB: MISSED)"
                }
            } else {
                " (reference, no target)"
            }
        );
    }
    ok
}

/// B3 op: author, id, deps, edit.
struct Authored {
    id: OpId,
    deps: BTreeSet<OpId>,
    edit: TextEdit,
}

fn case_b3(editors: usize, edits: usize, lines: usize, rounds: usize) -> bool {
    let node = NodeId::new(1, 1);
    let epoch = NodeId::new(2, 1);
    let seed: Vec<u8> = (0..lines)
        .flat_map(|i| format!("line {i} of the shared file\n").into_bytes())
        .collect();
    let per = edits / rounds;
    let mut docs: Vec<TextDoc> = (0..editors).map(|_| TextDoc::new(epoch, &seed)).collect();
    let mut rng = Rng::new(0xB3);
    let mut all: Vec<Authored> = Vec::new();
    let mut prev_round_last: BTreeSet<OpId> = [epoch].into_iter().collect();
    let everything = |_: OpId| true;
    let t_gen = Instant::now();
    let mut by_round: Vec<Vec<Vec<usize>>> = Vec::new();
    for r in 0..rounds {
        let mut round: Vec<Vec<usize>> = vec![Vec::new(); editors];
        let mut last_of_round = BTreeSet::new();
        for (ed, doc) in docs.iter_mut().enumerate() {
            let mut prev: Option<OpId> = None;
            for j in 0..per {
                let id = NodeId::new(3 + (r * per + j) as u64, 10 + ed as u128);
                let base = doc.visible_runs(&everything);
                let text: Vec<u8> = base.iter().flat_map(|(_, b)| b.iter().copied()).collect();
                let mut ls: Vec<&[u8]> = text.split_inclusive(|&b| b == b'\n').collect();
                let n = ls.len().max(1);
                let p = rng.below(n as u64) as usize;
                let new_line = format!("editor {ed} round {r} edit {j}\n").into_bytes();
                match rng.below(10) {
                    0..=4 => ls.insert(p, &new_line),
                    5..=7 if !ls.is_empty() => ls[p] = &new_line,
                    _ if ls.len() > 1 => {
                        ls.remove(p);
                    }
                    _ => ls.insert(p, &new_line),
                }
                let saved = ls.concat();
                let Some((del, ins)) =
                    diff_line(&base, &saved, &|l| doc.successor_at(l, &everything))
                else {
                    continue;
                };
                let t = edit(node, epoch, del, ins);
                doc.apply(id, &t);
                let mut deps = prev_round_last.clone();
                if let Some(p) = prev {
                    deps.insert(p);
                }
                prev = Some(id);
                round[ed].push(all.len());
                all.push(Authored { id, deps, edit: t });
            }
            if let Some(p) = prev {
                last_of_round.insert(p);
            }
        }
        // Sync: every editor receives every other editor's round ops.
        for (ed, doc) in docs.iter_mut().enumerate() {
            for (o, idxs) in round.iter().enumerate() {
                if o == ed {
                    continue;
                }
                for &i in idxs {
                    doc.apply(all[i].id, &all[i].edit);
                }
            }
        }
        prev_round_last.extend(last_of_round);
        by_round.push(round);
    }
    let gen_secs = t_gen.elapsed().as_secs_f64();
    let want = docs[0].materialize_head();
    let mut ok = docs.iter().all(|d| d.materialize_head() == want);
    // Three random causal orders: rounds in order, editors interleaved at
    // random within a round (each editor's own order kept).
    let mut times: Vec<f64> = Vec::new();
    let mut total = 0.0;
    for o in 0..3u64 {
        let mut rng = Rng::new(0x5EED + o);
        let mut d = TextDoc::new(epoch, &seed);
        for round in &by_round {
            let mut pos = vec![0usize; editors];
            let mut left: usize = round.iter().map(|v| v.len()).sum();
            while left > 0 {
                let ed = rng.below(editors as u64) as usize;
                if pos[ed] == round[ed].len() {
                    continue;
                }
                let a = &all[round[ed][pos[ed]]];
                pos[ed] += 1;
                left -= 1;
                let t0 = Instant::now();
                d.apply(a.id, &a.edit);
                let dt = t0.elapsed().as_secs_f64();
                total += dt;
                times.push(dt);
            }
        }
        ok &= d.materialize_head() == want;
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let q = |p: f64| times[((times.len() as f64 * p) as usize).min(times.len() - 1)] * 1e3;
    let p99 = q(0.99);
    let met = ok && p99 <= 1.0;
    ok &= p99 <= 1.0;
    println!(
        "B3: {editors} editors x {edits} edits ({} ops) on a {lines}-line file, {rounds} sync rounds; \
         3 random causal orders: {}; integrate per op p50 {:.4} ms, p99 {:.4} ms, max {:.3} ms, mean {:.4} ms \
         (target: convergence, p99 <= 1 ms: {}); final {} lines, {} runs; generation {:.1} s",
        all.len(),
        if docs.iter().all(|d| d.materialize_head() == want) { "converged" } else { "NOT CONVERGED" },
        q(0.5),
        p99,
        q(1.0),
        total / times.len() as f64 * 1e3,
        if met { "met" } else { "MISSED" },
        want.iter().filter(|&&b| b == b'\n').count(),
        docs[0].runs(),
        gen_secs,
    );
    // Full evaluation of the op set (validity, content, notices), for info.
    let blob = BlobId::of(&seed);
    let mut blobs = std::collections::BTreeMap::new();
    blobs.insert(blob, seed.clone());
    let mut ops = vec![
        Op {
            id: node,
            deps: BTreeSet::new(),
            payload: Payload::Create {
                kind: Kind::File,
                parent: ROOT,
                name: "f.txt".into(),
            },
        },
        Op {
            id: epoch,
            deps: [node].into_iter().collect(),
            payload: Payload::SetContent { node, blob },
        },
    ];
    ops.extend(all.iter().map(|a| Op {
        id: a.id,
        deps: a.deps.clone(),
        payload: Payload::TextEdit(a.edit.clone()),
    }));
    let t0 = Instant::now();
    let ev = evaluate_with(ops.iter(), &blobs);
    let ev_secs = t0.elapsed().as_secs_f64();
    let same = ev.text.content[&node].bytes.as_deref() == Some(&want[..]);
    ok &= same && ev.text.valid.len() == all.len();
    println!(
        "B3 evaluate_with over all {} ops: {:.2} s; valid {}/{}; {} notices; head {}",
        ops.len(),
        ev_secs,
        ev.text.valid.len(),
        all.len(),
        ev.text.notices.len(),
        if same { "matches" } else { "MISMATCH" }
    );
    ok
}

/// Per file: node, last op, epoch, seed length, last insert (op, length).
type B2File = (NodeId, OpId, OpId, u32, Option<(OpId, u32)>);

fn case_b2(nfiles: usize, nedits: usize) -> bool {
    let mut rng = Rng::new(54);
    let mut ops: Vec<Op> = Vec::with_capacity(2 * nfiles + nedits);
    let mut blobs: std::collections::HashMap<BlobId, Vec<u8>> = std::collections::HashMap::new();
    let mut counter = 10u64;
    let mut next = |r: u128| {
        counter += 1;
        NodeId::new(counter, r)
    };
    // Per file: (node, last op, epoch, seed len, last insert (op, len)).
    let mut files: Vec<B2File> = Vec::new();
    let mut seed_bytes = 0usize;
    let mut setup: Option<OpId> = None;
    for i in 0..nfiles {
        let n = next(1);
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
        seed_bytes += seed.len();
        let b = BlobId::of(&seed);
        let s = next(1);
        ops.push(Op {
            id: s,
            deps: [n].into(),
            payload: Payload::SetContent { node: n, blob: b },
        });
        setup = Some(s);
        files.push((n, s, s, seed.len() as u32, None));
        blobs.insert(b, seed);
    }
    // Each editor's ops form one causal chain (§2.6 #7, as real replicas
    // author on top of their own previous op).
    let mut mine: Vec<Option<OpId>> = vec![None; 20];
    for k in 0..nedits {
        let f = rng.below(nfiles as u64) as usize;
        let (n, last, epoch, slen, prev) = files[f];
        let r = 2 + (k % 20) as u128;
        let id = next(r);
        let mut deps: BTreeSet<OpId> = [last].into();
        deps.extend(mine[k % 20]);
        mine[k % 20] = Some(id);
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
    println!(
        "B2/B5: {nfiles} files, {} MiB of seeds, {nedits} TextEdits, {} ops",
        seed_bytes >> 20,
        ops.len()
    );
    let t = Instant::now();
    let mut r = Replica::new();
    r.receive_all_with(ops.clone(), &blobs);
    let replay = t.elapsed().as_secs_f64();
    let valid = r.text_valid().len();
    println!("  full replay (open without snapshot): {replay:.2} s, valid edits {valid}");
    let t = Instant::now();
    let snap = r.encode_snapshot().expect("closed");
    let enc = t.elapsed().as_secs_f64();
    let path = std::env::temp_dir().join(format!("hub54-b2-{}.snap", std::process::id()));
    std::fs::write(&path, &snap).unwrap();
    drop(r);
    let size = snap.len();
    drop(snap);
    // The CLI keeps seeds in `blobs/` and checks each referenced one
    // exists with its length before trusting the snapshot (A.2.9).
    let bdir = std::env::temp_dir().join(format!("hub54-b2-blobs-{}", std::process::id()));
    std::fs::create_dir_all(&bdir).unwrap();
    for (b, bytes) in &blobs {
        std::fs::write(bdir.join(b.hex()), bytes).unwrap();
    }
    alloc_peak_reset();
    let t = Instant::now();
    let bytes = std::fs::read(&path).unwrap();
    let read = t.elapsed().as_secs_f64();
    let back = Replica::decode_snapshot(&bytes).expect("decodes");
    let decoded = t.elapsed().as_secs_f64();
    let lazy = back.text_lazy_seeds();
    for (b, n) in &lazy {
        assert_eq!(
            std::fs::metadata(bdir.join(b.hex())).unwrap().len(),
            *n as u64
        );
    }
    let open = t.elapsed().as_secs_f64();
    drop(bytes);
    let peak = alloc_peak();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&bdir);
    println!(
        "  B2 open with snapshot: read {read:.2} s + decode {:.2} s + {} seed checks {:.2} s = {open:.2} s (target 1 s), peak {} MiB",
        decoded - read,
        lazy.len(),
        open - decoded,
        peak >> 20
    );
    // One more edit on the reopened replica (record's core step).
    let (n, last, epoch, _, _) = files[0];
    let mut back = back;
    let one = Op {
        id: next(99),
        deps: [last].into(),
        payload: Payload::TextEdit(TextEdit {
            node: n,
            epoch,
            seen: [last].into(),
            del: vec![],
            ins: vec![Ins {
                left: None,
                right: None,
                bytes: b"one more\n".to_vec(),
            }],
        }),
    };
    let t = Instant::now();
    back.receive_all_with(vec![one], &blobs);
    let rec = t.elapsed().as_secs_f64();
    println!(
        "  one more TextEdit on the reopened replica: {:.0} ms (target 1 s)",
        rec * 1e3
    );
    // B5 (7.11 target, Planner #54): size <= 1.2 x (live inserted bytes
    // + 40 B x runs) + a fixed overhead.
    let t = Instant::now();
    back.load_text_docs(&blobs).unwrap();
    let load = t.elapsed().as_secs_f64();
    let (mut runs, mut live) = (0usize, 0u64);
    for (n, _, e, _, _) in &files {
        let d = back.text_doc(*n, *e).unwrap();
        runs += d.runs();
        for (x, len, del) in d.elements() {
            if del.is_empty() && x.op != *e {
                live += len as u64;
            }
        }
    }
    let bound = 1.2 * (live as f64 + 40.0 * runs as f64);
    println!(
        "  B5 snapshot: {:.1} MiB; live inserted {:.1} MiB, {runs} runs; 1.2 x (live + 40 B x runs) = {:.1} MiB; size / that = {:.2} (fixed overhead {:.1} MiB if positive); encode {enc:.2} s; loading every lazy doc afterwards {load:.2} s",
        size as f64 / 1048576.0,
        live as f64 / 1048576.0,
        bound / 1048576.0,
        size as f64 / bound,
        (size as f64 - bound) / 1048576.0
    );
    let ok = valid == nedits && open <= 1.0 && rec <= 1.0;
    if !ok {
        println!("  B2: MISSED");
    }
    ok
}

fn main() {
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with("--"))
        .collect();
    let which = args.first().map(String::as_str).unwrap_or("all");
    let mut ok = true;
    if which == "all" || which == "b1" {
        let n = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(260_000);
        ok &= case_b1(n);
    }
    if which == "all" || which == "b3" {
        ok &= case_b3(20, 500, 10_000, 20);
    }
    if which == "all" || which == "b2" || which == "b5" {
        let f = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(10_000);
        let e = args
            .get(2)
            .and_then(|v| v.parse().ok())
            .unwrap_or(1_000_000);
        ok &= case_b2(f, e);
    }
    if !ok {
        eprintln!("text bench: a target was missed or a check failed (see above)");
        std::process::exit(1);
    }
}
