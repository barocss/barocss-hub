//! Issue #22 property: random record sessions in 2-3 repos plus a random
//! sync schedule converge to the same head id and checked-out bytes.
//! Runs the CLI in-process (`hub_cli::run`) in temp dirs, for speed.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use proptest::prelude::*;

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
}

fn step(n: usize) -> impl Strategy<Value = Step> {
    prop_oneof![
        3 => (0..n, 0u8..4, any::<u8>()).prop_map(|(r, k, v)| Step::Write { r, k, v }),
        1 => (0..n, 0u8..4).prop_map(|(r, k)| Step::Delete { r, k }),
        2 => (0..n, 0..n).prop_map(|(a, b)| Step::Sync { a, b }),
    ]
}

fn case() -> impl Strategy<Value = (usize, Vec<Step>)> {
    (2usize..=3).prop_flat_map(|n| (Just(n), prop::collection::vec(step(n), 1..12)))
}

static CASE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]
    #[test]
    fn random_sessions_and_syncs_converge((n, steps) in case()) {
        let t = std::env::temp_dir().join(format!(
            "hub-sync-prop-{}-{}",
            std::process::id(),
            CASE.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&t);
        let rs: Vec<PathBuf> = (0..n).map(|i| t.join(format!("r{i}"))).collect();
        for r in &rs {
            fs::create_dir_all(r).unwrap();
            run(r, &["init"]);
        }
        let sync = |a: usize, b: usize| {
            run(&rs[a], &["sync", rs[b].to_str().unwrap()]);
            run(&rs[a], &["checkout"]);
            run(&rs[b], &["checkout"]);
        };
        for s in &steps {
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
                Step::Sync { a, b } if a != b => sync(a, b),
                Step::Sync { .. } => {}
            }
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
    }
}
