//! Issue #24 property: for random trees and random `.hubignore` rule sets,
//! `record` then checkout into a fresh replica (sync + checkout) reproduces
//! exactly the non-ignored paths and bytes, and `checkout --force` in the
//! original leaves every ignored file untouched. Temp dirs only.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hub_cli::ignore::Ignore;
use proptest::prelude::*;

type Snap = BTreeMap<String, Option<Vec<u8>>>;

fn tmp() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let d = std::env::temp_dir().join(format!(
        "hub-ign-prop-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn hub(d: &Path, args: &[&str]) -> Result<String, String> {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let (mut out, mut warn) = (Vec::new(), Vec::new());
    hub_cli::run(&args, d, &mut out, &mut warn).map_err(|e| e.0)?;
    Ok(String::from_utf8(out).unwrap())
}

fn snapshot(root: &Path) -> Snap {
    fn walk(dir: &Path, rel: &str, out: &mut Snap) {
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
    let mut m = Snap::new();
    walk(root, "", &mut m);
    m
}

fn name() -> impl Strategy<Value = String> {
    prop::sample::select(vec!["a", "b", "x.log", "y.txt", "build", "nm", ".h"])
        .prop_map(str::to_string)
}

fn rule() -> impl Strategy<Value = String> {
    prop::sample::select(vec![
        "*.log",
        "!x.log",
        "!*.log",
        "build/",
        "/build",
        "build",
        "**/nm/",
        "a/**",
        "a/b",
        "/a",
        "?",
        "*.txt",
        "!y.txt",
        "nm",
        "b/*",
        "!b/a",
        "**/b/x.log",
        "*",
        "!a",
        "# c",
        "",
    ])
    .prop_map(str::to_string)
}

fn build(root: &Path, items: &[(Vec<String>, Option<Vec<u8>>)]) {
    for (parts, bytes) in items {
        let fp = parts.iter().fold(root.to_path_buf(), |a, n| a.join(n));
        let mut ok = !fp.exists();
        let mut cur = root.to_path_buf();
        for n in &parts[..parts.len() - 1] {
            cur = cur.join(n);
            if cur.is_file() {
                ok = false;
            }
        }
        if !ok {
            continue;
        }
        match bytes {
            None => fs::create_dir_all(&fp).unwrap(),
            Some(b) => {
                fs::create_dir_all(fp.parent().unwrap()).unwrap();
                fs::write(&fp, b).unwrap();
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    #[test]
    fn record_then_fresh_checkout_is_exactly_the_non_ignored_files(
        items in prop::collection::vec(
            (prop::collection::vec(name(), 1..4),
             prop::option::of(prop::collection::vec(any::<u8>(), 0..16))),
            0..14),
        rules in prop::collection::vec(rule(), 0..6),
    ) {
        let t = tmp();
        let (a, b) = (t.join("a"), t.join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        hub(&a, &["init"]).unwrap();
        hub(&b, &["init"]).unwrap();
        let text = rules.join("\n") + "\n";
        fs::write(a.join(".hubignore"), &text).unwrap();
        build(&a, &items);
        let before = snapshot(&a);
        hub(&a, &["record", "-m", "v1"]).unwrap();

        let ig = Ignore::parse(&text);
        let expected: Snap = before
            .iter()
            .filter(|(p, v)| !ig.is_ignored(p, v.is_none()))
            .map(|(p, v)| (p.clone(), v.clone()))
            .collect();
        hub(&b, &["sync", a.to_str().unwrap()]).unwrap();
        hub(&b, &["checkout"]).unwrap();
        prop_assert_eq!(snapshot(&b), expected);

        // The original keeps every ignored file through a forced checkout.
        hub(&a, &["checkout", "--force"]).unwrap();
        prop_assert_eq!(snapshot(&a), before);
        let st = hub(&a, &["status"]).unwrap();
        prop_assert!(st.contains("working directory: clean"), "{}", st);
        let _ = fs::remove_dir_all(&t);
    }
}
