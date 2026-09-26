# hub-cli

`hub`: first runnable build of barocss-hub (Issue #16). Single local replica,
tree-level only (no file content, checkout, sync or refs). State lives in
`.hub/` (replica id, append-only `ops.log`, `checkpoints.log`).

## Quickstart

```sh
cargo install --path crates/hub-cli   # or: alias hub='cargo run -q -p hub-cli --'
mkdir demo && cd demo && hub init
hub mkdir src && hub touch src/main.rs && hub commit -m "first"
hub mv src/main.rs src/lib.rs && hub rm src/lib.rs && hub status
hub log && hub show "$(hub log | tail -1 | cut -d' ' -f1)"
```

`hub --help` lists every command.
