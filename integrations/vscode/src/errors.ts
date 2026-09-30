// Typed CLI error codes (Wiki CLI-JSON §1) -> a clear message and suggested actions.

export type Action = 'Checkout' | 'Force Checkout' | 'Open Settings' | 'Stop Watch' | 'Show Log' | 'Copy reid Command' | 'Manage Workspace Trust';

export interface Explained { message: string; actions: Action[]; severity: 'error' | 'warning' | 'info' }

export function explain(code: string, raw: string): Explained {
  switch (code) {
    case 'interrupted':
      return { message: 'barocss-hub: a checkout was interrupted. Run Checkout to finish it.', actions: ['Checkout'], severity: 'warning' };
    case 'pinned':
      return { message: 'barocss-hub: the working directory is pinned to an old version (the watcher is paused). Checkout head to return.', actions: ['Checkout'], severity: 'warning' };
    case 'behind':
      return { message: 'barocss-hub: the working directory is behind head (incoming changes). Checkout to bring it up to date, then retry.', actions: ['Checkout'], severity: 'warning' };
    case 'blocked':
      return { message: `barocss-hub: some paths collide with case-clashing paths that cannot be written on this file system. ${raw}`, actions: ['Show Log'], severity: 'warning' };
    case 'refused':
      return { message: `barocss-hub: the update would overwrite unrecorded or ignored data. Record your changes first, or Force Checkout to discard them. ${raw}`, actions: ['Force Checkout', 'Show Log'], severity: 'warning' };
    case 'duplicate-replica':
      return { message: 'barocss-hub: sync aborted because this repository looks like a copy of another (same replica id). Run `barocss-hub reid` in the copy before recording there.', actions: ['Copy reid Command', 'Show Log'], severity: 'error' };
    case 'corrupt':
      return { message: `barocss-hub: the repository data is damaged or has an unsupported format. ${raw}`, actions: ['Show Log'], severity: 'error' };
    case 'not-a-repository':
      return { message: 'barocss-hub: this folder is not a barocss-hub repository (run `barocss-hub init` there).', actions: [], severity: 'info' };
    case 'locked':
      return { message: 'barocss-hub: another watcher (or lock holder) is already running for this repository.', actions: ['Stop Watch', 'Show Log'], severity: 'warning' };
    case 'binary-not-found':
      return { message: `barocss-hub: the barocss-hub binary was not found. Set barocssHub.binaryPath to its absolute path. (${raw})`, actions: ['Open Settings'], severity: 'error' };
    case 'bad-json':
      return { message: `barocss-hub: unexpected CLI output; is barocssHub.binaryPath a compatible barocss-hub (JSON schema 1)? ${raw}`, actions: ['Open Settings', 'Show Log'], severity: 'error' };
    case 'ambiguous':
      return { message: `barocss-hub: that id or message matches several versions; use a longer id. ${raw}`, actions: [], severity: 'warning' };
    case 'not-found':
    case 'no-such-path':
    case 'no-such-blob':
      return { message: `barocss-hub: not found. ${raw}`, actions: [], severity: 'warning' };
    case 'untrusted-remote':
      return { message: `barocss-hub: a remote configured in this folder was never used from this machine, so it was not contacted. Check it, then run \`barocss-hub remote trust <name>\` in a terminal. ${raw}`, actions: ['Show Log'], severity: 'warning' };
    case 'io':
      return { message: `barocss-hub: file system error. ${raw}`, actions: ['Show Log'], severity: 'error' };
    default:
      return { message: `barocss-hub: ${raw}`, actions: ['Show Log'], severity: 'error' };
  }
}

/** Watch events that deserve a notification (Issue #45); undefined for the rest. */
export function watchNotice(ev: { event: string; [k: string]: unknown }): Explained | undefined {
  if (ev.event === 'rejected') {
    const ops = Array.isArray(ev.ops) ? ev.ops as { op?: string; reason?: string; message?: string }[] : [];
    const list = ops.map((o) => `${String(o.op)} (${String(o.reason)}): ${String(o.message)}`).join('; ');
    return { message: `barocss-hub: remote ${String(ev.peer)} rejected ${ops.length} of your change(s): ${list}`, actions: ['Show Log'], severity: 'warning' };
  }
  if (ev.event === 'untrusted-remote') {
    return { message: `barocss-hub: not syncing with remote ${String(ev.peer)} (${String(ev.url)}): it is configured in this folder but was never used from this machine. Check it, then run \`barocss-hub remote trust ${String(ev.peer)}\` in a terminal.`, actions: ['Show Log'], severity: 'warning' };
  }
  return undefined;
}
