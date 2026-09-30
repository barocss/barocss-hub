// One controller per workspace folder: its last `status --json`, its watch child.

import * as vscode from 'vscode';
import * as path from 'path';
import * as fs from 'fs';
import { run, CliError, Status, WatchProcess, WatchEvent, PathLog, blobBytes } from './cli';

export function realpath(p: string): string {
  try { return fs.realpathSync.native(p); } catch { return p; }
}

/** Status bar text for a status document (pure: tested directly too). */
export function statusLabel(s: Status | undefined, watching: boolean, error?: string): { text: string; state: string } {
  const eye = watching ? ' $(eye) watching' : '';
  if (error) { return { text: `$(error) hub: ${error}${eye}`, state: 'error' }; }
  if (!s) { return { text: `$(circle-slash) hub${eye}`, state: 'none' }; }
  const w = s.workdir;
  let text: string;
  let state: string;
  if (w.interrupted_checkout) {
    state = 'interrupted'; text = '$(warning) hub: interrupted checkout';
  } else if (s.conflicts.length > 0) {
    state = 'conflicts'; text = `$(git-merge) hub: ${s.conflicts.length} conflict${s.conflicts.length === 1 ? '' : 's'}`;
  } else if (w.state === 'pinned') {
    state = 'pinned'; text = `$(pinned) hub: pinned ${w.pinned?.short ?? ''}`.trimEnd();
  } else if (w.state === 'behind') {
    state = 'behind'; text = `$(arrow-down) hub: behind ${w.behind_ops}`;
  } else if (w.state === 'at-head') {
    state = 'at-head'; text = `$(check) hub: at head ${s.head.version.short}`;
  } else {
    state = w.state; text = `$(question) hub: ${w.state}`;
  }
  // A conflict list can coexist with pinned/behind: show both.
  if (state === 'conflicts' && (w.state === 'pinned' || w.state === 'behind')) {
    text += w.state === 'pinned' ? ` · pinned ${w.pinned?.short ?? ''}`.trimEnd() : ` · behind ${w.behind_ops}`;
  }
  return { text: text + eye, state };
}

export class RepoController implements vscode.Disposable {
  status: Status | undefined;
  error: CliError | undefined;
  watch: WatchProcess | undefined;
  /** Last root reported by the CLI (realpath). */
  root: string | undefined;
  private readonly _onDidChange = new vscode.EventEmitter<void>();
  readonly onDidChange = this._onDidChange.event;
  private readonly _onWatchEvent = new vscode.EventEmitter<WatchEvent>();
  readonly onWatchEvent = this._onWatchEvent.event;
  private refreshing: Promise<void> | undefined;
  private again = false;

  constructor(readonly folder: vscode.WorkspaceFolder, private readonly bin: () => string, private readonly log: vscode.OutputChannel) {}

  get cwd(): string { return this.folder.uri.fsPath; }
  get isRepo(): boolean { return !!this.status; }
  get watching(): boolean { return !!this.watch; }

  cli<T = Record<string, unknown>>(args: string[]): Promise<T> {
    this.log.appendLine(`[${this.folder.name}] barocss-hub ${args.join(' ')} --json`);
    return run<T>(this.bin(), this.cwd, args).catch((e: CliError) => {
      this.log.appendLine(`[${this.folder.name}]   -> ${e.code}: ${e.message}`);
      throw e;
    });
  }

  blob(id: string): Promise<Buffer> { return blobBytes(this.bin(), this.cwd, id); }

  /** Re-run `status --json`; coalesces concurrent calls. */
  refresh(): Promise<void> {
    if (this.refreshing) { this.again = true; return this.refreshing; }
    this.refreshing = (async () => {
      do {
        this.again = false;
        try {
          this.status = await this.cli<Status>(['status']);
          this.root = realpath(this.status.root);
          this.error = undefined;
        } catch (e) {
          const err = e as CliError;
          this.status = undefined;
          this.error = err.code === 'not-a-repository' ? undefined : err;
        }
      } while (this.again);
      this.refreshing = undefined;
      this._onDidChange.fire();
    })();
    return this.refreshing;
  }

  label() { return statusLabel(this.status, this.watching, this.error?.code); }

  /** Root-relative `/` path of a file in this repository, or undefined. */
  relPath(uri: vscode.Uri): string | undefined {
    if (uri.scheme !== 'file' || !this.root) { return undefined; }
    const rel = path.relative(this.root, realpath(uri.fsPath));
    if (!rel || rel.startsWith('..') || path.isAbsolute(rel)) { return undefined; }
    return rel.split(path.sep).join('/');
  }

  pathLog(rel: string): Promise<PathLog> { return this.cli<PathLog>(['log', '--path', rel]); }

  startWatch(extra: string[]): Promise<void> {
    if (this.watch) { return Promise.resolve(); }
    return new Promise((resolve, reject) => {
      let started = false;
      this.log.appendLine(`[${this.folder.name}] barocss-hub watch ${extra.join(' ')} --json`);
      const w = new WatchProcess(this.bin(), this.cwd, extra, (ev) => {
        this.log.appendLine(`[${this.folder.name}] watch: ${JSON.stringify(ev)}`);
        if (ev.event === 'started' && !started) { started = true; resolve(); }
        this._onWatchEvent.fire(ev);
        if (ev.event !== 'started' && ev.event !== 'stopped') { void this.refresh(); }
      }, (code, err) => {
        this.log.appendLine(`[${this.folder.name}] watch exited (${code})${err ? `: ${err.code} ${err.message}` : ''}`);
        if (this.watch === w) { this.watch = undefined; }
        void this.refresh();
        if (!started) { started = true; reject(err ?? new CliError('error', `watch exited with ${code}`)); }
      });
      this.watch = w;
      void this.refresh();
    });
  }

  async stopWatch(): Promise<void> {
    const w = this.watch;
    if (!w) { return; }
    await w.stop();
    this.watch = undefined;
    await this.refresh();
  }

  dispose(): void {
    void this.watch?.stop();
    this._onDidChange.dispose();
    this._onWatchEvent.dispose();
  }
}
