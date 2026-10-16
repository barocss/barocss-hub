# barocss-hub

CRDT-native source control (experimental).

Validate: `cargo test --workspace`

Try it: see [crates/hub-cli/README.md](crates/hub-cli/README.md) for the `barocss-hub` CLI quickstart.

License: MIT (see [LICENSE](LICENSE)).

## Names (D-033, Issue #35)

The binary is `barocss-hub` (`barocss-hub --help`, `barocss-hub --version`).
The crate directories are unchanged; only the package and binary names moved.
The Rust library names stay `hub_core` / `hub_cli`. There is no `hub` alias
binary.

| directory | old package | new package | binary (old -> new) |
|---|---|---|---|
| `crates/hub-core` | `hub-core` | `barocss-hub-core` | - |
| `crates/hub-cli` | `hub-cli` | `barocss-hub-cli` | `hub` -> `barocss-hub` |
| `crates/hub-bench` | `hub-bench` | `barocss-hub-bench` | `hub-bench` -> `barocss-hub-bench` |

Unchanged: the repository directory `.hub/`, `.hubignore`, the on-disk
format (`barocss-hub/format/v1`) and the `HUB_*` environment variables.
Repositories made before the rename open as before
(`crates/hub-cli/tests/pre_rename.rs`).

## Leaving a pinned checkout

`barocss-hub checkout <old-id>` pins the working directory to that version (the
watcher pauses). A plain `barocss-hub checkout` returns to head and unpins. If an
edit made while pinned conflicts with head, that checkout is refused. Until
branches exist, the way out is:

1. copy the edited file(s) aside,
2. `barocss-hub checkout --force` (returns to head and unpins; discards the edit),
3. copy the file(s) back,
4. `barocss-hub record`.

The refusal message prints the same steps.

## Text files: merge, `.hubattributes`, notices (Issue #56, D-053, D-056)

A file is **text** when it is valid UTF-8 without NUL bytes, at most 8 MiB, and
no line is longer than 64 KiB (a leading BOM counts as its own line). Each save
of a text file is recorded as a line-level `TextEdit` against the version the
working directory was checked out at, so concurrent edits of one file merge:
different lines both stay; the same line edited on two replicas keeps both new
lines and raises a `concurrent-edit` notice. Line endings (`\r\n`, mixed) and
the BOM are kept byte for byte. Binary files keep whole-file last-writer-wins
with a `content` conflict, as before; a save that flips a file between text and
binary is a whole-file save (`SetContent`) and conflicts with concurrent text
edits.

`checkout` (of head) and `watch` merge over unrecorded text edits (D-056): the
dirty file is first recorded as a `TextEdit` (durable before anything is
written), then the merged text is written with an atomic replace. If the head
version of a dirty file is binary, or the dirty content is not text, or any
other unrecorded change exists (a binary file, an add, a delete, a rename),
`checkout` refuses as before; `--force` discards.

`.hubattributes` (a tracked file at the working-directory root, `.hubignore`
pattern syntax, last match wins) overrides the classification:

```
*.min.js binary
docs/**/*.txt text
```

`text` still requires UTF-8 without NUL bytes, and lifts the size and line caps
up to 64 MiB; `binary` always records whole files.

Notices are informational and never block: `status` prints
`notice: concurrent edits merged in <path> lines a-b (by <replicas>)`,
`status --json` has `notices: [{kind, node, path, ops, lines, authors}]`, and
`watch --json` emits `{"event":"notice", ...}` when one appears. A notice
clears once someone edits the file after the merge.

Mixed remotes: a configured remote whose last sync showed no wire feature
`text` (cached in `.hub/remote-text`) keeps this repository authoring whole-file
saves, so that remote is never wedged with `upgrade-required`; `record` warns.

## Exit status (D-045, Issue #45)

| Status | Meaning |
|---|---|
| 0 | success |
| 1 | any command error (`--json`: one error document on stderr) |
| 2 | the current directory cannot be read |
| 3 | `sync` finished, but the remote rejected some of this repository's ops |

On status 3 the normal report is still printed (human: one `warning: the
remote rejected your op ...` line per op; `--json`: the full `sync` document
with a non-empty `rejected` array). Scripts should treat it as "synced,
action needed". `watch` and the VS Code extension never exit on it: they
emit a `rejected` event or notification and keep running, without backoff.
Full contract: Wiki page CLI-JSON.

Each rejection is reported once per remote (D-050, Issue #50): this
repository remembers, in its local `.hub/reported-rejections`, every
rejection it has shown (remote, op, payload hash, reason). A later sync
whose rejections are all already reported exits 0 with no warning (`--json`
still lists them in `rejected` with `"already_reported": true`); any new
one exits 3 again, and `watch` emits events only for new ones.
`duplicate-replica` is never suppressed, and the D-008 equivocation and
duplicate-replica warnings are unaffected. `barocss-hub warnings` lists the
remembered rejections; `barocss-hub warnings --clear-reported [<remote>]`
forgets them (all, or one remote's) so they are reported again.

## Troubleshooting: a copied repository (`reid --rescue`, D-049, Issue #49)

If you copied a repository folder (with its `.hub/`) and recorded in both,
sync stops with `duplicate-replica` or keeps reporting rejected ops
(`equivocation`). Run, in the copy:

    barocss-hub reid --rescue --dry-run   # shows the plan, changes nothing
    barocss-hub reid --rescue             # does it

Rescue gives the copy a fresh replica id and re-records its edits by
effect: the copy's rejected ops (from the saved warnings and the
reported-rejections memory, so it works offline) and everything built on
them are removed from the copy's log, and the working directory as you see
it (unrecorded edits included) is recorded on top under the new id. A file
both copies changed keeps the copy's content at head and the server's in
history. Every changed file is first backed up to
`.hub/rescue/<timestamp>/files/`. Rescue is crash-safe (rerun it to finish;
other commands refuse until then) and a rerun after success does nothing.
Checkpoints (`record -m`, `commit -m`) that named the removed ops are kept
in the rescue manifest and re-created as private checkpoints on the rescued
version (same message and author); published ones are listed so you can
re-publish them (nothing is published automatically). While a rescue is
unfinished the repository's replica header is marked, so older barocss-hub
binaries refuse it ("unsupported repository format") instead of touching
it. With a default remote it then runs one sync to confirm (`--offline`
skips it) and checks out the result if the working tree is unchanged since
the rescue recorded it. The working directory must be at head
(`barocss-hub checkout` first). Known limit: a third repository that built
on the copy's rejected ops keeps those ops; after the rescue it can show a
name collision (`g` / `g~…`) and stays diverged on the shared id (a
follow-up Issue tracks it).
In VS Code, the duplicate-replica error offers "Rescue…", which shows the
plan and asks before running it.

## Trusted remotes store (D-046, Issue #45)

First contact with a remote that came with a folder's `.hub/remotes`
(ssh or `file://`) needs confirmation; confirmations are kept per user, per
local repository:

- macOS: `~/Library/Application Support/barocss-hub/trusted-remotes`
  (`XDG_CONFIG_HOME` is not consulted). An existing pre-D-046
  `~/.config/barocss-hub/trusted-remotes` (or under `$XDG_CONFIG_HOME`) is
  still read and is moved into the new store on the next write.
- Linux and other Unixes: `$XDG_CONFIG_HOME/barocss-hub/trusted-remotes`
  when `XDG_CONFIG_HOME` is absolute, else `~/.config/barocss-hub/trusted-remotes`.
- `BAROCSS_HUB_CONFIG_DIR` (absolute) overrides the directory on every
  platform (used by the tests).

## Property-test seeds

`crates/hub-cli/tests/sync_prop.rs` runs with a pinned deterministic seed by
default, so CI is reproducible. To explore other seeds, set a u64:

    HUB_PROPTEST_SEED=12345 cargo test -p barocss-hub-cli --test sync_prop

Only `HUB_PROPTEST_SEED` is read. proptest's global `PROPTEST_RNG_SEED` is
ignored by this test, so exporting it elsewhere never disables the floor
below. Under an override the 20% interruption floor is reported
but not enforced.

## Test hooks (`HUB_TEST_*`, Issue #55)

The test suites drive the binary through `HUB_TEST_*` environment variables.
Two are honored only in debug/test builds; a release binary ignores them
(proved by the release branch of the tests that use them):

- `HUB_TEST_NO_REJECTION_DEDUP`, `HUB_TEST_RESCUE_EDIT_AFTER_SYNC`

These twelve are still read by release builds (known; moving all hooks
behind a test-only cargo feature is tracked separately):

- crash points: `HUB_TEST_CRASH_STAGE`, `HUB_TEST_CRASH_IN_SYNC`,
  `HUB_TEST_CRASH_IN_CHECKOUT`, `HUB_TEST_CRASH_IN_BLOB_PUT`
- logs: `HUB_TEST_FLUSH_LOG`, `HUB_TEST_SYNC_STATS`, `HUB_TEST_BLOB_BARRIER_LOG`
- limits and timing: `HUB_TEST_MAX_BLOB_SIZE`, `HUB_TEST_SESSION_MS`,
  `HUB_TEST_WIRE_TIMEOUT_MS`
- platform: `HUB_TEST_FORCE_CASE_INSENSITIVE`
- protocol: `HUB_TEST_NO_TEXT_FEATURE` (hello without the text feature, #54)

`HUB_TEST_FULL_PROPTEST` and `HUB_TEST_OLD_BIN` are read only by the tests.
`cargo test --workspace --release` is expected to pass.
