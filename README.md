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
