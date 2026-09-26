# hub-cli

`hub`: the runnable build of barocss-hub. It has a single local replica and
no sync or refs yet. Since Issue #19 it works on a real working directory:
the directory that contains `.hub/` (minus `.hub/` itself). Edit files with
any editor, then `hub record`.

## Quickstart

```sh
cargo install --path crates/hub-cli   # or: alias hub="$PWD/target/debug/hub"
mkdir demo && cd demo && hub init
mkdir src && echo 'fn main() {}' > src/main.rs && echo hello > README
hub record -m "first"
echo 'hello, world' > README && mv src/main.rs src/app.rs
hub status
hub record -m "second"
hub log
hub show -l "$(hub log | tail -1 | cut -d' ' -f1)"
rm -rf src README && hub checkout && cat README
```

`hub --help` lists every command.

## Working directory

- `hub record [-m <msg>]` scans the working directory against `.hub/index`
  and appends ops. A new path becomes Create (plus SetContent if the file is
  non-empty). A missing path becomes Delete. Changed bytes become SetContent.
  If exactly one missing file and one new file share a blob, that pair is a
  Move, which keeps the NodeId. `-m` also creates a checkpoint.
- `hub checkout [--force] [<short-id>]` writes a version (by default the head)
  and restores missing tracked files. Without `--force` it refuses to
  overwrite or delete a file with unrecorded changes, and refuses to overwrite
  an untracked file that is in the way. Untracked files that are not in the
  way are never touched.
- `hub status` shows unrecorded changes (added / modified / deleted /
  renamed), conflicts, and whether the working directory is at the head.
- `mkdir/touch/mv/rm` append the op and also update the working directory.
  They refuse, appending nothing, if that would clobber unrecorded changes.
- `record` and `mkdir/touch/mv/rm` require the working directory to be at the
  head. After `hub checkout <older-id>`, run `hub checkout` to return first.
- Names are byte-exact (D-003). On a case-insensitive filesystem, a checkout
  whose tree has names that differ only in case is refused, even with
  `--force`.
- Symlinks and special files are skipped with a warning. Only `.hub/` is
  ignored (there is no `.hubignore` yet).

## Storage (`.hub/`)

- `replica`, `ops.log`, `checkpoints.log`, `lock`: see Issue #16.
- `blobs/<sha256 hex>`: raw file bytes, written temp, fsync, rename, then the
  directory is fsynced. Every read verifies the hash.
- `index`: the last materialized version (`base` frontier), plus
  path -> node, blob, size and mtime for fast change detection.
