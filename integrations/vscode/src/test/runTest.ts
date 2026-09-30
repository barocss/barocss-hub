// Headless integration run: a real release barocss-hub binary, temp repos, an isolated
// VS Code profile (--user-data-dir / --extensions-dir under VSCODE_TEST_ROOT).
import * as path from 'path';
import * as fs from 'fs';
import { spawnSync } from 'child_process';
import { runTests, downloadAndUnzipVSCode } from '@vscode/test-electron';

async function main() {
  const ext = path.resolve(__dirname, '../..');
  const bin = process.env.BAROCSS_HUB_BIN ?? path.resolve(ext, '../../target/release/barocss-hub');
  if (!fs.existsSync(bin)) { throw new Error(`no barocss-hub binary at ${bin} (cargo build --release -p barocss-hub-cli)`); } // boundary-allow: checks the CLI binary, not a repository
  const root = process.env.VSCODE_TEST_ROOT ?? path.join(ext, '.vscode-test');
  const run = path.join(root, `run-${Date.now()}`);
  const ws = path.join(run, 'a');
  const peer = path.join(run, 'b');
  const userData = path.join(root, 'user-data');
  const extDir = path.join(root, 'extensions');
  for (const d of [ws, peer, path.join(userData, 'User'), extDir]) { fs.mkdirSync(d, { recursive: true }); }
  for (const d of [ws, peer]) {
    const r = spawnSync(bin, ['init'], { cwd: d });
    if (r.status !== 0) { throw new Error(`init failed in ${d}: ${r.stderr}`); }
  }
  fs.writeFileSync(path.join(userData, 'User', 'settings.json'), JSON.stringify({
    'barocssHub.binaryPath': bin,
    'barocssHub.autoStartWatch': false,
    'barocssHub.watchArgs': ['--poll', '100', '--debounce', '100', '--sync', '300'],
    'barocssHub.pollInterval': 0,
    'security.workspace.trust.enabled': false,
    'update.mode': 'none',
    'telemetry.telemetryLevel': 'off',
    'extensions.autoUpdate': false,
  }, null, 2));
  const local = process.env.VSCODE_EXECUTABLE;
  const vscodeExecutablePath = local && fs.existsSync(local) ? local // boundary-allow: locates the VS Code binary
    : await downloadAndUnzipVSCode({ cachePath: path.join(root, 'cache'), version: process.env.VSCODE_VERSION ?? 'stable' });
  console.log(`VS Code: ${vscodeExecutablePath}\nworkspace: ${ws}\npeer: ${peer}`);
  const code = await runTests({
    vscodeExecutablePath,
    extensionDevelopmentPath: ext,
    extensionTestsPath: path.resolve(__dirname, './suite/index'),
    extensionTestsEnv: { BAROCSS_HUB_BIN: bin, HUB_TEST_WS: ws, HUB_TEST_PEER: peer },
    launchArgs: [ws, '--user-data-dir', userData, '--extensions-dir', extDir, '--disable-extensions', '--disable-workspace-trust', '--skip-welcome', '--skip-release-notes', '--disable-gpu', '--new-window'],
  });
  process.exit(code);
}

main().catch((e) => { console.error(e); process.exit(1); });
