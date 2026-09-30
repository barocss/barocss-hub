// The only bridge to the barocss-hub core: spawn `barocss-hub <args> --json`
// and parse the single JSON document it prints (Wiki CLI-JSON, schema 1).
// Nothing in this extension reads repository internals; every fact comes from
// a CLI document or a `watch --json` event.

import { spawn, ChildProcess } from 'child_process';
import * as readline from 'readline';

export const SCHEMA = 1;

/** A CLI failure: `error.code` from the stderr document (or a local code). */
export class CliError extends Error {
  constructor(
    public readonly code: string,
    message: string,
    public readonly args: string[] = [],
  ) {
    super(message);
  }
}

export interface Version { id: string; short: string }
export interface Checkpoint { version: Version; message: string; published: boolean; author: string }
export interface Conflict {
  kind: string;
  node?: string;
  path: string | null;
  winner?: string;
  loser?: string;
  winner_blob?: string;
  loser_blob?: string;
  name?: string;
  op?: string;
}
export interface Status {
  root: string;
  head: { version: Version; ops: number; checkpoints: number; published: number };
  workdir: {
    state: 'at-head' | 'behind' | 'pinned' | 'unknown' | string;
    version: Version | null;
    behind_ops: number;
    pinned: Version | null;
    interrupted_checkout: boolean;
    clean: boolean;
  };
  watch: { running: boolean; pid: number | null };
  conflicts: Conflict[];
  sync_warnings: unknown[];
  changes: unknown[];
  blocked: unknown[];
  warnings: string[];
}
export interface PathCheckpoint extends Checkpoint { path: string; blob: string | null }
export interface PathLog {
  path: string;
  node: string;
  kind: string;
  blob: string | null;
  checkpoints: PathCheckpoint[];
  ops: { op: string; kind: string; blob?: string; name?: string }[];
}
export interface WatchEvent { schema: number; event: string; [k: string]: unknown }

function parseDoc(text: string): Record<string, unknown> | undefined {
  const t = text.trim();
  if (!t) { return undefined; }
  try {
    const v = JSON.parse(t.split('\n')[0]);
    return v && typeof v === 'object' ? v as Record<string, unknown> : undefined;
  } catch {
    return undefined;
  }
}

/** Run one CLI command with `--json`; resolves with the document or rejects with CliError. */
export function run<T = Record<string, unknown>>(bin: string, cwd: string, args: string[], timeoutMs = 60000): Promise<T> {
  return new Promise((resolve, reject) => {
    let child: ChildProcess;
    try {
      child = spawn(bin, [...args, '--json'], { cwd, stdio: ['ignore', 'pipe', 'pipe'] });
    } catch (e) {
      reject(new CliError('spawn', `cannot run ${bin}: ${(e as Error).message}`, args));
      return;
    }
    const out: Buffer[] = [];
    const err: Buffer[] = [];
    child.stdout!.on('data', (d: Buffer) => out.push(d));
    child.stderr!.on('data', (d: Buffer) => err.push(d));
    const timer = setTimeout(() => child.kill('SIGKILL'), timeoutMs);
    child.on('error', (e: NodeJS.ErrnoException) => {
      clearTimeout(timer);
      const code = e.code === 'ENOENT' ? 'binary-not-found' : 'spawn';
      reject(new CliError(code, `cannot run '${bin}': ${e.message}`, args));
    });
    child.on('close', (status) => {
      clearTimeout(timer);
      const stdout = Buffer.concat(out).toString('utf8');
      const stderr = Buffer.concat(err).toString('utf8');
      if (status === 0) {
        const doc = parseDoc(stdout);
        if (!doc || doc.schema !== SCHEMA) {
          reject(new CliError('bad-json', `unexpected output from barocss-hub ${args.join(' ')} (schema ${doc?.schema ?? 'none'})`, args));
        } else {
          resolve(doc as T);
        }
        return;
      }
      const doc = parseDoc(stderr) as { error?: { code?: string; message?: string } } | undefined;
      if (doc?.error?.code) {
        reject(new CliError(doc.error.code, doc.error.message ?? doc.error.code, args));
      } else {
        reject(new CliError('error', stderr.trim() || `barocss-hub ${args.join(' ')} exited with ${status}`, args));
      }
    });
  });
}

/** Bytes of a stored blob, via `cat-blob <id> --json` (base64 content). */
export async function blobBytes(bin: string, cwd: string, blob: string): Promise<Buffer> {
  const doc = await run<{ encoding: string; content: string }>(bin, cwd, ['cat-blob', blob]);
  if (doc.encoding !== 'base64') { throw new CliError('bad-json', `unknown encoding ${doc.encoding}`); }
  return Buffer.from(doc.content, 'base64');
}

/** A running `barocss-hub watch --json` child. */
export class WatchProcess {
  readonly child: ChildProcess;
  private exited: Promise<number | null>;
  private stderr = '';

  constructor(bin: string, cwd: string, extra: string[], onEvent: (e: WatchEvent) => void, onExit: (code: number | null, err?: CliError) => void) {
    this.child = spawn(bin, ['watch', ...extra, '--json'], { cwd, stdio: ['ignore', 'pipe', 'pipe'] });
    readline.createInterface({ input: this.child.stdout! }).on('line', (line) => {
      const doc = parseDoc(line);
      if (doc && doc.schema === SCHEMA && typeof doc.event === 'string') { onEvent(doc as WatchEvent); }
    });
    this.child.stderr!.on('data', (d: Buffer) => { this.stderr += d.toString('utf8'); });
    this.exited = new Promise((resolve) => {
      this.child.on('error', (e: NodeJS.ErrnoException) => {
        onExit(null, new CliError(e.code === 'ENOENT' ? 'binary-not-found' : 'spawn', `cannot run '${bin}': ${e.message}`));
        resolve(null);
      });
      this.child.on('exit', (code) => {
        let err: CliError | undefined;
        if (code !== 0 && code !== null) {
          const doc = parseDoc(this.stderr) as { error?: { code?: string; message?: string } } | undefined;
          err = new CliError(doc?.error?.code ?? 'error', doc?.error?.message ?? (this.stderr.trim() || `watch exited with ${code}`));
        }
        onExit(code, err);
        resolve(code);
      });
    });
  }

  get pid(): number | undefined { return this.child.pid; }

  /** SIGTERM (the watcher stops cleanly), SIGKILL after `graceMs`. */
  async stop(graceMs = 3000): Promise<void> {
    if (this.child.exitCode !== null || this.child.signalCode !== null) { return; }
    this.child.kill('SIGTERM');
    const t = setTimeout(() => this.child.kill('SIGKILL'), graceMs);
    await this.exited;
    clearTimeout(t);
  }
}
