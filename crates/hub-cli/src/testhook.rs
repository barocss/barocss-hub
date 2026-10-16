//! Issue #57: every `HUB_TEST_*` environment hook is read through
//! [`hook_os!`] or [`hook_var!`]. Without the test-only cargo feature
//! `test-hooks` the read compiles to `None`, and the hook's name is never
//! referenced, so a shipped binary neither honors nor contains it.
//!
//! The test suites enable the feature (the crate's self dev-dependency in
//! `Cargo.toml`); `cargo build`/`cargo install`/dist builds do not. The
//! guard test `hook_reads_are_gated` fails if a `HUB_TEST_*` read appears
//! in `src/` outside these macros.

/// `std::env::var_os($name)` with `test-hooks`, else `None`.
macro_rules! hook_os {
    ($name:expr) => {{
        #[cfg(feature = "test-hooks")]
        let v: Option<std::ffi::OsString> = std::env::var_os($name);
        #[cfg(not(feature = "test-hooks"))]
        let v: Option<std::ffi::OsString> = None;
        v
    }};
}

/// `std::env::var($name).ok()` with `test-hooks`, else `None`.
macro_rules! hook_var {
    ($name:expr) => {{
        #[cfg(feature = "test-hooks")]
        let v: Option<String> = std::env::var($name).ok();
        #[cfg(not(feature = "test-hooks"))]
        let v: Option<String> = None;
        v
    }};
}

#[cfg(test)]
mod tests {
    /// Every `HUB_TEST_*` read in `src/` goes through the gated macros.
    /// A line that calls `env::var`/`env::var_os` must not name a hook (a
    /// `HUB_TEST_` literal or a constant whose value is one), except the
    /// test-harness-only reads listed here.
    #[test]
    fn hook_reads_are_gated() {
        const HARNESS: &[&str] = &["HUB_TEST_FULL_PROPTEST", "HUB_TEST_OLD_BIN"];
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        for e in std::fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            if p.extension().is_some_and(|x| x == "rs") {
                files.push((p.clone(), std::fs::read_to_string(&p).unwrap()));
            }
        }
        // Constants holding a hook name.
        let mut names: Vec<String> = Vec::new();
        for (_, s) in &files {
            for l in s.lines() {
                let t = l.trim();
                if t.starts_with("pub const ") && t.contains("= \"HUB_TEST_") {
                    let n = t["pub const ".len()..].split(':').next().unwrap();
                    names.push(n.trim().to_string());
                }
            }
        }
        assert!(names.len() >= 7, "hook constants not found: {names:?}");
        let mut bad = Vec::new();
        for (p, s) in &files {
            if p.ends_with("testhook.rs") {
                continue;
            }
            for (i, l) in s.lines().enumerate() {
                if !(l.contains("env::var(") || l.contains("env::var_os(")) {
                    continue;
                }
                if HARNESS.iter().any(|h| l.contains(h)) {
                    continue;
                }
                let names_hook = l.contains("HUB_TEST_")
                    || names.iter().any(|n| {
                        l.split(|c: char| !(c.is_alphanumeric() || c == '_'))
                            .any(|w| w == n)
                    });
                if names_hook {
                    bad.push(format!("{}:{}: {}", p.display(), i + 1, l.trim()));
                }
            }
        }
        assert!(
            bad.is_empty(),
            "ungated HUB_TEST_* reads:\n{}",
            bad.join("\n")
        );
    }
}
