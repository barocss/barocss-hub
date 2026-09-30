#!/usr/bin/env node
// Architecture boundary (D-034): the extension talks to the core ONLY by spawning
// `barocss-hub ... --json`. Fails if extension source (src/, tests included):
//  - mentions the repository-internals directory (".hub" as a path segment),
//  - reads files with fs / workspace.fs (reads, dir listings, streams, stat, watchers),
//  - parses JSON anywhere but src/cli.ts (the CLI document / watch event parser),
//  - spawns processes anywhere but src/cli.ts (tests may spawn only the barocss-hub CLI binary).
// Run by `npm run lint` and by the Rust test crates/hub-cli/tests/vscode_boundary.rs.
'use strict';
const fs = require('fs');
const path = require('path');

const src = path.join(__dirname, '..', 'src');
const files = [];
(function walk(d) {
  for (const e of fs.readdirSync(d, { withFileTypes: true })) {
    const p = path.join(d, e.name);
    if (e.isDirectory()) { walk(p); } else if (/\.ts$/.test(e.name)) { files.push(p); }
  }
})(src);

const rules = [
  { re: /(^|[^A-Za-z0-9_-])\.hub(?![A-Za-z0-9_-])/, why: 'references the .hub directory' },
  { re: /\b(readFile|readFileSync|readdir|readdirSync|opendir|createReadStream|statSync|lstatSync|existsSync|accessSync|openSync|watchFile|fs\.watch|createFileSystemWatcher|readDirectory)\b/, why: 'reads the file system directly' },
  { re: /\bworkspace\.fs\.(readFile|stat|readDirectory)\b/, why: 'reads through workspace.fs' },
];
const onlyCli = [
  { re: /\bJSON\.parse\b/, why: 'parses JSON outside src/cli.ts' },
  { re: /(?<![.\w])(spawn|spawnSync|exec|execSync|execFile|execFileSync|fork)\s*\(/, why: 'spawns a process outside src/cli.ts' },
  { re: /(child_process)['"]/, why: 'imports child_process outside src/cli.ts' },
];

// Tests build fixtures with the CLI binary (spawnSync of BAROCSS_HUB_BIN) and may write
// working-directory files; they still must not touch .hub or read files directly.
const testAllowed = new Set(['spawnSync']);
const testImport = /^import \{ spawnSync \} from 'child_process';$/;

const problems = [];
for (const f of files) {
  const rel = path.relative(path.join(__dirname, '..'), f).split(path.sep).join('/');
  const isCli = rel === 'src/cli.ts';
  const isTest = rel.startsWith('src/test/');
  fs.readFileSync(f, 'utf8').split('\n').forEach((line, i) => {
    if (isTest && /\/\/ boundary-allow: \S/.test(line)) { return; } // test-only, reason required
    const code = line.replace(/\/\/.*$/, '');
    for (const r of rules) { if (r.re.test(code)) { problems.push(`${rel}:${i + 1}: ${r.why}: ${line.trim()}`); } }
    if (!isCli) {
      for (const r of onlyCli) {
        const m = code.match(r.re);
        if (m && !(isTest && (testAllowed.has(m[1]) || testImport.test(line.trim())))) { problems.push(`${rel}:${i + 1}: ${r.why}: ${line.trim()}`); }
      }
    }
  });
}
if (process.argv.includes('--self-test')) {
  // The rules must catch the obvious violations.
  const bad = ['fs.readFileSync(root + "/.hub/ops.log")', "const p = '.hub/index'", 'JSON.parse(x)', "import { spawn } from 'child_process';", 'vscode.workspace.fs.readFile(u)'];
  for (const b of bad) {
    const hit = [...rules, ...onlyCli].some((r) => r.re.test(b));
    if (!hit) { console.error(`self-test: rule missed: ${b}`); process.exit(2); }
  }
  for (const ok of ['barocss-hub status', '.hubignore', 'barocssHub.binaryPath']) {
    if (rules.some((r) => r.re.test(ok))) { console.error(`self-test: false positive: ${ok}`); process.exit(2); }
  }
}
if (problems.length) {
  console.error('boundary check FAILED (the extension must use only `barocss-hub --json`):');
  for (const p of problems) { console.error('  ' + p); }
  process.exit(1);
}
console.log(`boundary check ok (${files.length} files)`);
