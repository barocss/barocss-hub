//! Issue #22 property: random record sessions in 2-3 repos plus a random
//! sync schedule converge to the same head id and checked-out bytes.
//! Runs the CLI in-process (`hub_cli::run`) in temp dirs, for speed.
//!
//! Issue #23: steps also include interrupted syncs
//! (`hub_cli::sync::Interrupt::Return`, the in-process equivalent of the
//! `HUB_TEST_CRASH_IN_SYNC` abort: stop after k op appends). Convergence
//! is still asserted after the final full schedule reruns every sync. The
//! share of cases with at least one interrupt that actually fired is
//! measured, printed, and required to be >= 20%.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

use hub_cli::sync::Interrupt;
use proptest::prelude::*;
use proptest::test_runner::{RngAlgorithm, TestCaseError, TestRng, TestRunner};

fn run(dir: &Path, args: &[&str]) -> String {
    let a: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let (mut out, mut warn) = (Vec::new(), Vec::new());
    if let Err(e) = hub_cli::run(&a, dir, &mut out, &mut warn) {
        panic!("hub {args:?} in {}: {e}", dir.display());
    }
    String::from_utf8(out).unwrap()
}

fn snapshot(root: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    fn walk(dir: &Path, rel: &str, out: &mut BTreeMap<String, Option<Vec<u8>>>) {
        for e in fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let name = e.file_name().into_string().unwrap();
            if rel.is_empty() && name == ".hub" {
                continue;
            }
            let p = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            if e.file_type().unwrap().is_dir() {
                out.insert(p.clone(), None);
                walk(&e.path(), &p, out);
            } else {
                out.insert(p, Some(fs::read(e.path()).unwrap()));
            }
        }
    }
    let mut m = BTreeMap::new();
    walk(root, "", &mut m);
    m
}

fn head(d: &Path) -> String {
    run(d, &["status"])
        .lines()
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .to_string()
}

#[derive(Clone, Debug)]
enum Step {
    /// In repo `r`: write file `f{k}` with `v` (len varies), or delete it.
    Write {
        r: usize,
        k: u8,
        v: u8,
    },
    Delete {
        r: usize,
        k: u8,
    },
    Sync {
        a: usize,
        b: usize,
    },
    /// Sync `a` with `b`, stopping after `k` op appends (across both).
    Interrupted {
        a: usize,
        b: usize,
        k: usize,
    },
}

fn step(n: usize) -> impl Strategy<Value = Step> {
    prop_oneof![
        3 => (0..n, 0u8..4, any::<u8>()).prop_map(|(r, k, v)| Step::Write { r, k, v }),
        1 => (0..n, 0u8..4).prop_map(|(r, k)| Step::Delete { r, k }),
        2 => (0..n, 0..n).prop_map(|(a, b)| Step::Sync { a, b }),
        3 => (0..n, 0..n, 0usize..2).prop_map(|(a, b, k)| Step::Interrupted { a, b, k }),
    ]
}

fn case() -> impl Strategy<Value = (usize, Vec<Step>)> {
    (2usize..=3).prop_flat_map(|n| (Just(n), prop::collection::vec(step(n), 1..12)))
}

static CASE: AtomicUsize = AtomicUsize::new(0);
static CASES_RUN: AtomicUsize = AtomicUsize::new(0);
static CASES_INTERRUPTED: AtomicUsize = AtomicUsize::new(0);
static SYNCS: AtomicUsize = AtomicUsize::new(0);
static SYNCS_INTERRUPTED: AtomicUsize = AtomicUsize::new(0);

fn one_case(n: usize, steps: &[Step]) -> Result<(), TestCaseError> {
    let t = std::env::temp_dir().join(format!(
        "hub-sync-prop-{}-{}",
        std::process::id(),
        CASE.fetch_add(1, SeqCst)
    ));
    let _ = fs::remove_dir_all(&t);
    let rs: Vec<PathBuf> = (0..n).map(|i| t.join(format!("r{i}"))).collect();
    for r in &rs {
        fs::create_dir_all(r).unwrap();
        run(r, &["init"]);
    }
    let checkout_both = |a: usize, b: usize| {
        run(&rs[a], &["checkout"]);
        run(&rs[b], &["checkout"]);
    };
    let sync = |a: usize, b: usize| {
        run(&rs[a], &["sync", rs[b].to_str().unwrap()]);
        checkout_both(a, b);
    };
    let mut fired = false;
    for s in steps {
        match *s {
            Step::Write { r, k, v } => {
                let body = vec![b'a' + (v % 26); 1 + v as usize % 7];
                fs::write(rs[r].join(format!("f{k}")), body).unwrap();
                run(&rs[r], &["record"]);
            }
            Step::Delete { r, k } => {
                let p = rs[r].join(format!("f{k}"));
                if p.is_file() {
                    fs::remove_file(p).unwrap();
                    run(&rs[r], &["record"]);
                }
            }
            Step::Sync { a, b } if a != b => {
                SYNCS.fetch_add(1, SeqCst);
                sync(a, b)
            }
            Step::Interrupted { a, b, k } if a != b => {
                SYNCS.fetch_add(1, SeqCst);
                let (mut out, mut warn) = (Vec::new(), Vec::new());
                match hub_cli::sync::sync_with(
                    &rs[a],
                    rs[b].to_str().unwrap(),
                    Some(Interrupt::Return(k)),
                    &mut out,
                    &mut warn,
                ) {
                    Ok(()) => {}
                    Err(e) if e.0 == hub_cli::sync::INTERRUPTED => {
                        fired = true;
                        SYNCS_INTERRUPTED.fetch_add(1, SeqCst);
                    }
                    Err(e) => panic!("interrupted sync failed otherwise: {e}"),
                }
                // Both logs are valid prefixes: checkout works on each.
                checkout_both(a, b);
            }
            Step::Sync { .. } | Step::Interrupted { .. } => {}
        }
    }
    CASES_RUN.fetch_add(1, SeqCst);
    if fired {
        CASES_INTERRUPTED.fetch_add(1, SeqCst);
    }
    // Full schedule: a chain there and back reaches the join everywhere.
    for i in 0..n - 1 {
        sync(i, i + 1);
    }
    for i in (0..n - 1).rev() {
        sync(i, i + 1);
    }
    let h = head(&rs[0]);
    let snap = snapshot(&rs[0]);
    for r in &rs[1..] {
        prop_assert_eq!(head(r), h.clone());
        prop_assert_eq!(snapshot(r), snap.clone());
    }
    fs::remove_dir_all(&t).unwrap();
    Ok(())
}

#[test]
fn random_sessions_and_syncs_converge() {
    // Seed pinned (deterministic ChaCha RNG) so the >= 20% interruption
    // floor below cannot flake on an unlucky random seed (#23 review).
    let mut runner = TestRunner::new_with_rng(
        ProptestConfig {
            cases: 64,
            failure_persistence: None,
            ..ProptestConfig::default()
        },
        TestRng::deterministic_rng(RngAlgorithm::ChaCha),
    );
    runner
        .run(&case(), |(n, steps)| one_case(n, &steps))
        .unwrap();
    // Rates over every case run (including any shrinking, which only
    // happens on failure, and then the run above already failed).
    let (c, ci) = (CASES_RUN.load(SeqCst), CASES_INTERRUPTED.load(SeqCst));
    let (s, si) = (SYNCS.load(SeqCst), SYNCS_INTERRUPTED.load(SeqCst));
    eprintln!(
        "sync_prop interruption rate: {ci}/{c} cases ({:.0}%) had >= 1 interrupted sync; \
         {si}/{s} syncs interrupted",
        100.0 * ci as f64 / c as f64
    );
    assert!(ci * 5 >= c, "interrupted cases {ci}/{c} below 20%");
}
