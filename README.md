# barocss-hub

CRDT-native source control (experimental).

Validate: `cargo test --workspace`

Try it: see [crates/hub-cli/README.md](crates/hub-cli/README.md) for the `hub` CLI quickstart.

## Leaving a pinned checkout

`hub checkout <old-id>` pins the working directory to that version (the
watcher pauses). A plain `hub checkout` returns to head and unpins. If an
edit made while pinned conflicts with head, that checkout is refused. Until
branches exist, the way out is:

1. copy the edited file(s) aside,
2. `hub checkout --force` (returns to head and unpins; discards the edit),
3. copy the file(s) back,
4. `hub record`.

The refusal message prints the same steps.

## Property-test seeds

`crates/hub-cli/tests/sync_prop.rs` runs with a pinned deterministic seed by
default, so CI is reproducible. To explore other seeds, set a u64:

    HUB_PROPTEST_SEED=12345 cargo test -p hub-cli --test sync_prop

Only `HUB_PROPTEST_SEED` is read. proptest's global `PROPTEST_RNG_SEED` is
ignored by this test, so exporting it elsewhere never disables the floor
below. Under an override the 20% interruption floor is reported
but not enforced.
