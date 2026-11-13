// [S-72]–[S-74] The offline layer's rules, in isolation.
//
// Everything here is about NOT losing a user's edit and NOT asking the server
// for something it cannot do: what a corrupt snapshot costs, which failure
// paints the cache, how a queue folds repeated edits of the same task, and how
// the queue is reconstructed on top of the last server answer.
import { describe, it, expect } from 'vitest';
import type { TodoTask } from './Todo.tsx';
import {
  cacheKey, parseCachedLists, getSnapshot, putSnapshot, CACHE_MAX_LISTS,
  parseOutbox, enqueueOp, dropOp, applyOps, opTarget, opsForList, opsForView,
  isServiceUnavailable, isRetryableFailure, errorHeadline,
  type CachedTask, type CreateOp, type PendingOp,
} from './offlineData.ts';

function task(id: string, over: Partial<CachedTask> = {}): CachedTask {
  return {
    id, title: id, status: 'notStarted', importance: 'normal',
    due_date: null, completed_at: null, list_id: 'L1', ...over,
  };
}

function op(over: Partial<Omit<CreateOp, 'kind'>> & { id: string }): CreateOp {
  return {
    kind: 'create', at: 1000, accountId: 'A', listId: 'L1',
    tempId: 'temp-1', title: 'typed offline', done: false, dueDate: null,
    ...over,
  };
}

describe('cache [S-72]', () => {
  it('keys a snapshot by account AND list', () => {
    expect(cacheKey('A', 'L1')).toBe('A|L1');
    expect(cacheKey('A', 'L1')).not.toBe(cacheKey('A', 'L2'));
    // A widget that has not picked a list yet must not read another's tasks.
    expect(cacheKey('A', undefined)).toBe('A|');
  });

  it('drops a corrupt entry but keeps its siblings', () => {
    const parsed = parseCachedLists([
      { key: 'A|L1', at: 5, tasks: [{ id: 't1', title: 'kept' }] },
      { at: 5, tasks: [] },                 // no key
      { key: '', at: 5, tasks: [] },        // empty key
      { key: 'A|L1', at: 9, tasks: [] },    // duplicate key, first wins
      null,
      'nonsense',
    ]);
    expect(parsed.map(l => l.key)).toEqual(['A|L1']);
    expect(parsed[0].tasks).toHaveLength(1);
  });

  it('fills a malformed task from defaults instead of trusting it', () => {
    const [list] = parseCachedLists([{
      key: 'A|L1', at: 5,
      tasks: [
        { id: 't1', title: 'ok', status: 'completed', importance: 'high', due_date: '2026-01-01', completed_at: null, list_id: 'L1' },
        { id: 't2' },                    // no title → dropped
        { title: 'no id' },              // no id → dropped
        { id: 't3', title: 'bare', status: 7, importance: null, list_id: 12 }, // wrong types → defaults
      ],
    }]);
    expect(list.tasks.map(t => t.id)).toEqual(['t1', 't3']);
    expect(list.tasks[1]).toEqual({
      id: 't3', title: 'bare', status: 'notStarted', importance: 'normal',
      due_date: null, completed_at: null, list_id: '',
    });
  });

  it('treats absent / corrupt storage as "no snapshot", never as an empty list', () => {
    expect(parseCachedLists(undefined)).toEqual([]);
    expect(parseCachedLists({ key: 'A|L1' })).toEqual([]);
    expect(parseCachedLists('A|L1')).toEqual([]);
  });

  it('replaces a list snapshot in place and moves it to the newest end', () => {
    let cache = putSnapshot([], 'A|L1', [task('t1')], 100);
    cache = putSnapshot(cache, 'A|L2', [task('t2')], 200);
    cache = putSnapshot(cache, 'A|L1', [task('t3')], 300);
    expect(cache.map(l => l.key)).toEqual(['A|L2', 'A|L1']);
    expect(getSnapshot(cache, 'A|L1')!.tasks.map(t => t.id)).toEqual(['t3']);
    expect(getSnapshot(cache, 'A|L1')!.at).toBe(300);
    expect(getSnapshot(cache, 'nope')).toBeUndefined();
  });

  it('evicts the oldest list rather than growing without bound', () => {
    let cache: ReturnType<typeof putSnapshot> = [];
    for (let i = 0; i <= CACHE_MAX_LISTS; i++) cache = putSnapshot(cache, `A|L${i}`, [], i);
    expect(cache).toHaveLength(CACHE_MAX_LISTS);
    expect(getSnapshot(cache, 'A|L0'), 'oldest snapshot goes first').toBeUndefined();
    expect(getSnapshot(cache, `A|L${CACHE_MAX_LISTS}`)).toBeDefined();
  });

  it('is shape-compatible with the task the widget renders', () => {
    // Type-level only: the component's own model and the cached copy must never
    // drift, or a snapshot would render as a different widget's rows.
    const cached: CachedTask = task('t1');
    const rendered: TodoTask = cached;
    const back: CachedTask = rendered;
    expect(back.id).toBe('t1');
  });
});

describe('outbox folding [S-73]', () => {
  it('drops a malformed op instead of repairing it', () => {
    const parsed = parseOutbox([
      op({ id: 'o1' }),
      { id: 'o2', kind: 'status' },                     // missing fields
      { id: 'o3', kind: 'nonsense', at: 1, accountId: 'A', listId: 'L1' },
      { id: '', kind: 'delete', at: 1, accountId: 'A', listId: 'L1', taskId: 't1' },
      { id: 'o4', kind: 'delete', at: 1, accountId: 'A', listId: 'L1', taskId: 't1' },
    ]);
    expect(parsed.map(o => o.id)).toEqual(['o1', 'o4']);
  });

  it('keeps one status op per task — the newest', () => {
    let ops = enqueueOp([], { id: 'o1', at: 1, kind: 'status', accountId: 'A', listId: 'L1', taskId: 't1', status: 'completed' });
    ops = enqueueOp(ops, { id: 'o2', at: 2, kind: 'status', accountId: 'A', listId: 'L1', taskId: 't1', status: 'notStarted' });
    expect(ops.map(o => o.id)).toEqual(['o2']);
  });

  // The two carry DIFFERENT fields. Folding "one op per task" would silently
  // drop half of what the user did while offline.
  it('keeps a status and a rename on the same task side by side', () => {
    let ops = enqueueOp([], { id: 'o1', at: 1, kind: 'status', accountId: 'A', listId: 'L1', taskId: 't1', status: 'completed' });
    ops = enqueueOp(ops, { id: 'o2', at: 2, kind: 'rename', accountId: 'A', listId: 'L1', taskId: 't1', title: 'new title' });
    expect(ops.map(o => `${o.kind}:${o.id}`)).toEqual(['status:o1', 'rename:o2']);
  });

  it('folds a tick and a rename into the create of a task the server has never seen', () => {
    let ops = enqueueOp([], op({ id: 'o1', tempId: 'temp-1', title: 'typed offline' }));
    ops = enqueueOp(ops, { id: 'o2', at: 2, kind: 'status', accountId: 'A', listId: 'L1', taskId: 'temp-1', status: 'completed' });
    ops = enqueueOp(ops, { id: 'o3', at: 3, kind: 'rename', accountId: 'A', listId: 'L1', taskId: 'temp-1', title: 'edited offline' });

    expect(ops).toHaveLength(1);
    const created = ops[0] as CreateOp;
    expect(created.kind).toBe('create');
    expect(created.done).toBe(true);
    expect(created.title).toBe('edited offline');
  });

  // Nothing was ever sent, so there is nothing to delete: sending a delete for a
  // temp id would 404 forever and block the queue behind it.
  it('forgets the create when a task typed offline is deleted before it is sent', () => {
    let ops = enqueueOp([], op({ id: 'o1', tempId: 'temp-1' }));
    ops = enqueueOp(ops, { id: 'o2', at: 2, kind: 'delete', accountId: 'A', listId: 'L1', taskId: 'temp-1' });
    expect(ops).toEqual([]);
  });

  it('lets a delete swallow every earlier op on the same task', () => {
    let ops = enqueueOp([], { id: 'o1', at: 1, kind: 'status', accountId: 'A', listId: 'L1', taskId: 't1', status: 'completed' });
    ops = enqueueOp(ops, { id: 'o2', at: 2, kind: 'rename', accountId: 'A', listId: 'L1', taskId: 't1', title: 'x' });
    ops = enqueueOp(ops, { id: 'o3', at: 3, kind: 'delete', accountId: 'A', listId: 'L1', taskId: 't1' });
    expect(ops.map(o => o.id)).toEqual(['o3']);
  });

  it('ignores a change arriving for a task that is already queued for deletion', () => {
    let ops = enqueueOp([], { id: 'o1', at: 1, kind: 'delete', accountId: 'A', listId: 'L1', taskId: 't1' });
    ops = enqueueOp(ops, { id: 'o2', at: 2, kind: 'rename', accountId: 'A', listId: 'L1', taskId: 't1', title: 'x' });
    expect(ops.map(o => o.id)).toEqual(['o1']);
  });

  it('drops an op by id, and is a no-op for an unknown one', () => {
    const ops = [op({ id: 'o1' })];
    expect(dropOp(ops, 'o1')).toEqual([]);
    expect(dropOp(ops, 'nope')).toEqual(ops);
  });

  it('groups the queue by account+list for the widget that owns it', () => {
    const ops = [
      op({ id: 'o1', listId: 'L1' }),
      op({ id: 'o2', listId: 'L2' }),
      op({ id: 'o3', accountId: 'B', listId: 'L1' }),
    ];
    expect(opsForList(ops, 'A', 'L1').map(o => o.id)).toEqual(['o1']);
    expect(opsForList(ops, 'B', 'L1').map(o => o.id)).toEqual(['o3']);
    expect(opsForList(ops, 'A', undefined)).toEqual([]);
  });

  // A create is the only op that can put a ROW on screen, so a device-wide queue
  // applied whole would drop a task typed in one widget into every other To Do
  // widget. The rest are matched by task id, which no other list shares — and
  // they must NOT be filtered by list, or Today ([S-52]) would lose every tick
  // made on a row that lives in one of the lists it aggregates.
  it('paints its own creates and every task-addressed op, whatever list it names', () => {
    const ops: PendingOp[] = [
      op({ id: 'o1', listId: 'L1' }),
      op({ id: 'o2', listId: 'L2' }),
      op({ id: 'o3', accountId: 'B', listId: 'L1' }),
      { id: 'o4', at: 4, kind: 'status', accountId: 'A', listId: 'L2', taskId: 't2', status: 'completed' },
      { id: 'o5', at: 5, kind: 'delete', accountId: 'A', listId: 'L1', taskId: 't1' },
    ];
    expect(opsForView(ops, 'A', 'L1').map(o => o.id)).toEqual(['o1', 'o4', 'o5']);
    expect(opsForView(ops, 'A', '__today__').map(o => o.id)).toEqual(['o4', 'o5']);
  });

  it('addresses a create by its temp id and everything else by its task id', () => {
    expect(opTarget(op({ id: 'o1', tempId: 'temp-9' }))).toBe('temp-9');
    expect(opTarget({ id: 'o2', at: 1, kind: 'delete', accountId: 'A', listId: 'L1', taskId: 't9' })).toBe('t9');
  });
});

describe('replaying the queue onto a snapshot [S-73]', () => {
  it('returns the snapshot untouched when nothing is queued', () => {
    const tasks = [task('t1')];
    const out = applyOps(tasks, []);
    expect(out).toEqual(tasks);
    expect(out).not.toBe(tasks); // a copy: the caller must not alias the cache
  });

  it('appends a create at the bottom, where the add row is', () => {
    const out = applyOps([task('t1')], [op({ id: 'o1', tempId: 'temp-1', title: 'new one' })]);
    expect(out.map(t => t.title)).toEqual(['t1', 'new one']);
    expect(out[1].list_id).toBe('L1');
    expect(out[1].status).toBe('notStarted');
  });

  it('replays status, rename and delete in queue order', () => {
    const ops: PendingOp[] = [
      { id: 'o1', at: 1, kind: 'status', accountId: 'A', listId: 'L1', taskId: 't1', status: 'completed' },
      { id: 'o2', at: 2, kind: 'rename', accountId: 'A', listId: 'L1', taskId: 't1', title: 'renamed' },
      { id: 'o3', at: 3, kind: 'delete', accountId: 'A', listId: 'L1', taskId: 't2' },
    ];
    const out = applyOps([task('t1'), task('t2')], ops);
    expect(out.map(t => t.id)).toEqual(['t1']);
    expect(out[0].title).toBe('renamed');
    expect(out[0].status).toBe('completed');
    expect(out[0].completed_at).not.toBeNull();
  });

  it('paints a folded "created, then ticked" as already completed', () => {
    const out = applyOps([], [op({ id: 'o1', tempId: 'temp-1', done: true, at: 5 })]);
    expect(out[0].status).toBe('completed');
    expect(out[0].completed_at).toBe(new Date(5).toISOString());
  });

  it('never mutates the snapshot it is given (the cache is the server\'s answer)', () => {
    const cache = [task('t1')];
    applyOps(cache, [
      { id: 'o1', at: 1, kind: 'status', accountId: 'A', listId: 'L1', taskId: 't1', status: 'completed' },
      op({ id: 'o2', tempId: 'temp-2' }),
    ]);
    expect(cache).toEqual([task('t1')]);
  });
});

describe('failure classification [S-72] [S-74]', () => {
  it('paints the cache only for a SERVICE failure', () => {
    expect(isServiceUnavailable('network_error')).toBe(true);
    expect(isServiceUnavailable('Graph API error 502')).toBe(true);
    expect(isServiceUnavailable('Graph API error 500')).toBe(true);
    expect(isServiceUnavailable('Google Tasks API error 503')).toBe(true);
    // Not the service being down: a stale copy of a deleted list would be a lie.
    expect(isServiceUnavailable('Graph API error 404')).toBe(false);
    expect(isServiceUnavailable('Graph API error 429')).toBe(false);
    expect(isServiceUnavailable('todo_not_authorized')).toBe(false);
    expect(isServiceUnavailable('Account not found')).toBe(false);
    expect(isServiceUnavailable('')).toBe(false);
  });

  it('keeps a queued change for anything that could still succeed', () => {
    expect(isRetryableFailure('network_error')).toBe(true);
    expect(isRetryableFailure('Graph API error 503')).toBe(true);
    expect(isRetryableFailure('Graph API error 429')).toBe(true);
    expect(isRetryableFailure('Graph API error 408')).toBe(true);
    expect(isRetryableFailure('todo_not_authorized')).toBe(true);
    expect(isRetryableFailure('Account not found')).toBe(true);
    // Unclassifiable: keep it. Losing the user's edit is the worse outcome.
    expect(isRetryableFailure('something we have never seen')).toBe(true);
    // A request the server will reject identically forever must not block the
    // queue for every later change.
    expect(isRetryableFailure('Graph API error 400')).toBe(false);
    expect(isRetryableFailure('Google Tasks API error 404')).toBe(false);
  });

  it('reads the headline off a value that carries a detail block', () => {
    expect(errorHeadline('Graph API error 502\nrequest: /x\nresponse: y')).toBe('Graph API error 502');
    expect(errorHeadline('network_error')).toBe('network_error');
  });
});
