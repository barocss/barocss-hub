# barocss-hub-cli (crates/hub-cli)

`barocss-hub`: the runnable build of barocss-hub. Each repository is one replica;
since Issue #22 `barocss-hub sync <path>` syncs two repositories on the same machine
(no network transport or refs yet). Since Issue #19 it works on a real working directory:
the directory that contains `.hub/` (minus `.hub/` itself). Edit files with
any editor, then `barocss-hub record`.

## Quickstart

```sh
cargo install --path crates/hub-cli   # or: alias barocss-hub="$PWD/target/debug/barocss-hub"
mkdir demo && cd demo && barocss-hub init
mkdir src && echo 'fn main() {}' > src/main.rs && echo hello > README
barocss-hub record -m "first"
echo 'hello, world' > README && mv src/main.rs src/app.rs
barocss-hub status
barocss-hub record -m "second"
barocss-hub log
barocss-hub show -l "$(barocss-hub log | tail -1 | cut -d' ' -f1)"
rm -rf src README && barocss-hub checkout && cat README
```

`barocss-hub --help` lists every command.

## Two-folder sync

```sh
mkdir a b
(cd a && barocss-hub init && echo hello > greet.txt && barocss-hub record -m "first" && barocss-hub publish first)
(cd b && barocss-hub init && barocss-hub sync ../a && barocss-hub status && barocss-hub checkout && cat greet.txt)
(cd b && echo "from b" > b.txt && barocss-hub record -m "b work" && barocss-hub publish "b work")
(cd a && echo edit >> greet.txt && barocss-hub record -m "local only" && barocss-hub sync ../b && barocss-hub checkout && barocss-hub log)
(cd b && barocss-hub sync ../a && barocss-hub checkout && barocss-hub log)
```

`barocss-hub log` in `a` shows `[published]` on `first` and `b work`. `local only`
was never published, so `b` never sees it (D-006). Both folders end with the
same head id and identical files.

## Sync

- `barocss-hub sync <path>` pulls both ways (§4.2 `Have`/`Ops`/`Need`), copies the
  blobs the new ops name (SHA-256 verified), then exchanges published
  checkpoints. Afterwards both op logs hold the union.
- Sync is log-only. It never touches the working directory or the index, so
  it works even with unrecorded changes. `barocss-hub status` then says the working
  directory is behind head, and `barocss-hub checkout` brings it up to date (with
  the usual refusals for files with unrecorded changes).
- Locks: both `.hub/lock` files are taken exclusively, in canonical-path
  order, so `a$ barocss-hub sync ../b` and `b$ barocss-hub sync ../a` at the same time
  cannot deadlock.
- Crash safety: blobs are written first, then new ops are appended in `<`
  order (every prefix is causally closed). An interrupted sync leaves both
  repositories valid; rerun it to finish.
- `barocss-hub publish <short-id|message>` marks a local checkpoint published.
  Only published checkpoints are sent; a received checkpoint is published
  (grow-only set, §4.6). `barocss-hub log` marks them `[published]`.
- Ops with missing deps are kept in `.hub/pending.log` and retried on the
  next sync. D-008 warnings (lamport-violation, equivocation, corrupt-blob,
  duplicate-replica) are kept in `.hub/warnings.log`. `barocss-hub status` and
  `barocss-hub warnings` show them with the peer, the op, the reason and the stuck
  descendants.
- Two repositories with the same replica id (a copied `.hub/`) cannot sync:
  the sync aborts (D-022). Run `barocss-hub reid` in one copy to give it a fresh id
  (new ops use it; old ops keep theirs), then sync. Do this before
  recording in the copy: ops already recorded under the shared id still
  collide.
- A successful `barocss-hub reid` also clears this repository's saved
  `duplicate-replica` warnings (provisional default, #23; the other copy
  keeps its own until it is synced or reid'd).
- `barocss-hub init` draws the replica id from the OS CSPRNG. Existing repositories
  keep theirs.

## Working directory

- `barocss-hub record [-m <msg>]` scans the working directory against `.hub/index`
  and appends ops. A new path becomes Create (plus SetContent if the file is
  non-empty). A missing path becomes Delete. Changed bytes become SetContent.
  If exactly one missing file and one new file share a blob, that pair is a
  Move, which keeps the NodeId. `-m` also creates a checkpoint.
- `barocss-hub checkout [--force] [<short-id>]` writes a version (by default the head)
  and restores missing tracked files. Without `--force` it refuses to
  overwrite or delete a file with unrecorded changes, and refuses to overwrite
  an untracked file that is in the way. Untracked files that are not in the
  way are never touched.
- `barocss-hub status` shows unrecorded changes (added / modified / deleted /
  renamed), conflicts, and whether the working directory is at the head.
- `mkdir/touch/mv/rm` append the op and also update the working directory.
  They refuse, appending nothing, if that would clobber unrecorded changes.
- `record` and `mkdir/touch/mv/rm` require the working directory to be at the
  head. After `barocss-hub checkout <older-id>`, run `barocss-hub checkout` to return first.
- Names are byte-exact (D-003). On a case-insensitive filesystem, paths
  whose names differ only in case cannot all exist on disk (D-019): checkout
  skips just those clashing paths with a warning, records them in the index
  as *unmaterialized* (`u` entries), and writes everything else. `barocss-hub status`
  lists them under `unmaterialized (case clash ...)`. They are not reported
  as deleted and `barocss-hub record` ignores them.
- A working file or directory that sits on (or case-folds onto, or lies
  under) an unmaterialized path is *blocked*. `barocss-hub status` prints one line
  per path, `blocked:  <path> collides with unmaterialized <node paths>`,
  under `blocked (record refuses until resolved):`. `barocss-hub record` refuses
  until you rename or remove those files, or rename one of the clashing
  nodes (for example in a case-sensitive checkout). Checkout never
  overwrites a blocked file, and `mkdir/touch/mv/rm` refuse (`refusing to
  mv: ...`) rather than overwrite it.
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
  it; its edits and deletion are still recorded, and `barocss-hub status` lists it
  under "tracked but ignored". Use `barocss-hub rm` to stop tracking it.
  [decision-proposal]
- `barocss-hub status` hides ignored files; `barocss-hub status --ignored` lists them (the
  top-most ignored path of each ignored subtree).
- `barocss-hub checkout` still writes tracked files that match a rule. It never
  deletes or overwrites an ignored untracked file, **even with `--force`**:
  if one is in the way, it refuses and names it; move it away and retry.
- Sync is unaffected: rules only apply when scanning the working directory.

### Built-in default ignores (Issue #37)

Editor and OS temp files are ignored even without a `.hubignore`. The list
(`hub_cli::ignore::DEFAULTS`) is read as if it were the first lines of
`.hubignore`, so a `!pattern` line in the file re-includes any of them:

| pattern | what |
|---|---|
| `*.swp` `*.swo` `*.swn` `*.swx` | vim swap files |
| `*___jb_tmp___` `*___jb_old___` | JetBrains "safe write" temp files |
| `.~lock.*#` | LibreOffice / OpenOffice lock files |
| `.goutputstream-*` | GNOME (GIO) atomic-replace temp files |
| `.DS_Store` `Thumbs.db` `desktop.ini` | macOS / Windows folder metadata |

Names that editors use but that could also be real files are ignored
**only while they look like the editor's file**; otherwise they are
recorded like any file (never silently dropped):

| pattern | ignored only when |
|---|---|
| `4913` | it is an empty regular file (vim's write probe) |
| `*~` | the file without the `~` exists next to it (`x~` beside `x`: a backup) |
| `#*#` | the file without the `#`s exists next to it (`#x#` beside `x`: emacs auto-save) |
| `.#*` | it is a symlink, or a regular file of at most 256 bytes on one line containing `@` (emacs lock `user@host.pid:boot`) |

Directories never match these four. `record`, `status` and `watch` use the
same rules. Plain `status` prints `N file(s) hidden by default ignore rules
(see `barocss-hub status --ignored`)` when defaults hide something
(`--json`: `hidden_by_defaults`, and `"default": true` on each `ignored`
entry).

As with any rule, a file that is already tracked stays tracked. Saves by
vim (`backupcopy=no`), VS Code-style atomic writes (temp + rename) and
JetBrains safe write are recorded as a content edit of the same node
(`crates/hub-cli/tests/editor.rs`). While a tracked file is missing and its
own `x~`, `x___jb_old___` or `x___jb_tmp___` exists, `barocss-hub watch`
waits for the save to finish (at most 5 s, `HUB_WATCH_SAVE_WAIT_MS`)
instead of recording a delete. (After a real delete, a leftover `x~` no
longer has its sibling, so it is recorded.)

## Remotes and SSH (Issue #43, Wiki Remote-Sync)

`sync` speaks wire protocol v1 to a remote; a same-machine folder is just a
`file://` remote (the `peer` command is gone, D-041; an existing
`.hub/peers` is converted to `file://` remotes by the first writing
command).

```sh
# on the server: a bare repository (no working directory, D-042)
barocss-hub init --bare /srv/hub/proj
# on each laptop
barocss-hub remote add origin ssh://alice@srv/srv/hub/proj   # or srv:proj (home-relative)
barocss-hub remote add --exec /home/alice/.cargo/bin/barocss-hub origin2 srv:/srv/hub/proj
barocss-hub remote list
barocss-hub sync            # the default remote; or `sync origin`, a URL, or a path
barocss-hub checkout        # sync is log-only
```

- The client runs the system `ssh` (keys, host keys, agents, ProxyJump and
  `~/.ssh/config` are ssh's business; barocss-hub never asks for a
  password). `BAROCSS_HUB_SSH` replaces the ssh program and fixed args.
  The remote end is `barocss-hub serve --stdio <path>`, which serves only
  that repository.
- Restricting a key on the server (`~/.ssh/authorized_keys`):
  `restrict,command="barocss-hub serve --stdio --root /srv/hub --from-ssh-original-command" ssh-ed25519 ...`
  Add `--read-only` for keys that may only pull: such a client pulls,
  prints `remote is read-only; pulled only` and exits 0.
- `watch` syncs with every remote (D-044). `sync --json` reports
  `received`, `sent`, `read_only` and `head`.
- Not verified here against a real sshd (none on the development
  machine); the tests use `HUB_SSH_COMMAND`, a stand-in `ssh` that runs the
  remote command locally.

## Live sync between two folders (`barocss-hub watch`)

`barocss-hub watch` records working-directory edits automatically, syncs with every
configured remote and applies what arrives. Run it in the foreground in each
folder; Ctrl-C stops it cleanly (exit 0).

```sh
mkdir a b
(cd a && barocss-hub init) && (cd b && barocss-hub init)
(cd a && barocss-hub remote add b ../b)  # .hub/remotes: local config, not replicated
(cd b && barocss-hub remote add a ../a)
(cd a && barocss-hub watch)               # terminal 1
(cd b && barocss-hub watch)               # terminal 2
echo hello > a/notes.txt          # ~1 s later: b/notes.txt says hello
echo "from b" >> b/notes.txt      # ~1 s later: a/notes.txt has both lines
(cd a && barocss-hub status)              # "remotes:", "watch: active (pid N)", clean
```

Watcher output (terminal 1) for the above, as actually run:

```text
watching /.../a (1 remote(s)); Ctrl-C to stop
recorded 1 change(s):
  added:    notes.txt
synced with b: received 0 op(s), 0 blob(s), 0 checkpoint(s); sent 2 op(s), 1 blob(s), 0 checkpoint(s)
checked out f1137570 (1 written, 0 removed)
watch stopped
```

- Change detection polls the tree (`--poll`, default 100 ms); an edit is
  recorded after the tree has been quiet for `--debounce` (default 400 ms),
  so a burst of saves becomes one record. Every remote is also synced
  every `--sync` ms (default 2000); a failing remote backs off (up to 32
  intervals) and is reported as an `error` event with `peer` = its name.
- Auto-record is `barocss-hub record`: `.hubignore`, blocked paths and an
  interrupted checkout are honoured; a refusal is printed and retried.
  It never checkpoints; use `barocss-hub commit`. [decision-proposal]
- Unrecorded edits are never clobbered: before incoming ops are applied the
  local edits are recorded (depending on the version you last had on disk),
  so a same-file edit on both sides becomes a content conflict in
  `barocss-hub status` on both sides, with the same bytes in both folders.
- The watcher's own writes are not re-recorded (the tree snapshot is taken
  again after each step), and the op count stays constant when idle.
- `barocss-hub checkout <id>` of an older version **pins** the working directory
  (`.hub/pinned`): the watcher pauses auto-record and auto-checkout (it
  keeps syncing the log) and says "paused: ..."; `barocss-hub status` shows
  "pinned at <id>". Plain `barocss-hub checkout` (head) unpins and watching resumes.
  `barocss-hub checkout <id>` naming head itself does not pin. [decision-proposal]
- After a crash or `kill -9`, just start `barocss-hub watch` (or any command) again:
  an interrupted checkout is resumed, a torn log tail is dropped, temps are
  swept. A second `barocss-hub watch` on the same repo refuses while one runs.

## Machine-readable output (`--json`, Issue #37)

`--json` anywhere on the command line switches stdout to exactly one JSON
document (schema 1), for tools such as editor extensions. The schema is
documented in the Wiki page `CLI-JSON`; golden documents are in
`crates/hub-cli/tests/golden/json/`.

```sh
barocss-hub status --json            # head, workdir state (at-head/behind/pinned), watch pid,
                                     # conflicts (with blob ids), changes, blocked, ignored
barocss-hub log --json --path a.txt  # checkpoints containing the file + every op on its node
barocss-hub cat <version> a.txt      # raw bytes (--json: base64 in a document)
barocss-hub cat-blob <64-hex>        # raw bytes of a stored blob
barocss-hub watch --json             # JSON Lines events: started, recorded, synced,
                                     # checked-out, deferred, paused, resumed, warning, error, stopped
```

- Warnings are the document's `"warnings"` array; nothing else is printed.
- On an error stdout is empty, stderr holds
  `{"schema":1,"program":"barocss-hub","error":{"code":...,"message":...},"warnings":[...]}`
  and the exit status is 1 (2: the current directory cannot be read).
  `code` is set where the error is raised (`hub_cli::Code`): `usage`,
  `not-a-repository`, `already-exists`, `invalid-path`, `no-such-path`,
  `not-a-file`, `not-a-directory`, `bad-version`, `not-found`, `ambiguous`,
  `no-such-blob`, `bad-blob-id`, `locked`, `behind`, `pinned`,
  `interrupted`, `blocked`, `refused`, `duplicate-replica`, `corrupt`, `io`,
  `error`.
- Human output is unchanged; human errors are prefixed `barocss-hub: `.

## Storage (`.hub/`)

- On-disk format **v1** (Issue #33): `replica` starts with
  `barocss-hub/replica/v1` (the version marker; older binaries refuse the
  repo before touching it), `format` repeats it. A v0 repository is migrated
  automatically, crash-safely, on its first open by this binary.
- `replica`, `ops.log`, `checkpoints.log`, `lock`: see Issue #16. In v1,
  `ops.log` groups op records into batches, each ended by a
  `B <seq> <count> <chain>` commit record; only committed batches count, and
  an uncommitted tail (crash or power loss) is dropped on open.
- `snapshot`: a validated cache of the replayed state bound to a log
  position, so open replays only the ops after it. Deleting it changes
  nothing observable. `HUB_NO_SNAPSHOT=1` disables it, `HUB_SNAPSHOT_MIN=<n>`
  tunes when it is rewritten (default: every max(4096, ops/16) new ops).
- `published.log`, `pending.log`, `warnings.log`: Issue #22 (optional,
  same record framing as `ops.log`).
- `blobs/<sha256 hex>`: raw file bytes, written temp, fsync, rename, then the
  directory is fsynced. Every read verifies the hash.
- `index`: the last materialized version (`base` frontier), plus
  path -> node, blob, size and mtime for fast change detection. `u` lines are
  unmaterialized paths (D-019).
- `peers`, `pinned`, `watch.lock`, `watch.pid`: Issue #25, local only (never
  synced). A watch is active only while `watch.lock` is `flock`ed, so a pid
  file left by a crash is ignored.
- `tmp/`, `checkout-in-progress`: in-flight working-file temps and the
  interrupted-checkout marker (rerun `barocss-hub checkout` to finish).
- Blob ids are canonical lowercase hex everywhere; an uppercase blob id in a
  log is rejected as corrupt.
- Locking uses `flock` on unix. Other targets have no repository lock and
  every command prints a warning saying so.
- The full layout is specified in the Wiki, Repository-Semantics,
  Appendix A ("On-disk format v0", A.1 durability, A.2 "On-disk format v1").
