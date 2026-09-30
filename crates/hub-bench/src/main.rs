//! hub-bench: scale benchmarks and a multi-replica scenario runner for
//! barocss-hub (Issue #27). It drives the release `hub` binary as a
//! subprocess, under `/usr/bin/time -l` (macOS) for peak RSS, with a hard
//! per-command timeout. It never links product code, so it measures exactly
//! what a user runs. See README.md for usage.
//!
//! Every generator is seeded (xorshift64*), so a rerun with the same flags
//! builds byte-identical working trees.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::Hasher;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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

struct Ctx {
    hub: PathBuf,
    root: PathBuf,
    out: PathBuf,
    timeout: Duration,
}

#[derive(Clone, Debug)]
struct Meas {
    wall: f64,
    rss_mb: f64,
    ok: bool,
    timed_out: bool,
    stdout: String,
}

impl Ctx {
    /// Run `hub <args>` in `dir`, measuring wall time and peak RSS.
    fn run(&self, dir: &Path, args: &[&str], keep_stdout: bool) -> Meas {
        let errf = self.root.join(".time.err");
        let outf = self.root.join(".time.out");
        let mut cmd = Command::new("/usr/bin/time");
        cmd.arg("-l")
            .arg(&self.hub)
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(fs::File::create(&outf).unwrap())
            .stderr(fs::File::create(&errf).unwrap())
            .process_group(0);
        let t0 = Instant::now();
        let mut child = cmd.spawn().expect("spawn /usr/bin/time");
        let mut timed_out = false;
        let status = loop {
            if let Some(s) = child.try_wait().unwrap() {
                break Some(s);
            }
            if t0.elapsed() > self.timeout {
                let _ = Command::new("kill")
                    .args(["-9", &format!("-{}", child.id())])
                    .status();
                let _ = child.wait();
                timed_out = true;
                break None;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let wall = t0.elapsed().as_secs_f64();
        let err = fs::read_to_string(&errf).unwrap_or_default();
        let rss = err
            .lines()
            .find(|l| l.contains("maximum resident set size"))
            .and_then(|l| l.split_whitespace().next())
            .and_then(|n| n.parse::<f64>().ok())
            .unwrap_or(0.0)
            / 1048576.0;
        let ok = status.map(|s| s.success()).unwrap_or(false);
        if !ok && !timed_out {
            let tail: Vec<&str> = err.lines().take(5).collect();
            eprintln!("  ! hub {:?} failed: {}", args, tail.join(" | "));
        }
        let stdout = if keep_stdout {
            fs::read_to_string(&outf).unwrap_or_default()
        } else {
            String::new()
        };
        Meas {
            wall,
            rss_mb: rss,
            ok,
            timed_out,
            stdout,
        }
    }

    fn report(&self, case: &str, size: &str, what: &str, m: &Meas, extra: &str) {
        let st = if m.timed_out {
            "TIMEOUT"
        } else if m.ok {
            "ok"
        } else {
            "FAIL"
        };
        let line = format!(
            "| {case} | {size} | {what} | {:.3} | {:.1} | {st} | {extra} |",
            m.wall, m.rss_mb
        );
        println!("{line}");
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.out)
            .unwrap();
        writeln!(f, "{line}").unwrap();
    }

    fn fresh(&self, name: &str) -> PathBuf {
        let d = self.root.join(name);
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }
}

fn content(rng: &mut Rng, len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    while v.len() < len {
        let x = rng.next();
        for b in x.to_le_bytes() {
            // printable text keeps the files source-like
            v.push(b'a' + (b % 26));
        }
    }
    v.truncate(len);
    v
}

/// `n` files, 100 per directory, `size` bytes each.
fn file_path(i: usize) -> String {
    format!("d{:04}/f{:06}.txt", i / 100, i)
}

fn gen_tree(dir: &Path, n: usize, size: usize, seed: u64) {
    let mut rng = Rng::new(seed);
    for i in 0..n {
        let p = dir.join(file_path(i));
        if i % 100 == 0 {
            fs::create_dir_all(p.parent().unwrap()).unwrap();
        }
        fs::write(p, content(&mut rng, size)).unwrap();
    }
}

/// Edit `k` files. With `variant = Some(v)` every edited file gets the same
/// content `v`, so blobs dedup and the run isolates op-log cost from blob
/// writes; with `None` every edit is a unique blob.
fn edit_files(dir: &Path, n_files: usize, k: usize, rng: &mut Rng, variant: Option<u64>) {
    // k distinct-ish files (sequential window from a random start)
    let start = rng.below(n_files as u64) as usize;
    for j in 0..k.min(n_files) {
        let i = (start + j) % n_files;
        let tag = rng.next();
        let body = match variant {
            Some(v) => format!("variant {v}\n"),
            None => format!("edit {tag:016x}\n"),
        };
        fs::write(dir.join(file_path(i)), body).unwrap();
    }
}

fn du_mb(p: &Path) -> f64 {
    let o = Command::new("du").arg("-sk").arg(p).output().unwrap();
    String::from_utf8_lossy(&o.stdout)
        .split_whitespace()
        .next()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0)
        / 1024.0
}

fn head_of(status_out: &str) -> String {
    status_out
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("?")
        .to_string()
}

fn ops_of(status_out: &str) -> String {
    status_out
        .lines()
        .next()
        .and_then(|l| l.split('(').nth(1))
        .and_then(|l| l.split_whitespace().next())
        .unwrap_or("?")
        .to_string()
}

/// Hash of every regular file and directory outside `.hub/` (sorted paths).
fn tree_hash(root: &Path) -> u64 {
    fn walk(base: &Path, d: &Path, acc: &mut Vec<(String, Option<Vec<u8>>)>) {
        for e in fs::read_dir(d).unwrap() {
            let e = e.unwrap();
            let p = e.path();
            let rel = p.strip_prefix(base).unwrap().to_string_lossy().to_string();
            if rel == ".hub" {
                continue;
            }
            if e.file_type().unwrap().is_dir() {
                acc.push((rel, None));
                walk(base, &p, acc);
            } else {
                acc.push((rel, Some(fs::read(&p).unwrap())));
            }
        }
    }
    let mut acc = Vec::new();
    walk(root, root, &mut acc);
    acc.sort();
    let mut h = DefaultHasher::new();
    for (p, c) in acc {
        h.write(p.as_bytes());
        h.write_u8(0);
        if let Some(c) = c {
            h.write(&c);
        }
        h.write_u8(1);
    }
    h.finish()
}

/// Clone `src` into a fresh dir: init + sync (+ checkout). Returns dir and the sync/checkout measurements.
fn clone_repo(ctx: &Ctx, src: &Path, name: &str, checkout: bool) -> (PathBuf, Meas, Option<Meas>) {
    let d = ctx.fresh(name);
    ctx.run(&d, &["init"], false);
    let s = ctx.run(&d, &["sync", src.to_str().unwrap()], false);
    let c = checkout.then(|| ctx.run(&d, &["checkout"], false));
    (d, s, c)
}

// ---------------------------------------------------------------- cases

fn case_files(ctx: &Ctx, n: usize) {
    let size = format!("{n} files");
    let a = ctx.fresh(&format!("files-{n}"));
    gen_tree(&a, n, 1024, 42);
    let m = ctx.run(&a, &["init"], false);
    ctx.report("files", &size, "init", &m, "");
    let m = ctx.run(&a, &["record", "-m", "base"], false);
    let hubmb = du_mb(&a.join(".hub"));
    ctx.report(
        "files",
        &size,
        "first record",
        &m,
        &format!(".hub {hubmb:.0} MB"),
    );
    if m.timed_out {
        return;
    }
    let m = ctx.run(&a, &["status"], false);
    ctx.report("files", &size, "status (clean)", &m, "");
    let mut rng = Rng::new(7);
    edit_files(&a, n, 1, &mut rng, None);
    let m = ctx.run(&a, &["status"], false);
    ctx.report("files", &size, "status (1 edit)", &m, "");
    let m = ctx.run(&a, &["record"], false);
    ctx.report("files", &size, "record (1 edit)", &m, "");
    let m = ctx.run(&a, &["log"], false);
    ctx.report("files", &size, "log", &m, "");
    let (b, s, c) = clone_repo(ctx, &a, &format!("files-{n}-clone"), true);
    ctx.report("files", &size, "sync into empty repo", &s, "");
    ctx.report("files", &size, "checkout into empty dir", &c.unwrap(), "");
    let _ = fs::remove_dir_all(b);
}

/// Grow the op log to each target in `targets` with rounds of `per` edits
/// over a `files`-file tree; measure status/record/log/checkout at each.
fn case_ops(ctx: &Ctx, files: usize, per: usize, targets: &[usize]) {
    let a = ctx.fresh("ops");
    gen_tree(&a, files, 256, 43);
    ctx.run(&a, &["init"], false);
    ctx.run(&a, &["record", "-m", "base"], false);
    let mut ops = 2 * files + files / 100;
    let mut rng = Rng::new(9);
    let mut round = 0;
    let mut worst_round = 0.0f64;
    for &t in targets {
        let t0 = Instant::now();
        while ops < t {
            edit_files(&a, files, per, &mut rng, Some(round as u64 % 3));
            round += 1;
            let m = ctx.run(&a, &["record", "-m", &format!("r{round}")], false);
            worst_round = worst_round.max(m.wall);
            if !m.ok {
                ctx.report(
                    "ops",
                    &format!("~{ops} ops"),
                    "record round",
                    &m,
                    "stopping",
                );
                return;
            }
            ops += per;
        }
        let size = format!("{ops} ops");
        eprintln!(
            "  built {ops} ops in {:.0}s (slowest record round {worst_round:.2}s)",
            t0.elapsed().as_secs_f64()
        );
        let hubmb = du_mb(&a.join(".hub"));
        let m = ctx.run(&a, &["status"], false);
        ctx.report(
            "ops",
            &size,
            "status (clean)",
            &m,
            &format!(".hub {hubmb:.0} MB, {round} ckpts"),
        );
        edit_files(&a, files, 1, &mut rng, None);
        let m = ctx.run(&a, &["status"], false);
        ctx.report("ops", &size, "status (1 edit)", &m, "");
        let m = ctx.run(&a, &["record"], false);
        ops += 1;
        ctx.report("ops", &size, "record (1 edit)", &m, "");
        let m = ctx.run(&a, &["log"], false);
        ctx.report("ops", &size, "log", &m, "");
        let (b, s, c) = clone_repo(ctx, &a, "ops-clone", true);
        ctx.report("ops", &size, "sync into empty repo", &s, "");
        ctx.report("ops", &size, "checkout into empty dir", &c.unwrap(), "");
        let _ = fs::remove_dir_all(b);
        if m.timed_out || s.timed_out || m.wall > 300.0 {
            eprintln!("  axis stopped: unusable at {ops} ops");
            return;
        }
    }
}

/// A and B share a `files`-file base; B makes K ops offline; A syncs B.
fn case_delta(ctx: &Ctx, files: usize, ks: &[usize]) {
    let a = ctx.fresh("delta-a");
    gen_tree(&a, files, 256, 44);
    ctx.run(&a, &["init"], false);
    ctx.run(&a, &["record", "-m", "base"], false);
    for &k in ks {
        let (b, _, _) = clone_repo(ctx, &a, "delta-b", true);
        let mut rng = Rng::new(k as u64);
        let mut left = k;
        while left > 0 {
            let step = left.min(files);
            edit_files(&b, files, step, &mut rng, Some(left as u64));
            ctx.run(&b, &["record"], false);
            left -= step;
        }
        let m = ctx.run(&a, &["sync", b.to_str().unwrap()], false);
        ctx.report(
            "sync delta",
            &format!("K={k} ops ({files}-file base)"),
            "sync",
            &m,
            "",
        );
        let m = ctx.run(&a, &["checkout"], false);
        ctx.report(
            "sync delta",
            &format!("K={k} ops ({files}-file base)"),
            "checkout after sync",
            &m,
            "",
        );
        let _ = fs::remove_dir_all(b);
    }
}

fn case_blobs(ctx: &Ctx, count: usize, mb_each: usize) {
    let size = format!("{count} x {mb_each} MB");
    let a = ctx.fresh("blobs-a");
    let mut rng = Rng::new(45);
    fs::create_dir_all(a.join("big")).unwrap();
    for i in 0..count {
        let mut f = fs::File::create(a.join(format!("big/b{i:05}.bin"))).unwrap();
        let chunk = 1 << 20;
        for _ in 0..mb_each {
            f.write_all(&content(&mut rng, chunk)).unwrap();
        }
    }
    ctx.run(&a, &["init"], false);
    let m = ctx.run(&a, &["record", "-m", "blobs"], false);
    ctx.report(
        "blobs",
        &size,
        "record",
        &m,
        &format!(".hub {:.0} MB", du_mb(&a.join(".hub"))),
    );
    let m = ctx.run(&a, &["status"], false);
    ctx.report("blobs", &size, "status (clean)", &m, "");
    let (b, s, c) = clone_repo(ctx, &a, "blobs-b", true);
    ctx.report("blobs", &size, "sync into empty repo", &s, "");
    ctx.report("blobs", &size, "checkout into empty dir", &c.unwrap(), "");
    let _ = fs::remove_dir_all(b);
    let _ = fs::remove_dir_all(a);
}

fn cpu_secs(pid: u32) -> Option<f64> {
    let o = Command::new("ps")
        .args(["-o", "time=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
    // [[dd-]hh:]mm:ss.cc
    let mut total = 0.0;
    for part in s.split(':') {
        total = total * 60.0 + part.parse::<f64>().ok()?;
    }
    Some(total)
}

fn case_watch(ctx: &Ctx, n: usize, secs: u64) {
    let a = ctx.fresh(&format!("watch-{n}"));
    gen_tree(&a, n, 256, 46);
    ctx.run(&a, &["init"], false);
    ctx.run(&a, &["record", "-m", "base"], false);
    let mut child = Command::new(&ctx.hub)
        .arg("watch")
        .current_dir(&a)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(3)); // startup scan
    let c0 = cpu_secs(child.id()).unwrap_or(0.0);
    std::thread::sleep(Duration::from_secs(secs));
    let c1 = cpu_secs(child.id()).unwrap_or(0.0);
    let rss = Command::new("ps")
        .args(["-o", "rss=", "-p", &child.id().to_string()])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<f64>()
                .unwrap_or(0.0)
                / 1024.0
        })
        .unwrap_or(0.0);
    let _ = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status();
    std::thread::sleep(Duration::from_millis(500));
    let _ = child.kill();
    let _ = child.wait();
    let pct = 100.0 * (c1 - c0) / secs as f64;
    let polls = secs as f64 * 10.0; // default --poll 100 ms
    let m = Meas {
        wall: secs as f64,
        rss_mb: rss,
        ok: true,
        timed_out: false,
        stdout: String::new(),
    };
    ctx.report(
        "watch idle",
        &format!("{n} files"),
        &format!("{secs}s idle, --poll 100"),
        &m,
        &format!(
            "CPU {pct:.1}% of one core, {:.1} ms CPU per poll",
            1000.0 * (c1 - c0) / polls
        ),
    );
}

/// R replicas share a base, each makes `edits` offline edit rounds, then a
/// ring or star mesh syncs until heads agree. Asserts convergence.
fn case_scenario(ctx: &Ctx, r: usize, topo: &str, files: usize, edits: usize) -> bool {
    let size = format!("R={r} {topo}");
    let base = ctx.fresh("sc-base");
    gen_tree(&base, files, 256, 47);
    ctx.run(&base, &["init"], false);
    ctx.run(&base, &["record", "-m", "base"], false);
    let mut reps = Vec::new();
    for i in 0..r {
        let (d, _, _) = clone_repo(ctx, &base, &format!("sc-r{i:02}"), true);
        reps.push(d);
    }
    // offline work: each replica edits, adds, moves and deletes
    let mut rng = Rng::new(1000 + r as u64);
    let mut local_ops = 0usize;
    for (i, d) in reps.iter().enumerate() {
        for e in 0..edits {
            edit_files(d, files, 5, &mut rng, None);
            fs::write(d.join(format!("new-r{i}-{e}.txt")), format!("r{i} e{e}\n")).unwrap();
            let victim = d.join(file_path(rng.below(files as u64) as usize));
            if rng.below(4) == 0 {
                let _ = fs::remove_file(&victim);
            } else if rng.below(4) == 0 {
                let _ = fs::rename(&victim, d.join(format!("moved-r{i}-{e}.txt")));
            }
            let m = ctx.run(d, &["record"], false);
            if !m.ok {
                ctx.report("scenario", &size, "offline record", &m, "");
            }
            local_ops += 8;
        }
    }
    let t0 = Instant::now();
    let mut syncs = 0;
    let mut worst = 0.0f64;
    let mut passes = 0;
    let heads = |reps: &Vec<PathBuf>| -> Vec<String> {
        reps.iter()
            .map(|d| head_of(&ctx.run(d, &["status"], true).stdout))
            .collect()
    };
    loop {
        passes += 1;
        match topo {
            "ring" => {
                for i in 0..r {
                    let j = (i + 1) % r;
                    let m = ctx.run(&reps[i], &["sync", reps[j].to_str().unwrap()], false);
                    worst = worst.max(m.wall);
                    syncs += 1;
                }
            }
            _ => {
                for i in 1..r {
                    let m = ctx.run(&reps[i], &["sync", reps[0].to_str().unwrap()], false);
                    worst = worst.max(m.wall);
                    syncs += 1;
                }
            }
        }
        let h = heads(&reps);
        if h.iter().all(|x| *x == h[0]) || passes >= 4 {
            break;
        }
    }
    let sync_time = t0.elapsed().as_secs_f64();
    let h = heads(&reps);
    let st = ctx.run(&reps[0], &["status"], true).stdout;
    let conflicts = st
        .lines()
        .find(|l| l.starts_with("conflicts"))
        .unwrap_or("?")
        .to_string();
    let mut hashes = Vec::new();
    for d in &reps {
        let m = ctx.run(d, &["checkout"], false);
        if !m.ok {
            ctx.report("scenario", &size, "checkout", &m, "");
        }
        hashes.push(tree_hash(d));
    }
    let same_head = h.iter().all(|x| *x == h[0]);
    let same_tree = hashes.iter().all(|x| *x == hashes[0]);
    let conv = same_head && same_tree;
    let m = Meas {
        wall: sync_time,
        rss_mb: 0.0,
        ok: conv,
        timed_out: false,
        stdout: String::new(),
    };
    ctx.report(
        "scenario",
        &size,
        "mesh sync total",
        &m,
        &format!(
            "{passes} passes, {syncs} syncs, slowest sync {worst:.3}s, ~{local_ops} offline changes, head {} ops {}, {conflicts}, converged={conv} (head {same_head}, tree {same_tree})",
            h[0],
            ops_of(&st)
        ),
    );
    for d in reps {
        let _ = fs::remove_dir_all(d);
    }
    let _ = fs::remove_dir_all(base);
    conv
}

/// D-030 (Issue #34): R replicas copied from one base (`cp -R` + `hub reid`),
/// each records `rounds` rounds of `per` unique-content edits (about
/// R*rounds*per SetContent ops in total), then star (every replica syncs
/// with replica 0) or ring passes until all heads agree (max `max_passes`).
/// Reports every sync of the first pass (k = sync index) with wall and peak
/// RSS, pass totals, status/record at replica 0, and asserts convergence:
/// same head short id and identical checked-out trees.
#[allow(clippy::too_many_arguments)]
fn case_d030(
    ctx: &Ctx,
    r: usize,
    topo: &str,
    files: usize,
    rounds: usize,
    per: usize,
    max_passes: usize,
    every: usize,
) -> bool {
    let size = format!("R={r} {topo} {}x{}x{}", r, rounds, per);
    let base = ctx.fresh("d30-base");
    gen_tree(&base, files, 64, 30);
    ctx.run(&base, &["init"], false);
    ctx.run(&base, &["record", "-m", "base"], false);
    let t_build = Instant::now();
    let mut reps = Vec::new();
    let mut rng = Rng::new(3000 + r as u64);
    for i in 0..r {
        let d = ctx.root.join(format!("d30-r{i:03}"));
        let _ = fs::remove_dir_all(&d);
        let st = Command::new("cp")
            .arg("-R")
            .arg(&base)
            .arg(&d)
            .status()
            .unwrap();
        assert!(st.success(), "cp -R");
        let m = ctx.run(&d, &["reid"], false);
        assert!(m.ok, "reid");
        for _ in 0..rounds {
            edit_files(&d, files, per, &mut rng, None);
            let m = ctx.run(&d, &["record"], false);
            if !m.ok {
                ctx.report("d030", &size, "offline record", &m, "");
            }
        }
        reps.push(d);
    }
    let build = Meas {
        wall: t_build.elapsed().as_secs_f64(),
        rss_mb: 0.0,
        ok: true,
        timed_out: false,
        stdout: String::new(),
    };
    ctx.report("d030", &size, "build replicas", &build, "");
    let heads = |reps: &Vec<PathBuf>| -> Vec<String> {
        reps.iter()
            .map(|d| head_of(&ctx.run(d, &["status"], true).stdout))
            .collect()
    };
    let t0 = Instant::now();
    let mut passes = 0;
    let mut worst = 0.0f64;
    let mut worst_rss = 0.0f64;
    let mut syncs = 0;
    let mut failed = false;
    loop {
        passes += 1;
        let tp = Instant::now();
        let pairs: Vec<(usize, usize)> = match topo {
            "ring" => (0..r).map(|i| (i, (i + 1) % r)).collect(),
            _ => (1..r).map(|i| (i, 0)).collect(),
        };
        for (k, (i, j)) in pairs.into_iter().enumerate() {
            let m = ctx.run(&reps[i], &["sync", reps[j].to_str().unwrap()], false);
            worst = worst.max(m.wall);
            worst_rss = worst_rss.max(m.rss_mb);
            syncs += 1;
            let k = k + 1;
            if !m.ok {
                ctx.report("d030", &size, &format!("pass {passes} sync k={k}"), &m, "");
                failed = true;
                break;
            }
            if passes == 1 && (k == 1 || k % every == 0 || k + 1 == r) {
                ctx.report("d030", &size, &format!("pass 1 sync k={k}"), &m, "");
            }
        }
        let pm = Meas {
            wall: tp.elapsed().as_secs_f64(),
            rss_mb: 0.0,
            ok: !failed,
            timed_out: false,
            stdout: String::new(),
        };
        ctx.report("d030", &size, &format!("pass {passes} total"), &pm, "");
        if failed {
            break;
        }
        let h = heads(&reps);
        if h.iter().all(|x| *x == h[0]) || passes >= max_passes {
            break;
        }
    }
    let sync_time = t0.elapsed().as_secs_f64();
    let st = ctx.run(&reps[0], &["status"], true);
    ctx.report("d030", &size, "status at replica 0", &st, "");
    let h = heads(&reps);
    let mut hashes = Vec::new();
    for d in &reps {
        let m = ctx.run(d, &["checkout"], false);
        if !m.ok {
            ctx.report("d030", &size, "checkout", &m, "");
        }
        hashes.push(tree_hash(d));
    }
    edit_files(&reps[0], files, 10, &mut rng, None);
    let rec = ctx.run(&reps[0], &["record"], false);
    ctx.report("d030", &size, "record 10 edits at replica 0", &rec, "");
    let same_head = h.iter().all(|x| *x == h[0]);
    let same_tree = hashes.iter().all(|x| *x == hashes[0]);
    let conv = !failed && same_head && same_tree;
    let m = Meas {
        wall: sync_time,
        rss_mb: worst_rss,
        ok: conv,
        timed_out: false,
        stdout: String::new(),
    };
    ctx.report(
        "d030",
        &size,
        "mesh sync total",
        &m,
        &format!(
            "{passes} passes, {syncs} syncs, slowest sync {worst:.3}s, head {} ops {}, converged={conv} (head {same_head}, tree {same_tree})",
            h[0],
            ops_of(&st.stdout)
        ),
    );
    for d in reps {
        let _ = fs::remove_dir_all(d);
    }
    let _ = fs::remove_dir_all(base);
    conv
}

fn list(s: &str) -> Vec<usize> {
    s.split(',')
        .map(|x| x.parse().expect("number list"))
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str, def: &str| -> String {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1).cloned())
            .unwrap_or_else(|| def.to_string())
    };
    let case = args.first().cloned().unwrap_or_default();
    let root = PathBuf::from(flag("--root", "/tmp/hub-bench"));
    fs::create_dir_all(&root).unwrap();
    let hub = PathBuf::from(flag("--hub", "target/release/barocss-hub"));
    let hub =
        fs::canonicalize(&hub).expect("--hub: release barocss-hub binary (cargo build --release)");
    let ctx = Ctx {
        hub,
        out: root.join("results.md"),
        root: fs::canonicalize(&root).unwrap(),
        timeout: Duration::from_secs(flag("--timeout", "300").parse().unwrap()),
    };
    match case.as_str() {
        "files" => {
            for n in list(&flag("--n", "1000,10000,100000")) {
                case_files(&ctx, n);
            }
        }
        "ops" => case_ops(
            &ctx,
            flag("--files", "5000").parse().unwrap(),
            flag("--per", "5000").parse().unwrap(),
            &list(&flag("--targets", "10000,100000,1000000")),
        ),
        "delta" => case_delta(
            &ctx,
            flag("--files", "10000").parse().unwrap(),
            &list(&flag("--k", "10,1000,100000")),
        ),
        "blobs" => case_blobs(
            &ctx,
            flag("--count", "1").parse().unwrap(),
            flag("--mb", "1024").parse().unwrap(),
        ),
        "watch" => {
            let secs = flag("--secs", "60").parse().unwrap();
            for n in list(&flag("--n", "10000,100000")) {
                case_watch(&ctx, n, secs);
            }
        }
        "scenario" => {
            let files = flag("--files", "1000").parse().unwrap();
            let edits = flag("--edits", "3").parse().unwrap();
            let mut all = true;
            for r in list(&flag("--r", "2,10,50")) {
                for topo in ["ring", "star"] {
                    all &= case_scenario(&ctx, r, topo, files, edits);
                }
            }
            if !all {
                eprintln!("scenario: NOT all runs converged");
                std::process::exit(1);
            }
        }
        "d030" => {
            let r: usize = flag("--r", "100").parse().unwrap();
            let ok = case_d030(
                &ctx,
                r,
                &flag("--topo", "star"),
                flag("--files", "1000").parse().unwrap(),
                flag("--rounds", "10").parse().unwrap(),
                flag("--per", "100").parse().unwrap(),
                flag("--passes", "4").parse().unwrap(),
                flag("--every", "10").parse().unwrap(),
            );
            if !ok {
                eprintln!("d030: NOT converged");
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!(
                "usage: barocss-hub-bench <files|ops|delta|blobs|watch|scenario|d030> [--root DIR] [--hub PATH] [--timeout S] [case flags]\n\
                 see crates/hub-bench/README.md"
            );
            std::process::exit(2);
        }
    }
}
