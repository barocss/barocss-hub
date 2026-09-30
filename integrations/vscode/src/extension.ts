// barocss-hub for VS Code (D-034). Everything goes through `barocss-hub --json`.

import * as vscode from 'vscode';
import { CliError, Checkpoint, WatchEvent } from './cli';
import { explain, Action } from './errors';
import { RepoController } from './repo';
import { BLOB_SCHEME, BlobProvider, ConflictItem, ConflictsProvider, HistoryProvider, conflictDiff, blobUri } from './views';

type Prompt = (message: string, ...items: string[]) => Thenable<string | undefined>;

export interface GuardNotice { uri: string; message: string; incomingBlob: string; choice: string | undefined }

/** Returned from activate(); used by the integration tests (and other extensions). */
export interface Api {
  repos: () => RepoController[];
  statusBar: vscode.StatusBarItem;
  conflicts: ConflictsProvider;
  history: HistoryProvider;
  conflictDiff: typeof conflictDiff;
  refreshAll: () => Promise<void>;
  /** Replaces the unsaved-edit notification prompt (tests auto-answer it). */
  setGuardPrompt: (p: Prompt | undefined) => void;
  guardNotices: GuardNotice[];
  stopAll: () => Promise<void>;
}

let controllers: RepoController[] = [];

function cfg() { return vscode.workspace.getConfiguration('barocssHub'); }
function bin(): string { return cfg().get<string>('binaryPath') || 'barocss-hub'; }

export async function activate(context: vscode.ExtensionContext): Promise<Api> {
  const log = vscode.window.createOutputChannel('barocss-hub');
  const statusBar = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 50);
  statusBar.command = 'barocssHub.showActions';
  statusBar.name = 'barocss-hub';
  context.subscriptions.push(log, statusBar);

  const conflicts = new ConflictsProvider(() => controllers.filter((c) => c.isRepo));
  const history = new HistoryProvider((uri) => repoFor(uri));
  context.subscriptions.push(
    vscode.window.registerTreeDataProvider('barocssHub.conflicts', conflicts),
    vscode.window.registerTreeDataProvider('barocssHub.history', history),
    vscode.workspace.registerTextDocumentContentProvider(BLOB_SCHEME, new BlobProvider((i) => controllers.find((c) => c.folder.index === i))),
  );

  let guardPrompt: Prompt | undefined;
  const guardNotices: GuardNotice[] = [];
  /** Saved text of each open document (what the editor believes is on disk). */
  const baseline = new Map<string, string>();
  const noteBaseline = (d: vscode.TextDocument) => { if (d.uri.scheme === 'file' && !d.isDirty) { baseline.set(d.uri.toString(), d.getText()); } };
  vscode.workspace.textDocuments.forEach(noteBaseline);

  function repoFor(uri: vscode.Uri): RepoController | undefined {
    const f = vscode.workspace.getWorkspaceFolder(uri);
    const byFolder = f && controllers.find((c) => c.folder.uri.toString() === f.uri.toString() && c.isRepo);
    return byFolder ?? controllers.find((c) => c.isRepo && c.relPath(uri) !== undefined);
  }

  /** The repository the status bar speaks for: the active editor's, else the first. */
  function current(): RepoController | undefined {
    const uri = vscode.window.activeTextEditor?.document.uri;
    return (uri && repoFor(uri)) || controllers.find((c) => c.isRepo) || controllers.find((c) => c.error) ;
  }

  function render() {
    const c = current();
    if (!c) { statusBar.hide(); return; }
    const l = c.label();
    const multi = controllers.filter((x) => x.isRepo).length > 1;
    statusBar.text = l.text + (multi ? ` (${c.folder.name})` : '');
    statusBar.tooltip = c.status
      ? `barocss-hub ${c.folder.name}: head ${c.status.head.version.short}, ${c.status.head.ops} ops, working directory ${c.status.workdir.state}${c.status.conflicts.length ? `, ${c.status.conflicts.length} conflict(s)` : ''}${c.watching ? `, watch pid ${c.watch?.pid}` : ''}\nClick for actions`
      : `barocss-hub: ${c.error?.message ?? 'no repository'}`;
    statusBar.backgroundColor = ['conflicts', 'interrupted', 'error'].includes(l.state)
      ? new vscode.ThemeColor('statusBarItem.warningBackground') : undefined;
    statusBar.show();
    void vscode.commands.executeCommand('setContext', 'barocssHub.active', true);
  }

  /** Shows the explained error; the chosen action runs later (commands never block on it). */
  function showError(e: unknown, repo?: RepoController): Promise<void> {
    void handleError(e, repo);
    return Promise.resolve();
  }
  async function handleError(e: unknown, repo?: RepoController) {
    const err = e instanceof CliError ? e : new CliError('error', String((e as Error)?.message ?? e));
    const x = explain(err.code, err.message);
    const show = x.severity === 'error' ? vscode.window.showErrorMessage : x.severity === 'warning' ? vscode.window.showWarningMessage : vscode.window.showInformationMessage;
    const pick = await show(x.message, ...x.actions) as Action | undefined;
    const r = repo ?? current();
    switch (pick) {
      case 'Checkout': if (r) { await doCheckout(r, false); } break;
      case 'Force Checkout': if (r) { await doCheckout(r, true); } break;
      case 'Open Settings': await vscode.commands.executeCommand('workbench.action.openSettings', 'barocssHub.binaryPath'); break;
      case 'Stop Watch': if (r) { await r.stopWatch(); } break;
      case 'Show Log': log.show(); break;
      case 'Copy reid Command': await vscode.env.clipboard.writeText(`cd "${r?.status?.root ?? r?.cwd ?? ''}" && barocss-hub reid`); break;
    }
  }

  async function pickRepo(): Promise<RepoController | undefined> {
    const repos = controllers.filter((c) => c.isRepo);
    if (repos.length <= 1) { return repos[0] ?? current(); }
    const cur = current();
    const p = await vscode.window.showQuickPick(repos.map((r) => ({ label: r.folder.name, description: r.label().text.replace(/\$\([^)]*\)\s*/g, ''), r, picked: r === cur })), { placeHolder: 'barocss-hub repository' });
    return p?.r;
  }

  async function doCheckout(r: RepoController, force: boolean) {
    try {
      const d = await r.cli<{ version: { short: string }; written: number; removed: number }>(force ? ['checkout', '--force'] : ['checkout']);
      vscode.window.setStatusBarMessage(`barocss-hub: checked out ${d.version.short} (${d.written} written, ${d.removed} removed)`, 5000);
    } catch (e) { await showError(e, r); }
    await r.refresh();
  }

  async function startWatch(r: RepoController) {
    try { await r.startWatch(cfg().get<string[]>('watchArgs') ?? []); } catch (e) { await showError(e, r); }
  }

  // ---- unsaved-edit guard ------------------------------------------------------
  async function guard(r: RepoController, ev: WatchEvent) {
    if (ev.event !== 'checked-out') { return; }
    for (const doc of vscode.workspace.textDocuments) {
      if (!doc.isDirty || doc.uri.scheme !== 'file') { continue; }
      const rel = r.relPath(doc.uri);
      if (!rel) { continue; }
      let incoming: string | null;
      try { incoming = (await r.pathLog(rel)).blob; } catch { continue; }
      if (!incoming) { continue; }
      const bytes = (await r.blob(incoming)).toString('utf8');
      const base = baseline.get(doc.uri.toString());
      if (base !== undefined && base === bytes) { continue; }
      const message = `barocss-hub: ${rel} changed on disk (incoming change) while you have unsaved edits.`;
      const prompt: Prompt = guardPrompt ?? ((m, ...items) => vscode.window.showWarningMessage(m, ...items));
      const notice: GuardNotice = { uri: doc.uri.toString(), message, incomingBlob: incoming, choice: undefined };
      guardNotices.push(notice);
      baseline.set(doc.uri.toString(), bytes);
      void Promise.resolve(prompt(message, 'Compare', 'Keep mine', 'Reload')).then(async (choice) => {
        notice.choice = choice;
        if (choice === 'Compare') {
          await vscode.commands.executeCommand('vscode.diff', blobUri(r, rel, incoming!, 'incoming'), doc.uri, `${rel}: incoming ↔ yours (unsaved)`);
        } else if (choice === 'Keep mine') {
          // Write the buffer over the incoming bytes, then revert the (now identical) buffer:
          // the watcher records it as a concurrent edit.
          const mine = doc.getText();
          await vscode.workspace.fs.writeFile(doc.uri, Buffer.from(mine, 'utf8'));
          await vscode.window.showTextDocument(doc);
          await vscode.commands.executeCommand('workbench.action.files.revert');
          baseline.set(doc.uri.toString(), mine);
        } else if (choice === 'Reload') {
          await vscode.window.showTextDocument(doc);
          await vscode.commands.executeCommand('workbench.action.files.revert');
        }
      });
    }
  }

  // ---- controllers per workspace folder -----------------------------------------
  function wire(c: RepoController) {
    c.onDidChange(() => { render(); conflicts.fire(); history.fire(); });
    c.onWatchEvent((ev) => { void guard(c, ev); if (ev.event === 'error') { log.appendLine(`watch error (${String(ev.stage)}): ${String(ev.message)}`); } });
  }
  async function syncFolders() {
    const folders = vscode.workspace.workspaceFolders ?? [];
    const keep = controllers.filter((c) => folders.some((f) => f.uri.toString() === c.folder.uri.toString()));
    for (const c of controllers) { if (!keep.includes(c)) { c.dispose(); } }
    const added = folders.filter((f) => !keep.some((c) => c.folder.uri.toString() === f.uri.toString())).map((f) => {
      const c = new RepoController(f, bin, log); wire(c); return c;
    });
    controllers = [...keep, ...added];
    await Promise.all(added.map((c) => c.refresh()));
    if (cfg().get<boolean>('autoStartWatch')) {
      for (const c of added) { if (c.isRepo && !c.status!.watch.running) { await startWatch(c); } }
    }
    render();
  }
  const refreshAll = async () => { await Promise.all(controllers.map((c) => c.refresh())); render(); };
  const stopAll = async () => { await Promise.all(controllers.map((c) => c.stopWatch())); };

  let saveTimer: NodeJS.Timeout | undefined;
  context.subscriptions.push(
    vscode.workspace.onDidChangeWorkspaceFolders(() => void syncFolders()),
    vscode.workspace.onDidOpenTextDocument(noteBaseline),
    vscode.workspace.onDidSaveTextDocument((d) => {
      noteBaseline(d);
      if (saveTimer) { clearTimeout(saveTimer); }
      saveTimer = setTimeout(() => { const r = repoFor(d.uri); if (r) { void r.refresh(); } }, 300);
    }),
    vscode.workspace.onDidCloseTextDocument((d) => baseline.delete(d.uri.toString())),
    vscode.window.onDidChangeActiveTextEditor(() => { render(); history.fire(); }),
    vscode.window.onDidChangeWindowState((s) => { if (s.focused) { void refreshAll(); } }),
    vscode.workspace.onDidChangeConfiguration((e) => { if (e.affectsConfiguration('barocssHub.binaryPath')) { void refreshAll(); } }),
    { dispose: () => { if (saveTimer) { clearTimeout(saveTimer); } } },
  );
  const poll = cfg().get<number>('pollInterval') ?? 15000;
  if (poll > 0) {
    const t = setInterval(() => void refreshAll(), Math.max(1000, poll));
    context.subscriptions.push({ dispose: () => clearInterval(t) });
  }

  // ---- commands --------------------------------------------------------------------
  const reg = (id: string, f: (...a: any[]) => unknown) => context.subscriptions.push(vscode.commands.registerCommand(id, f));
  reg('barocssHub.refresh', refreshAll);
  reg('barocssHub.startWatch', async () => { const r = await pickRepo(); if (r) { await startWatch(r); } });
  reg('barocssHub.stopWatch', async () => { const r = await pickRepo(); if (r) { await r.stopWatch(); } });
  reg('barocssHub.checkout', async () => { const r = await pickRepo(); if (r) { await doCheckout(r, false); } });
  reg('barocssHub.record', async (message?: string) => {
    const r = await pickRepo(); if (!r) { return; }
    const m = typeof message === 'string' ? message : await vscode.window.showInputBox({ prompt: 'Checkpoint message (barocss-hub record -m)', placeHolder: 'message' });
    if (!m) { return; }
    try {
      const d = await r.cli<{ recorded: boolean; changes: unknown[]; checkpoint: Checkpoint | null }>(['record', '-m', m]);
      vscode.window.setStatusBarMessage(`barocss-hub: ${d.changes.length} change(s) recorded, checkpoint ${d.checkpoint?.version.short ?? '-'}`, 5000);
    } catch (e) { await showError(e, r); }
    await r.refresh();
  });
  reg('barocssHub.publish', async (id?: string) => {
    const r = await pickRepo(); if (!r) { return; }
    let target = typeof id === 'string' ? id : undefined;
    if (!target) {
      try {
        const d = await r.cli<{ checkpoints: Checkpoint[] }>(['log']);
        const items = d.checkpoints.filter((c) => !c.published).map((c) => ({ label: c.message, description: c.version.short, id: c.version.short }));
        if (!items.length) { void vscode.window.showInformationMessage('barocss-hub: no unpublished checkpoints.'); return; }
        target = (await vscode.window.showQuickPick(items, { placeHolder: 'Checkpoint to publish' }))?.id;
      } catch (e) { await showError(e, r); return; }
    }
    if (!target) { return; }
    try { await r.cli(['publish', target]); vscode.window.setStatusBarMessage(`barocss-hub: published ${target}`, 5000); } catch (e) { await showError(e, r); }
    await r.refresh();
  });
  reg('barocssHub.compareConflict', async (item?: ConflictItem) => {
    if (!item) { return; }
    const d = conflictDiff(item);
    await vscode.commands.executeCommand('vscode.diff', d.left, d.right, d.title);
  });
  reg('barocssHub.openVersion', async (uri: vscode.Uri) => {
    const doc = await vscode.workspace.openTextDocument(uri);
    await vscode.window.showTextDocument(doc, { preview: true });
  });
  reg('barocssHub.showActions', async () => {
    const r = current();
    type It = vscode.QuickPickItem & { run: () => unknown };
    const items: It[] = [];
    if (r?.isRepo) {
      const s = r.status!;
      if (s.workdir.state !== 'at-head' || s.workdir.interrupted_checkout) { items.push({ label: '$(arrow-down) Checkout latest', description: 'barocss-hub checkout', run: () => doCheckout(r, false) }); }
      items.push(r.watching
        ? { label: '$(debug-stop) Stop watch', description: `pid ${r.watch?.pid}`, run: () => r.stopWatch() }
        : { label: '$(eye) Start watch', description: s.watch.running ? `another watcher is running (pid ${s.watch.pid})` : 'barocss-hub watch', run: () => startWatch(r) });
      items.push({ label: '$(git-commit) Record checkpoint…', description: 'barocss-hub record -m', run: () => vscode.commands.executeCommand('barocssHub.record') });
      items.push({ label: '$(cloud-upload) Publish checkpoint…', description: 'barocss-hub publish', run: () => vscode.commands.executeCommand('barocssHub.publish') });
      if (s.conflicts.length) { items.push({ label: `$(git-merge) Show ${s.conflicts.length} conflict(s)`, run: () => vscode.commands.executeCommand('barocssHub.conflicts.focus') }); }
    }
    items.push({ label: '$(refresh) Refresh', run: refreshAll });
    items.push({ label: '$(output) Show log', run: () => log.show() });
    items.push({ label: '$(gear) Settings', run: () => vscode.commands.executeCommand('workbench.action.openSettings', 'barocssHub') });
    const p = await vscode.window.showQuickPick(items, { placeHolder: r ? `barocss-hub: ${r.folder.name}` : 'barocss-hub' });
    await p?.run();
  });

  await syncFolders();
  return {
    repos: () => controllers,
    statusBar, conflicts, history, conflictDiff, refreshAll,
    setGuardPrompt: (p) => { guardPrompt = p; },
    guardNotices,
    stopAll,
  };
}

export async function deactivate(): Promise<void> {
  await Promise.all(controllers.map((c) => c.stopWatch().catch(() => undefined)));
  controllers.forEach((c) => c.dispose());
  controllers = [];
}
