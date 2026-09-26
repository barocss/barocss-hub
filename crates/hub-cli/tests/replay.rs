//! Issue #16: replaying the on-disk log equals in-memory evaluation.

use std::path::PathBuf;

use hub_cli::Repo;
use hub_core::tree::*;
use proptest::prelude::*;

#[path = "../../hub-core/tests/common/mod.rs"]
mod common;
use common::*;

fn tmp(i: u64) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hub-cli-replay-{}-{i}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn replayed_equals_in_memory((nrep, script, seeds) in script_strategy()) {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let ops = simulate(nrep, &script);
        let spec = evaluate(&ops);
        let root = tmp(N.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        {
            let mut repo = Repo::init(&root).unwrap();
            // Append in a shuffled causal-or-not order (Replica buffers).
            for op in delivery(&ops, seeds[0]) {
                if !repo.store.contains_key(&op.id) {
                    repo.append_op(op).unwrap();
                }
            }
            prop_assert_eq!(repo.state(), &spec.state);
        }
        let re = Repo::discover(&root).unwrap();
        prop_assert!(re.warnings.is_empty());
        prop_assert_eq!(re.replica.pending_len(), 0);
        prop_assert_eq!(re.state(), &spec.state);
        let ev = evaluate(re.store.values());
        prop_assert_eq!(&ev.conflicts, &spec.conflicts);
        prop_assert_eq!(re.store.len(), ops.len());
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[test]
fn string_escaping_round_trips() {
    for s in ["", "a b", "tab\tnl\n", "100%", "한글 ~x", "s", "-"] {
        assert_eq!(hub_cli::store::unesc(&hub_cli::store::esc(s)).unwrap(), s);
    }
}
