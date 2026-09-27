# hub-cli

`hub`: the runnable build of barocss-hub. Each repository is one replica;
since Issue #22 `hub sync <path>` syncs two repositories on the same machine
(no network transport or refs yet). Since Issue #19 it works on a real working directory:
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

## Two-folder sync

```sh
mkdir a b
(cd a && hub init && echo hello > greet.txt && hub record -m "first" && hub publish first)
(cd b && hub init && hub sync ../a && hub status && hub checkout && cat greet.txt)
(cd b && echo "from b" > b.txt && hub record -m "b work" && hub publish "b work")
(cd a && echo edit >> greet.txt && hub record -m "local only" && hub sync ../b && hub checkout && hub log)
(cd b && hub sync ../a && hub checkout && hub log)
```

`hub log` in `a` shows `[published]` on `first` and `b work`. `local only`
was never published, so `b` never sees it (D-006). Both folders end with the
same head id and identical files.

## Sync

- `hub sync <path>` pulls both ways (§4.2 `Have`/`Ops`/`Need`), copies the
  blobs the new ops name (SHA-256 verified), then exchanges published
  checkpoints. Afterwards both op logs hold the union.
- Sync is log-only. It never touches the working directory or the index, so
  it works even with unrecorded changes. `hub status` then says the working
  directory is behind head, and `hub checkout` brings it up to date (with
  the usual refusals for files with unrecorded changes).
- Locks: both `.hub/lock` files are taken exclusively, in canonical-path
  order, so `a$ hub sync ../b` and `b$ hub sync ../a` at the same time
  cannot deadlock.
- Crash safety: blobs are written first, then new ops are appended in `<`
  order (every prefix is causally closed). An interrupted sync leaves both
  repositories valid; rerun it to finish.
- `hub publish <short-id|message>` marks a local checkpoint published.
  Only published checkpoints are sent; a received checkpoint is published
  (grow-only set, §4.6). `hub log` marks them `[published]`.
- Ops with missing deps are kept in `.hub/pending.log` and retried on the
  next sync. D-008 warnings (lamport-violation, equivocation, corrupt-blob,
  duplicate-replica) are kept in `.hub/warnings.log`. `hub status` and
  `hub warnings` show them with the peer, the op, the reason and the stuck
  descendants.
- Two repositories with the same replica id (a copied `.hub/`) cannot sync:
  the sync aborts (D-022). Run `hub reid` in one copy to give it a fresh id
  (new ops use it; old ops keep theirs), then sync. Do this before
  recording in the copy: ops already recorded under the shared id still
  collide.
- `hub init` draws the replica id from the OS CSPRNG. Existing repositories
  keep theirs.

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
- Symlinks and special files are skipped with a warning.

## Ignore rules (`.hubignore`)

A `.hubignore` file at the working-directory root lists paths that `record`
never adds. `.hub/` is always ignored. Example:

```text
# build output
target/
*.log
!keep.log
**/node_modules/
```

Supported (a subset of gitignore): blank lines and `#` comments; `!` to
re-include (the last matching line wins); a trailing `/` matches directories
only; a pattern with a `/` at the start or in the middle is anchored to the
root, otherwise it matches the name at any depth; `*` and `?` (never across
`/`); `**` as a whole segment (`**/x`, `a/**/b`, `a/**`). As in git, nothing
inside an ignored directory can be re-included. Not supported: `[...]`
classes, backslash escapes other than a leading `\#` or `\!`, nested
`.hubignore` files, global or per-repo exclude files.

- `.hubignore` is an ordinary tracked file: record it and it syncs, so every
  replica ignores the same paths. [decision-proposal]
- A file that is already tracked stays tracked when a rule starts matching
  it; its edits and deletion are still recorded, and `hub status` lists it
  under "tracked but ignored". Use `hub rm` to stop tracking it.
  [decision-proposal]
- `hub status` hides ignored files; `hub status --ignored` lists them (the
  top-most ignored path of each ignored subtree).
- `hub checkout` still writes tracked files that match a rule. It never
  deletes or overwrites an ignored untracked file, **even with `--force`**:
  if one is in the way, it refuses and names it; move it away and retry.
- Sync is unaffected: rules only apply when scanning the working directory.

## Storage (`.hub/`)

- `replica`, `ops.log`, `checkpoints.log`, `lock`: see Issue #16.
- `published.log`, `pending.log`, `warnings.log`: Issue #22 (optional,
  same record framing as `ops.log`).
- `blobs/<sha256 hex>`: raw file bytes, written temp, fsync, rename, then the
  directory is fsynced. Every read verifies the hash.
- `index`: the last materialized version (`base` frontier), plus
  path -> node, blob, size and mtime for fast change detection.
