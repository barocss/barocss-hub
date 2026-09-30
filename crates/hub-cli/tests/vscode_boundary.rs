//! Architecture boundary for the VS Code extension (#38, D-034): the TypeScript
//! integration consumes the core only through `barocss-hub --json`. This mirrors
//! `integrations/vscode/scripts/check-boundary.js` (run by the extension's lint) so
//! `cargo test` fails too if the extension source names the `.hub` directory or
//! reads files directly, and runs the full script when `node` is available.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn ext_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../integrations/vscode")
}

fn ts_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            ts_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "ts") {
            out.push(p);
        }
    }
}

/// `.hub` as a path segment (not `.hubignore`, not `barocss-hub`).
fn names_hub_dir(line: &str) -> bool {
    let b = line.as_bytes();
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'-';
    line.match_indices(".hub").any(|(i, _)| {
        let before = i == 0 || !word(b[i - 1]);
        let after = b.get(i + 4).is_none_or(|&c| !word(c));
        before && after
    })
}

#[test]
fn detector_catches_hub_paths() {
    assert!(names_hub_dir("fs.readFileSync(root + '/.hub/ops.log')"));
    assert!(names_hub_dir("'.hub'"));
    assert!(!names_hub_dir("'.hubignore'"));
    assert!(!names_hub_dir("barocss-hub status"));
}

#[test]
fn extension_source_never_reads_repository_internals() {
    let mut files = Vec::new();
    ts_files(&ext_dir().join("src"), &mut files);
    assert!(!files.is_empty(), "no extension sources found");
    let reads = [
        "readFile",
        "readdir",
        "createReadStream",
        "statSync",
        "existsSync",
        "createFileSystemWatcher",
    ];
    let mut bad = Vec::new();
    for f in &files {
        let test = f.components().any(|c| c.as_os_str() == "test");
        for (i, line) in fs::read_to_string(f).unwrap().lines().enumerate() {
            if test && line.contains("// boundary-allow: ") {
                continue;
            }
            let code = line.split("//").next().unwrap_or("");
            if names_hub_dir(code) || reads.iter().any(|r| code.contains(r)) {
                bad.push(format!("{}:{}: {}", f.display(), i + 1, line.trim()));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "extension boundary violations:\n{}",
        bad.join("\n")
    );
}

#[test]
fn boundary_script_passes_when_node_is_available() {
    let Ok(out) = Command::new("node")
        .arg("scripts/check-boundary.js")
        .arg("--self-test")
        .current_dir(ext_dir())
        .output()
    else {
        eprintln!("node not found; skipped the JS boundary script (the Rust scan above still ran)");
        return;
    };
    assert!(
        out.status.success(),
        "check-boundary.js failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
