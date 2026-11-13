// Persistence + reactivity for the To Do widget's offline layer ([S-72]…[S-74]).
//
// Two separate keys, both written through `sharedStorage` so the grid tile, a
// pop-out and the config window all see the same queue:
//
//   todo-task-cache  the last snapshot the server gave us, per account+list
//   todo-outbox      changes made while the server was unreachable
//
// They are separate on purpose — a snapshot is refreshed on every successful
// poll, while the queue is drained only when a *write* succeeds. Keeping them in
// one blob would mean every successful poll rewrote the queue too, and a crash
// mid-write could take a pending change down with a snapshot it has nothing to
// do with.
//
// Both are schema'd through `persistedStore` (versioned envelope, loud fallback
// on corruption), like every other shared store in the app.

import { createStore } from 'solid-js/store';
import { onStorageKey, sharedStorage } from '../../../src/sync.ts';
import { logger } from '../../../src/debugLog.ts';
import { loadPersisted, savePersistedBare, type PersistedSchema } from '../../../src/persistedStore.ts';
import {
  parseCachedLists, parseOutbox, getSnapshot, putSnapshot, enqueueOp, dropOp, opsForList,
  TODO_CACHE_KEY, TODO_OUTBOX_KEY,
  type CachedList, type CachedTask, type PendingOp,
} from './offlineData.ts';

// Node-safe storage handle: unit tests import this module without a DOM.
const LS: Pick<Storage, 'getItem' | 'setItem'> =
  typeof localStorage !== 'undefined'
    ? localStorage
    : { getItem: () => null, setItem: () => {} };

const CACHE_SCHEMA: PersistedSchema<CachedList[]> = {
  version: 1,
  defaults: () => [],
  parse: parseCachedLists,
};

const OUTBOX_SCHEMA: PersistedSchema<PendingOp[]> = {
  version: 1,
  defaults: () => [],
  parse: parseOutbox,
};

/** How long an unchanged snapshot may go without being rewritten. Long enough
 *  that the once-a-minute poll is not a disk write every minute, short enough
 *  that `at` on disk (used after a restart) is never badly stale. */
const SNAPSHOT_REWRITE_MS = 5 * 60 * 1000;

function loadCache(): CachedList[] {
  return loadPersisted(TODO_CACHE_KEY, CACHE_SCHEMA, LS, {
    onCorrupt: (m) => logger.error('todoCache', m),
  });
}

function loadOutbox(): PendingOp[] {
  return loadPersisted(TODO_OUTBOX_KEY, OUTBOX_SCHEMA, LS, {
    onCorrupt: (m) => logger.error('todoOutbox', m),
  });
}

// A plain `createStore` assign (no `reconcile`) is enough here: nothing renders
// the arrays themselves — the widget reads a COUNT of the queue and looks
// snapshots up by key — so there is no row identity to preserve.
const [store, setStore] = createStore<{ cache: CachedList[]; ops: PendingOp[] }>({
  cache: loadCache(),
  ops: loadOutbox(),
});

/** The whole queue, device-wide. Every widget's pending changes live here: the
 *  first one to reach a live server drains it. */
export const outbox = (): PendingOp[] => store.ops;

/** The queued changes that belong to one list on one account — what a widget
 *  counts in its offline banner. */
export function pendingFor(accountId: string | undefined, listId: string | undefined): PendingOp[] {
  return opsForList(store.ops, accountId, listId);
}

/** The snapshot recorded for `key`, or undefined if that list was never loaded
 *  successfully on this device. */
export function readSnapshot(key: string) {
  return getSnapshot(store.cache, key);
}

/** Record the server's answer for `key` ([S-72]). Called only on a successful
 *  fetch — never with a snapshot we merely reproduced from the queue.
 *
 *  An unchanged list is not rewritten: the poll runs every refresh interval, and
 *  each write here is a localStorage write plus a cross-window broadcast (and
 *  `savePersistedBare` copies the old value to a backup first). Identical
 *  content therefore only advances `at` IN MEMORY, which is what the banner
 *  reports as "last synced"; the disk copy catches up on the next real change
 *  or after five minutes, whichever comes first. */
export function writeSnapshot(key: string, tasks: readonly CachedTask[]): void {
  const prev = getSnapshot(store.cache, key);
  const same = !!prev && prev.tasks.length === tasks.length
    && JSON.stringify(prev.tasks) === JSON.stringify(tasks);
  if (same && Date.now() - prev.at < SNAPSHOT_REWRITE_MS) {
    setStore('cache', putSnapshot(store.cache, key, tasks, Date.now()));
    return;
  }
  const next = putSnapshot(store.cache, key, tasks, Date.now());
  setStore('cache', next);
  savePersistedBare(TODO_CACHE_KEY, next, sharedStorage, {
    onError: (m) => logger.error('todoCache', m),
  });
}

/** Queue one offline change ([S-73]). */
export function queueOp(op: PendingOp): void {
  const next = enqueueOp(store.ops, op);
  setStore('ops', next);
  savePersistedBare(TODO_OUTBOX_KEY, next, sharedStorage, {
    onError: (m) => logger.error('todoOutbox', m),
  });
}

/** Forget one op — it reached the server (or can never do so). */
export function removeOp(id: string): void {
  const next = dropOp(store.ops, id);
  if (next.length === store.ops.length) return;
  setStore('ops', next);
  savePersistedBare(TODO_OUTBOX_KEY, next, sharedStorage, {
    onError: (m) => logger.error('todoOutbox', m),
  });
}

// Another window changed the queue or a snapshot: reload both. Cheap (two
// localStorage reads) and it happens only when a store was actually written.
function reload(): void {
  setStore({ cache: loadCache(), ops: loadOutbox() });
}
onStorageKey(TODO_CACHE_KEY, reload);
onStorageKey(TODO_OUTBOX_KEY, reload);
