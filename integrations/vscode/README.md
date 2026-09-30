# barocss-hub for VS Code

VS Code integration for [barocss-hub](https://github.com/barocss/barocss-hub) (D-034).
It is written in TypeScript and talks to the core **only** by spawning
`barocss-hub ... --json` (and `barocss-hub watch --json` for live events, see the
Wiki page *CLI-JSON*, schema 1). It never reads `.hub/` itself; a boundary check
(`npm run lint`, and `cargo test -p barocss-hub-cli --test vscode_boundary`) enforces this.

Not on the Marketplace (the publisher account is a pending decision): install the `.vsix`.

## Install from the .vsix

1. Build the CLI and put it somewhere stable:
   `cargo build --release -p barocss-hub-cli` (binary: `target/release/barocss-hub`).
2. Build the extension package (or use a `.vsix` you were given):
   ```sh
   cd integrations/vscode
   npm install
   npm run compile
   npx @vscode/vsce package --no-dependencies   # -> barocss-hub-vscode-0.0.1.vsix
   ```
3. Install it: `code --install-extension barocss-hub-vscode-0.0.1.vsix`
   (or *Extensions* view, `...` menu, *Install from VSIX...*).
4. Set `barocssHub.binaryPath` to the absolute path of `barocss-hub` (VS Code
   started from the Dock does not see your shell `PATH`).
5. Open a folder that is a barocss-hub repository (`barocss-hub init` was run in it
   or above it).

Uninstall: `code --uninstall-extension barocss.barocss-hub-vscode`.

## Features

- **Status bar**: `at head <id>`, `behind N` (ops from a sync not yet checked out),
  `pinned <id>`, `interrupted checkout`, `N conflicts`, plus `watching` while the
  watcher runs. From `status --json`; refreshed on save, on every watch event,
  on window focus and every `barocssHub.pollInterval` ms. Click it for the actions
  quick pick (checkout latest, start/stop watch, record, publish, show conflicts,
  refresh, log, settings).
- **Conflicts view** (Explorer: *barocss-hub Conflicts*): each conflict with its
  kind and path. For a content conflict, *Compare with Losing Version* opens a
  diff of the losing and winning blobs (read-only `barocss-hub-blob:` documents
  fetched with `cat-blob`).
- **File History view** (Explorer: *barocss-hub File History*): the checkpoints
  containing the active file (from `log --path`), newest first, numbered, with
  message, short id and author; plus every content edit op. Ops have no
  timestamps, so there is no relative time. Click one to open that version
  read-only.
- **Commands** (`barocss-hub:` in the palette): Start Watch / Stop Watch (a
  `watch --json` child per folder, stopped with SIGTERM on Stop and when VS Code
  closes), Record Checkpoint... (`record -m`), Checkout Latest, Publish
  Checkpoint..., Refresh, Actions...
- **Unsaved-edit guard**: when the watcher checks out an incoming change to a
  file that has unsaved edits, a notification offers *Compare* (incoming vs
  yours), *Keep mine* (write your buffer; the watcher records it as a concurrent
  edit) or *Reload* (discard your edits).
- **Errors**: the CLI's typed codes (`interrupted`, `pinned`, `behind`,
  `blocked`, `refused`, `duplicate-replica`, `corrupt`, `not-a-repository`,
  `locked`, ...) are shown as plain messages with a suggested action (Checkout,
  Force Checkout, Copy reid command, Stop Watch, Open Settings, Show Log).
- **Multi-root workspaces**: every workspace folder that is (inside) a
  barocss-hub repository gets its own status, conflicts and watcher. The status
  bar shows the repository of the active editor (folder name appended when
  there are several); commands ask which repository when there is more than one.
  Two workspace folders inside the *same* repository are treated as two
  controllers of one repository: start the watcher in only one of them (the
  second gets `locked`).

## Settings

| setting | default | meaning |
|---|---|---|
| `barocssHub.binaryPath` | `barocss-hub` | the CLI binary (use an absolute path) |
| `barocssHub.autoStartWatch` | `false` | start `watch` for every repository folder on startup |
| `barocssHub.watchArgs` | `[]` | extra `watch` arguments, e.g. `["--sync", "2000"]` |
| `barocssHub.pollInterval` | `15000` | ms between background status refreshes (0: off) |

## Development

```sh
npm install
npm run compile
npm run lint        # tsc --noEmit + the boundary check
npm test            # headless @vscode/test-electron run against target/release/barocss-hub
```

`npm test` env: `BAROCSS_HUB_BIN` (default `../../target/release/barocss-hub`),
`VSCODE_EXECUTABLE` (an existing VS Code binary, e.g.
`/Applications/Visual Studio Code.app/Contents/MacOS/Electron`; otherwise a copy
is downloaded), `VSCODE_TEST_ROOT` (where the isolated `--user-data-dir`,
`--extensions-dir`, the download cache and the temp repositories go; default
`.vscode-test/`). Your own VS Code profile is never used.

Known limits: VS Code's Timeline API is still a proposed API (unusable by an
installed extension), so history is a tree view instead; the unsaved-edit guard
covers text documents only.
