// Read-only blob documents, the Conflicts tree and the File History tree.

import * as vscode from 'vscode';
import { Conflict, PathLog } from './cli';
import { RepoController } from './repo';

export const BLOB_SCHEME = 'barocss-hub-blob';

/** `barocss-hub-blob:/<path>?folder=<index>&blob=<id>&label=<text>` (path keeps language detection). */
export function blobUri(repo: RepoController, rel: string, blob: string, label: string): vscode.Uri {
  const q = new URLSearchParams({ folder: String(repo.folder.index), blob, label });
  return vscode.Uri.from({ scheme: BLOB_SCHEME, path: '/' + rel, query: q.toString() });
}

export class BlobProvider implements vscode.TextDocumentContentProvider {
  constructor(private readonly repoAt: (index: number) => RepoController | undefined) {}
  async provideTextDocumentContent(uri: vscode.Uri): Promise<string> {
    const q = new URLSearchParams(uri.query);
    const repo = this.repoAt(Number(q.get('folder')));
    const blob = q.get('blob');
    if (!repo || !blob) { throw new Error('barocss-hub: unknown repository for ' + uri.toString()); }
    return (await repo.blob(blob)).toString('utf8');
  }
}

// ---- Conflicts ---------------------------------------------------------------

export class ConflictItem extends vscode.TreeItem {
  constructor(readonly repo: RepoController, readonly conflict: Conflict, multiRoot: boolean) {
    const where = conflict.path ?? (conflict.op ? `op ${conflict.op}` : '(trashed)');
    super(where, vscode.TreeItemCollapsibleState.None);
    this.description = conflict.kind + (multiRoot ? ` · ${repo.folder.name}` : '');
    this.tooltip = `${conflict.kind} conflict on ${where}` + (conflict.winner ? `\nwinner ${conflict.winner}` : '') + (conflict.loser ? `\nloser ${conflict.loser}` : '');
    const isContent = conflict.kind === 'content' && !!conflict.winner_blob && !!conflict.loser_blob && conflict.path !== null;
    this.contextValue = isContent ? 'content-conflict' : 'conflict';
    this.iconPath = new vscode.ThemeIcon(isContent ? 'diff' : 'warning');
    if (isContent) {
      this.command = { command: 'barocssHub.compareConflict', title: 'Compare with Losing Version', arguments: [this] };
    }
  }
}

export function conflictDiff(item: ConflictItem): { left: vscode.Uri; right: vscode.Uri; title: string } {
  const c = item.conflict;
  const rel = c.path!;
  return {
    left: blobUri(item.repo, rel, c.loser_blob!, `loser ${c.loser}`),
    right: blobUri(item.repo, rel, c.winner_blob!, `winner ${c.winner}`),
    title: `${rel}: losing version ↔ winning version`,
  };
}

export class ConflictsProvider implements vscode.TreeDataProvider<ConflictItem> {
  private readonly _onDidChange = new vscode.EventEmitter<void>();
  readonly onDidChangeTreeData = this._onDidChange.event;
  constructor(private readonly repos: () => RepoController[]) {}
  fire() { this._onDidChange.fire(); }
  getTreeItem(e: ConflictItem) { return e; }
  getChildren(): ConflictItem[] {
    const rs = this.repos();
    return rs.flatMap((r) => (r.status?.conflicts ?? []).map((c) => new ConflictItem(r, c, rs.length > 1)));
  }
}

// ---- File history (checkpoints of the active file) ----------------------------

export class HistoryItem extends vscode.TreeItem {
  constructor(label: string, readonly children: HistoryItem[] = [], state = vscode.TreeItemCollapsibleState.None) {
    super(label, state);
  }
}

/**
 * Items for `log --path` (pure; ops carry no timestamps, so order + message + author only).
 * Checkpoints newest first, numbered #n (oldest = #1), then the node's content ops.
 */
export function historyItems(repo: RepoController, log: PathLog): HistoryItem[] {
  const n = log.checkpoints.length;
  const items = log.checkpoints.map((cp, i) => {
    const it = new HistoryItem(`#${n - i} ${cp.message}`);
    it.description = `${cp.version.short} · ${cp.author.slice(0, 8)}${cp.published ? ' · published' : ''}`;
    it.tooltip = `checkpoint ${cp.version.short} "${cp.message}"\nauthor ${cp.author}\npath ${cp.path}`;
    it.iconPath = new vscode.ThemeIcon(cp.published ? 'cloud' : 'git-commit');
    it.contextValue = 'checkpoint';
    if (cp.blob) {
      it.command = { command: 'barocssHub.openVersion', title: 'Open Version', arguments: [blobUri(repo, cp.path, cp.blob, `${cp.version.short} ${cp.message}`)] };
    }
    return it;
  });
  const edits = log.ops.filter((o) => o.kind === 'set-content' && o.blob);
  if (edits.length) {
    const children = edits.map((o, i) => {
      const it = new HistoryItem(`edit ${edits.length - i}`);
      it.description = `op ${o.op.split('.')[0]} · ${(o.op.split('.')[1] ?? '').slice(0, 8)}`;
      it.tooltip = `set-content op ${o.op}\nblob ${o.blob}`;
      it.iconPath = new vscode.ThemeIcon('edit');
      it.command = { command: 'barocssHub.openVersion', title: 'Open Version', arguments: [blobUri(repo, log.path, o.blob!, `op ${o.op}`)] };
      return it;
    });
    const group = new HistoryItem(`All content edits (${edits.length})`, children, vscode.TreeItemCollapsibleState.Collapsed);
    group.iconPath = new vscode.ThemeIcon('history');
    items.push(group);
  }
  return items;
}

export class HistoryProvider implements vscode.TreeDataProvider<HistoryItem> {
  private readonly _onDidChange = new vscode.EventEmitter<void>();
  readonly onDidChangeTreeData = this._onDidChange.event;
  constructor(private readonly repoFor: (uri: vscode.Uri) => RepoController | undefined) {}
  fire() { this._onDidChange.fire(); }
  getTreeItem(e: HistoryItem) { return e; }
  async getChildren(e?: HistoryItem): Promise<HistoryItem[]> {
    if (e) { return e.children; }
    const uri = vscode.window.activeTextEditor?.document.uri;
    return uri ? this.itemsFor(uri) : [];
  }
  async itemsFor(uri: vscode.Uri): Promise<HistoryItem[]> {
    const repo = this.repoFor(uri);
    const rel = repo?.relPath(uri);
    if (!repo || !rel) { return [new HistoryItem('Not in a barocss-hub repository')]; }
    try {
      return historyItems(repo, await repo.pathLog(rel));
    } catch (err) {
      return [new HistoryItem(`No history: ${(err as Error).message}`)];
    }
  }
}
