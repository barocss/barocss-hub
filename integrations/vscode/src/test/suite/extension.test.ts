import * as assert from 'assert';
import * as path from 'path';
import * as fs from 'fs';
import { spawnSync } from 'child_process';
import * as vscode from 'vscode';
import type { Api } from '../../extension';
import { statusLabel } from '../../repo';
import { watchNotice, explain, rescueSummary } from '../../errors';
import * as os from 'os';

const BIN = process.env.BAROCSS_HUB_BIN!;
const WS = process.env.HUB_TEST_WS!;
const PEER = process.env.HUB_TEST_PEER!;

/** Fixture driver: the barocss-hub CLI itself (human output is enough here). */
function hub(cwd: string, ...args: string[]): string {
  return hubEnv({}, cwd, ...args);
}
function hubEnv(env: Record<string, string>, cwd: string, ...args: string[]): string {
  const r = spawnSync(BIN, args, { cwd, encoding: 'utf8', env: { ...process.env, ...env } });
  if (r.status !== 0) { throw new Error(`barocss-hub ${args.join(' ')} in ${cwd}: ${r.stderr}`); }
  return r.stdout;
}
function write(dir: string, rel: string, text: string) { fs.writeFileSync(path.join(dir, rel), text); }
function alive(pid: number): boolean { try { process.kill(pid, 0); return true; } catch { return false; } }
async function waitFor<T>(what: string, f: () => T | undefined | false | Promise<T | undefined | false>, ms = 15000): Promise<T> {
  const end = Date.now() + ms;
  for (;;) {
    const v = await f();
    if (v) { return v; }
    if (Date.now() > end) { throw new Error(`timed out waiting for ${what}`); }
    await new Promise((r) => setTimeout(r, 100));
  }
}
const fileUri = (rel: string) => vscode.Uri.file(path.join(WS, rel));

describe('barocss-hub extension (real CLI, temp repos)', function () {
  let api: Api;
  const firstShort = () => /^([0-9a-f]+) first$/m.exec(hub(WS, 'log'))![1];

  before(async () => {
    const ext = vscode.extensions.all.find((e) => e.packageJSON.name === 'barocss-hub-vscode')!;
    api = (await ext.activate()) as Api;
    write(WS, 'f.txt', 'one\n');
    hub(WS, 'record', '-m', 'first');
    write(WS, 'f.txt', 'two\n');
    hub(WS, 'record', '-m', 'second');
    await api.refreshAll();
  });

  after(async () => { await api.stopAll(); });

  it('status bar: at head', async () => {
    assert.match(api.statusBar.text, /hub: at head [0-9a-f]{8}/);
    assert.strictEqual(api.statusBar.command, 'barocssHub.showActions');
  });

  it('status labels for every state (pure mapping)', () => {
    const base = { root: '/', head: { version: { id: 'x', short: 'aaaaaaaa' }, ops: 1, checkpoints: 0, published: 0 }, watch: { running: false, pid: null }, conflicts: [], sync_warnings: [], changes: [], blocked: [], warnings: [] };
    const wd = { state: 'at-head', version: null, behind_ops: 0, pinned: null, interrupted_checkout: false, clean: true };
    assert.strictEqual(statusLabel({ ...base, workdir: { ...wd, interrupted_checkout: true } }, false).state, 'interrupted');
    assert.strictEqual(statusLabel({ ...base, workdir: wd }, true).text, '$(check) hub: at head aaaaaaaa $(eye) watching');
    assert.strictEqual(statusLabel(undefined, false, 'corrupt').state, 'error');
  });

  it('timeline (file history): checkpoints in order with message and author, versions open read-only', async () => {
    const items = await api.history.itemsFor(fileUri('f.txt'));
    assert.deepStrictEqual(items.map((i) => i.label), ['#2 second', '#1 first', 'All content edits (2)']);
    assert.match(String(items[0].description), /^[0-9a-f]{8} · [0-9a-f]{8}$/);
    const uri = items[1].command!.arguments![0] as vscode.Uri;
    assert.strictEqual(uri.scheme, 'barocss-hub-blob');
    const doc = await vscode.workspace.openTextDocument(uri);
    assert.strictEqual(doc.getText(), 'one\n');
    assert.strictEqual(items[2].children.length, 2);
  });

  it('status bar: behind N after a sync brings ops', async () => {
    hub(PEER, 'sync', WS);
    hub(PEER, 'checkout');
    write(PEER, 'g.txt', 'from b\n');
    hub(PEER, 'record', '-m', 'b1');
    hub(WS, 'sync', PEER);
    await api.refreshAll();
    assert.match(api.statusBar.text, /hub: behind [1-9]/);
  });

  it('conflicts: tree lists kind and path, diff URIs resolve to loser and winner', async () => {
    hub(WS, 'checkout');
    hub(PEER, 'sync', WS);
    hub(PEER, 'checkout');
    write(WS, 'f.txt', 'mine-a\n');
    hub(WS, 'record');
    write(PEER, 'f.txt', 'theirs-b\n');
    hub(PEER, 'record');
    hub(WS, 'sync', PEER);
    await api.refreshAll();
    assert.match(api.statusBar.text, /hub: 1 conflict\b/);
    const kids = api.conflicts.getChildren();
    assert.strictEqual(kids.length, 1);
    assert.strictEqual(kids[0].label, 'f.txt');
    assert.strictEqual(kids[0].description, 'content');
    assert.strictEqual(kids[0].contextValue, 'content-conflict');
    const d = api.conflictDiff(kids[0]);
    const c = kids[0].conflict;
    assert.strictEqual(new URLSearchParams(d.left.query).get('blob'), c.loser_blob);
    assert.strictEqual(new URLSearchParams(d.right.query).get('blob'), c.winner_blob);
    const texts = [(await vscode.workspace.openTextDocument(d.left)).getText(), (await vscode.workspace.openTextDocument(d.right)).getText()].sort();
    assert.deepStrictEqual(texts, ['mine-a\n', 'theirs-b\n']);
    await vscode.commands.executeCommand('barocssHub.compareConflict', kids[0]);
    await waitFor('diff editor', () => vscode.window.tabGroups.all.flatMap((g) => g.tabs).some((t) => t.input instanceof vscode.TabInputTextDiff));
    await vscode.commands.executeCommand('workbench.action.closeAllEditors');
    hub(WS, 'checkout');
  });

  it('status bar: pinned after checkout of an old version, back after checkout', async () => {
    hub(WS, 'checkout', firstShort());
    await api.refreshAll();
    assert.match(api.statusBar.text, /pinned [0-9a-f]{8}/);
    await vscode.commands.executeCommand('barocssHub.checkout');
    assert.doesNotMatch(api.statusBar.text, /pinned/);
  });

  it('record -m via the command', async () => {
    write(WS, 'h.txt', 'h\n');
    await vscode.commands.executeCommand('barocssHub.record', 'from vscode');
    assert.match(hub(WS, 'log'), /from vscode/);
  });

  it('watch: start shows "watching" with a live pid; stop kills it', async () => {
    await vscode.commands.executeCommand('barocssHub.startWatch');
    const repo = api.repos()[0];
    const pid = repo.watch!.pid!;
    assert.ok(alive(pid));
    await waitFor('watching label', () => api.statusBar.text.includes('watching') && repo.status?.watch.running);
    await vscode.commands.executeCommand('barocssHub.stopWatch');
    await waitFor('watch exit', () => !alive(pid));
    assert.ok(!api.statusBar.text.includes('watching'));
    assert.strictEqual(repo.status!.watch.running, false);
  });

  it('unsaved-edit guard: incoming change on a dirty file offers Compare / Keep mine / Reload', async () => {
    hub(WS, 'remote', 'add', 'peer', PEER);
    const offered: string[][] = [];
    api.setGuardPrompt(async (_m, ...items) => { offered.push(items); return 'Keep mine'; });
    const doc = await vscode.workspace.openTextDocument(fileUri('h.txt'));
    const ed = await vscode.window.showTextDocument(doc);
    await ed.edit((e) => e.insert(new vscode.Position(0, 0), 'unsaved '));
    assert.ok(doc.isDirty);
    await vscode.commands.executeCommand('barocssHub.startWatch');
    hub(PEER, 'sync', WS);
    hub(PEER, 'checkout');
    write(PEER, 'h.txt', 'incoming\n');
    hub(PEER, 'record');
    const n = await waitFor('guard notice', () => api.guardNotices.find((x) => x.uri === doc.uri.toString()), 20000);
    assert.match(n.message, /h\.txt changed on disk/);
    assert.deepStrictEqual(offered[0], ['Compare', 'Keep mine', 'Reload']);
    await waitFor('keep mine applied', () => n.choice === 'Keep mine' && !doc.isDirty);
    assert.strictEqual(doc.getText(), 'unsaved h\n');
    // The watcher records the kept buffer as a new (concurrent) edit.
    await waitFor('kept edit recorded', async () => (await api.repos()[0].pathLog('h.txt')).ops.filter((o) => o.kind === 'set-content').length >= 3);
    api.setGuardPrompt(undefined);
    await vscode.commands.executeCommand('barocssHub.stopWatch');
    hub(WS, 'remote', 'rm', 'peer');
  });

  it('observes VS Code save temp names (report only)', async () => {
    const seen = new Set<string>();
    const w = fs.watch(WS, (_e, name) => { if (name) { seen.add(String(name)); } }); // boundary-allow: observes editor save temp names in the working dir
    // VS Code 1.103 has no atomic-save setting for editor saves; save twice with the defaults.
    for (let i = 0; i < 2; i++) {
      const doc = await vscode.workspace.openTextDocument(fileUri('h.txt'));
      const ed = await vscode.window.showTextDocument(doc);
      await ed.edit((e) => e.insert(new vscode.Position(0, 0), 'x'));
      await doc.save();
      await new Promise((r) => setTimeout(r, 500));
    }
    w.close();
    console.log(`SAVE-NAMES ${JSON.stringify([...seen].sort())}`);
    const st = await api.repos()[0].cli<{ changes: { path: string }[] }>(['status']);
    console.log(`SAVE-STATUS-CHANGES ${st.changes.map((c) => c.path).join(',')}`);
    assert.ok(seen.has('h.txt'));
  });

  it('Workspace Trust: restricted mode watches with --no-sync and says so; trusted syncs', async () => {
    const repo = api.repos()[0];
    const started: unknown[] = [];
    const sub = repo.onWatchEvent((ev) => { if (ev.event === 'started') { started.push(ev.sync); } });
    try {
      api.setTrustOverride(false);
      assert.ok(api.watchArgs().includes('--no-sync'));
      const before = api.notices.length;
      await vscode.commands.executeCommand('barocssHub.startWatch');
      await waitFor('started (restricted)', () => started.length === 1);
      assert.strictEqual(started[0], false);
      assert.ok(api.notices.slice(before).some((n) => /Restricted Mode/.test(n.message) && n.actions.includes('Manage Workspace Trust')));
      await vscode.commands.executeCommand('barocssHub.stopWatch');
      api.setTrustOverride(true);
      assert.ok(!api.watchArgs().includes('--no-sync'));
      await vscode.commands.executeCommand('barocssHub.startWatch');
      await waitFor('started (trusted)', () => started.length === 2);
      assert.strictEqual(started[1], true);
      await vscode.commands.executeCommand('barocssHub.stopWatch');
    } finally { sub.dispose(); api.setTrustOverride(undefined); }
  });

  it('a planted (never trusted) remote is skipped with one notification, never contacted', async () => {
    // Added under ANOTHER user's trust store: for this user it is exactly a
    // remote that came with the folder.
    hubEnv({ XDG_CONFIG_HOME: process.env.HUB_TEST_OTHER_XDG!, BAROCSS_HUB_CONFIG_DIR: path.join(process.env.HUB_TEST_OTHER_XDG!, 'barocss-hub') }, WS, 'remote', 'add', 'planted', 'ssh://planted.invalid/r');
    const repo = api.repos()[0];
    const events: string[] = [];
    const sub = repo.onWatchEvent((ev) => { events.push(ev.event); });
    const before = api.notices.length;
    try {
      api.setTrustOverride(true);
      await vscode.commands.executeCommand('barocssHub.startWatch');
      const n = await waitFor('untrusted notice', () => api.notices.slice(before).find((x) => /not syncing with remote planted/.test(x.message)));
      assert.match(n.message, /remote trust planted/);
      await new Promise((r) => setTimeout(r, 1200));
      assert.strictEqual(events.filter((e) => e === 'untrusted-remote').length, 1);
      assert.ok(!events.includes('error'), events.join(','));
    } finally {
      sub.dispose();
      api.setTrustOverride(undefined);
      await vscode.commands.executeCommand('barocssHub.stopWatch');
      hub(WS, 'remote', 'rm', 'planted');
    }
  });

  it('rejected-op events become a warning naming the op and what to do', () => {
    const n = watchNotice({ schema: 1, event: 'rejected', peer: 'origin', ops: [{ op: '5.00ab', reason: 'equivocation', message: 'run `barocss-hub reid` here' }] })!;
    assert.strictEqual(n.severity, 'warning');
    assert.match(n.message, /remote origin rejected 1 of your change\(s\): 5\.00ab \(equivocation\): run `barocss-hub reid` here/);
    assert.strictEqual(watchNotice({ schema: 1, event: 'synced' }), undefined);
  });

  it('duplicate-replica offers Rescue: dry-run plan, confirm, then reid --rescue (CLI only)', async () => {
    assert.ok(explain('duplicate-replica', '').actions.includes('Rescue…'));
    const t = fs.mkdtempSync(path.join(os.tmpdir(), 'hub49-vsc-'));
    const a = path.join(t, 'a'); const c = path.join(t, 'c'); const srv = path.join(t, 'srv');
    hub(t, 'init', '--bare', 'srv');
    fs.mkdirSync(a); hub(a, 'init');
    hub(a, 'remote', 'add', '--default', 'origin', `file://${fs.realpathSync(srv)}`);
    write(a, 'f', 'base\n'); hub(a, 'record'); hub(a, 'sync');
    fs.cpSync(a, c, { recursive: true });
    hub(c, 'remote', 'trust', 'origin');
    write(a, 'f', 'A'); hub(a, 'record'); hub(a, 'sync');
    write(c, 'f', 'BB'); hub(c, 'record');
    assert.notStrictEqual(spawnSync(BIN, ['sync'], { cwd: c }).status, 0);
    // Declined: nothing changes, the sync is still refused.
    let shown = '';
    assert.strictEqual(await api.rescue(c, async (p) => { shown = rescueSummary(p); return false; }), undefined);
    assert.match(shown, /Re-record 1 rejected change/);
    assert.match(shown, /modified: f/);
    assert.notStrictEqual(spawnSync(BIN, ['sync'], { cwd: c }).status, 0);
    // Confirmed: rescued and confirmed by one sync; the next sync exits 0.
    const res = (await api.rescue(c, async () => true))!;
    assert.strictEqual(res.noop, false);
    assert.strictEqual((res.sync as { ok: boolean }).ok, true);
    assert.strictEqual(spawnSync(BIN, ['sync'], { cwd: c }).status, 0);
    fs.rmSync(t, { recursive: true, force: true });
  });

  it('deactivate-style stop leaves no watch process', async () => {
    await vscode.commands.executeCommand('barocssHub.startWatch');
    const pid = api.repos()[0].watch!.pid!;
    await api.stopAll();
    await waitFor('watch exit', () => !alive(pid));
  });
});
