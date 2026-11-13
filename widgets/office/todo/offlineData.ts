// The To Do widget's OFFLINE layer ([S-72]…[S-74]).
//
// Two pieces of state, both pure data here and both persisted by
// `offlineStore.ts`:
//
//  1. A CACHE of the last list the server actually handed us, per
//     account+list. It is what the widget paints when Graph/Google answers
//     502 — a desktop widget that blanks out because a remote API is having a
//     bad afternoon is worse than one showing yesterday's tasks, clearly
//     marked as such.
//  2. An OUTBOX of changes made while the server was unreachable. Every
//     mutation the user can perform offline becomes an op; ops are replayed in
//     order once a fetch succeeds again ([S-74]).
//
// The two are deliberately SEPARATE and the split is load-bearing: the cache
// holds the server's own answer, never the user's pending intent. The display
// is always `applyOps(cache, outbox)` — so a snapshot can be read any number of
// times without a queued "create" being appended twice, and a queued change
// survives even when the cache is refreshed underneath it.
//
// Nothing here imports Solid, Tauri or storage: this module owns the SHAPE and
// the RULES, so every one of them is unit-tested (`offlineData.test.ts`).

/** The task shape the backend returns, as far as the cache needs to reproduce
 *  it. Structurally identical to `TodoTask` in Todo.tsx — kept as its own type
 *  so this module (and its tests) never has to import a `.tsx` component. */
export interface CachedTask {
  id: string;
  title: string;
  status: string;
  importance: string;
  due_date: string | null;
  completed_at: string | null;
  list_id: string;
}

export interface CachedList {
  /** `accountId|listId` — see `cacheKey`. */
  key: string;
  /** When the snapshot was taken (ms). What the offline banner reports as
   *  "last synced". */
  at: number;
  tasks: CachedTask[];
}

/** Persisted keys. Declared here (not next to the store) so a node unit test
 *  can name them without pulling in Solid + the cross-window sync hub. */
export const TODO_CACHE_KEY = 'todo-task-cache';
export const TODO_OUTBOX_KEY = 'todo-outbox';

/** How many lists are kept. A device with a handful of widgets touches a
 *  handful of lists; the cap is what stops the store growing without bound as
 *  the user browses lists over months, and it is small enough that a full
 *  snapshot stays far below a localStorage quota. Eviction is oldest-first. */
export const CACHE_MAX_LISTS = 24;

/** The cache key for one list on one account. Both ids are interpolated even
 *  when absent, so two widgets that share a list share a snapshot and a widget
 *  that has not picked a list yet never collides with one that has. */
export function cacheKey(accountId: string | undefined, listId: string | undefined): string {
  return `${accountId ?? ''}|${listId ?? ''}`;
}

// ── Cache: parse + read + write ──────────────────────────────────────────────
// `parse*` are the trust boundary: what comes back from storage is `unknown`,
// and a corrupt entry must cost the user nothing more than losing a snapshot.

function parseCachedTask(raw: unknown): CachedTask | null {
  if (raw === null || typeof raw !== 'object') return null;
  const o = raw as Record<string, unknown>;
  if (typeof o.id !== 'string' || !o.id) return null;
  if (typeof o.title !== 'string') return null;
  return {
    id: o.id,
    title: o.title,
    status: typeof o.status === 'string' ? o.status : 'notStarted',
    importance: typeof o.importance === 'string' ? o.importance : 'normal',
    due_date: typeof o.due_date === 'string' ? o.due_date : null,
    completed_at: typeof o.completed_at === 'string' ? o.completed_at : null,
    list_id: typeof o.list_id === 'string' ? o.list_id : '',
  };
}

function parseCachedTasks(raw: unknown): CachedTask[] {
  if (!Array.isArray(raw)) return [];
  const out: CachedTask[] = [];
  for (const entry of raw) {
    const task = parseCachedTask(entry);
    if (task) out.push(task);
  }
  return out;
}

/** Narrow an untrusted stored value to a validated `CachedList[]`. A non-array
 *  (absent/corrupt) yields `[]`; entries missing a `key` are dropped, and a
 *  REPEATED key is dropped keeping the first, since every read is by key and a
 *  duplicate would make "what did the server last say" ambiguous. Never
 *  throws. */
export function parseCachedLists(raw: unknown): CachedList[] {
  if (!Array.isArray(raw)) return [];
  const out: CachedList[] = [];
  const seen = new Set<string>();
  for (const entry of raw) {
    if (entry === null || typeof entry !== 'object') continue;
    const o = entry as Record<string, unknown>;
    if (typeof o.key !== 'string' || !o.key) continue;
    if (seen.has(o.key)) continue;
    seen.add(o.key);
    out.push({
      key: o.key,
      at: typeof o.at === 'number' && isFinite(o.at) ? o.at : 0,
      tasks: parseCachedTasks(o.tasks),
    });
  }
  return out;
}

/** The snapshot for `key`, if one was ever taken. */
export function getSnapshot(lists: readonly CachedList[], key: string): CachedList | undefined {
  return lists.find(l => l.key === key);
}

/** Record what the server just returned, replacing any earlier snapshot of the
 *  same list and moving it to the NEWEST end. A new list evicts the oldest one
 *  once `max` is reached. Immutable. */
export function putSnapshot(
  lists: readonly CachedList[],
  key: string,
  tasks: readonly CachedTask[],
  at: number,
  max: number = CACHE_MAX_LISTS,
): CachedList[] {
  const next = lists.filter(l => l.key !== key);
  next.push({ key, at, tasks: tasks.map(t => ({ ...t })) });
  return next.length > max ? next.slice(next.length - max) : next;
}

// ── Outbox: the changes made while the server was away ───────────────────────
// Every op carries the account and list it belongs to, so a widget on one list
// can flush a change made on another (there is ONE queue per device, and the
// first widget to reach a live server drains it).

interface OpBase {
  /** Unique per op; the queue's identity for `dropOp`. */
  id: string;
  /** When the change was made (ms). Also the completion time a folded
   *  `done` paints onto its row. */
  at: number;
  accountId: string;
  /** The list the write must address — the task's own list, exactly like the
   *  online path (`writeListId`, [S-52]). */
  listId: string;
}

/** A task the user added while offline. `tempId` is the id it is displayed
 *  under until the server answers with a real one. */
export interface CreateOp extends OpBase {
  kind: 'create';
  tempId: string;
  title: string;
  /** Folded from later ticks on the same not-yet-sent task, so "add it, then
   *  tick it" is ONE create that already arrives completed. */
  done: boolean;
  dueDate: string | null;
}
export interface StatusOp extends OpBase {
  kind: 'status';
  taskId: string;
  status: string;
}
export interface RenameOp extends OpBase {
  kind: 'rename';
  taskId: string;
  title: string;
}
export interface DeleteOp extends OpBase {
  kind: 'delete';
  taskId: string;
}
export type PendingOp = CreateOp | StatusOp | RenameOp | DeleteOp;

/** The task an op is about — the temp id for a create, the server id otherwise. */
export function opTarget(op: PendingOp): string {
  return op.kind === 'create' ? op.tempId : op.taskId;
}

/** The queue entry for one list on one account, for the widget that owns it. */
export function opsForList(ops: readonly PendingOp[], accountId: string | undefined, listId: string | undefined): PendingOp[] {
  const key = cacheKey(accountId, listId);
  return ops.filter(op => cacheKey(op.accountId, op.listId) === key);
}

function isOp(x: unknown): x is PendingOp {
  if (x === null || typeof x !== 'object') return false;
  const o = x as Record<string, unknown>;
  if (typeof o.id !== 'string' || !o.id) return false;
  if (typeof o.at !== 'number' || !isFinite(o.at)) return false;
  if (typeof o.accountId !== 'string' || typeof o.listId !== 'string') return false;
  if (typeof o.kind !== 'string') return false;
  switch (o.kind) {
    case 'create':
      return typeof o.tempId === 'string' && !!o.tempId && typeof o.title === 'string'
        && typeof o.done === 'boolean'
        && (o.dueDate === null || typeof o.dueDate === 'string');
    case 'status':
      return typeof o.taskId === 'string' && !!o.taskId && typeof o.status === 'string';
    case 'rename':
      return typeof o.taskId === 'string' && !!o.taskId && typeof o.title === 'string';
    case 'delete':
      return typeof o.taskId === 'string' && !!o.taskId;
  }
  return false;
}

/** Narrow an untrusted stored value to a validated queue. A malformed op is
 *  DROPPED, never repaired: the queue is replayed against a live server, and a
 *  half-understood write is worse than a lost one. Never throws. */
export function parseOutbox(raw: unknown): PendingOp[] {
  if (!Array.isArray(raw)) return [];
  return raw.filter(isOp);
}

/** Add `op` to the queue, folding it into what is already there.
 *
 *  Three rules, each of them about not asking the server for something it has
 *  already been told or can no longer do:
 *
 *  - A task that only exists LOCALLY absorbs punctuation: ticking it folds
 *    `done` into its create, renaming folds the title, and deleting it drops
 *    the create entirely (the task never reached the server, so "delete" is
 *    just "forget it").
 *  - On a task the server already knows, one op PER KIND is kept — the newest.
 *    Ticking a box ten times offline is one status write, not ten — but a
 *    rename does NOT displace a pending status, because the two carry different
 *    fields and dropping either would silently lose half the user's edit.
 *  - A change arriving for a task that already has a queued delete is DROPPED:
 *    the delete wins, and updating a row that is about to be removed would only
 *    be undone a second later.
 *
 *  Immutable. */
export function enqueueOp(ops: readonly PendingOp[], op: PendingOp): PendingOp[] {
  const target = opTarget(op);
  const create = ops.find((o): o is CreateOp => o.kind === 'create' && o.tempId === target);

  if (op.kind === 'delete') {
    const rest = ops.filter(o => opTarget(o) !== target);
    return create ? rest : [...rest, op];
  }
  if (create) {
    // `op` here is a status or a rename (create and delete returned above).
    const folded: CreateOp = op.kind === 'status'
      ? { ...create, done: op.status === 'completed' }
      : { ...create, title: op.title };
    return ops.map(o => (o === create ? folded : o));
  }
  if (ops.some(o => o.kind === 'delete' && o.taskId === target)) return [...ops];
  const rest = ops.filter(o => !(o.kind === op.kind && opTarget(o) === target));
  return [...rest, op];
}

/** Drop one op by id. Unknown id → equal queue. */
export function dropOp(ops: readonly PendingOp[], id: string): PendingOp[] {
  return ops.filter(o => o.id !== id);
}

/** Reproduce `ops` on top of `tasks`, in queue order. This is the ONLY place a
 *  pending change reaches the screen: the cache stays the server's answer, and
 *  the display is "server + what we still owe it". A create APPENDS, so a task
 *  typed while offline lands at the bottom of the list exactly like one typed
 *  while online ([S-55]). Immutable. */
export function applyOps(tasks: readonly CachedTask[], ops: readonly PendingOp[]): CachedTask[] {
  if (!ops.length) return [...tasks];
  let out: CachedTask[] = tasks.map(t => ({ ...t }));
  for (const op of ops) {
    switch (op.kind) {
      case 'create': {
        const when = new Date(op.at).toISOString();
        out = [...out, {
          id: op.tempId,
          title: op.title,
          status: op.done ? 'completed' : 'notStarted',
          importance: 'normal',
          due_date: op.dueDate,
          completed_at: op.done ? when : null,
          list_id: op.listId,
        }];
        break;
      }
      case 'status': {
        const when = new Date(op.at).toISOString();
        out = out.map(t => (t.id === op.taskId
          ? { ...t, status: op.status, completed_at: op.status === 'completed' ? when : null }
          : t));
        break;
      }
      case 'rename':
        out = out.map(t => (t.id === op.taskId ? { ...t, title: op.title } : t));
        break;
      case 'delete':
        out = out.filter(t => t.id !== op.taskId);
        break;
    }
  }
  return out;
}

// ── Failure classification ───────────────────────────────────────────────────
// The widget has to answer two different questions about a failed request, and
// they are NOT the same question:
//
//  - "Should I paint the cache instead of an error?" ([S-72]) → only for a
//    server-side outage. A 404 means the list is gone, and showing a stale copy
//    of a deleted list would be a lie.
//  - "Should I keep this queued change for later?" ([S-74]) → for anything that
//    could succeed tomorrow, which includes throttling (429) and the server
//    being down. A 400 will fail identically forever.

/** Status-bearing headlines from both backends ("Graph API error 502",
 *  "Google Tasks API error 503"). The number is the only part we trust. */
function statusOf(headline: string): number | null {
  const m = /API error (\d{3})\b/.exec(headline);
  return m ? Number(m[1]) : null;
}

/** True when the failure is the SERVICE being unavailable (as opposed to the
 *  request being wrong or the account being gone) — the case the cached
 *  snapshot exists for. `network_error` is the backend's own transport sentinel
 *  ([S-63]); 5xx is the remote API failing on its side. */
export function isServiceUnavailable(headline: string): boolean {
  const h = headline.trim();
  if (h === 'network_error') return true;
  const status = statusOf(h);
  return status !== null && status >= 500 && status <= 599;
}

/** True when a failed op could succeed later, so the queue keeps it. A failure
 *  we cannot classify is KEPT: losing the user's edit is worse than retrying
 *  something that may be permanently broken, and the queue drains as soon as
 *  the server accepts it. */
export function isRetryableFailure(headline: string): boolean {
  const h = headline.trim();
  if (h === 'network_error') return true;
  if (h.includes('todo_not_authorized')) return true;
  if (h.startsWith('Account not found') || h === 'no_accounts') return true;
  const status = statusOf(h);
  if (status === null) return true;
  return status >= 500 || status === 408 || status === 429;
}

/** The headline of a backend error value, which may carry a `\n`-separated
 *  detail block ([S-70]). */
export function errorHeadline(raw: unknown): string {
  const s = String(raw);
  const nl = s.indexOf('\n');
  return nl < 0 ? s : s.slice(0, nl);
}
