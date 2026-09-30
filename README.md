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

## Property-test seeds

`crates/hub-cli/tests/sync_prop.rs` runs with a pinned deterministic seed by
default, so CI is reproducible. To explore other seeds, set a u64:

    HUB_PROPTEST_SEED=12345 cargo test -p barocss-hub-cli --test sync_prop

Only `HUB_PROPTEST_SEED` is read. proptest's global `PROPTEST_RNG_SEED` is
ignored by this test, so exporting it elsewhere never disables the floor
below. Under an override the 20% interruption floor is reported
but not enforced.
