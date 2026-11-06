import { describe, it, expect } from 'vitest';
import {
  parseLocalTasks,
  addLocalTask,
  toggleLocalTask,
  removeLocalTask,
  renameLocalTask,
  reorderLocalTasks,
  visibleLocalTasks,
  type LocalTask,
} from './localTasks.ts';

// The local-storage To Do path keeps its tasks in the widget's own config bag
// (an untrusted `unknown` at read time). These pure helpers own the trust
// boundary + every mutation, so they carry the tests. Tags mirror SPEC.md.

describe('parseLocalTasks [S-42]', () => {
  it('returns [] for non-array input (absent / corrupt config)', () => {
    expect(parseLocalTasks(undefined)).toEqual([]);
    expect(parseLocalTasks(null)).toEqual([]);
    expect(parseLocalTasks('nope')).toEqual([]);
    expect(parseLocalTasks({ tasks: [] })).toEqual([]);
    expect(parseLocalTasks(42)).toEqual([]);
  });

  it('round-trips well-formed entries (id, title, done)', () => {
    const raw = [
      { id: 'a', title: 'Buy milk', done: false },
      { id: 'b', title: 'Call bank', done: true },
    ];
    expect(parseLocalTasks(raw)).toEqual([
      { id: 'a', title: 'Buy milk', done: false },
      { id: 'b', title: 'Call bank', done: true },
    ]);
  });

  it('defaults a missing/invalid `done` to false without dropping the task', () => {
    const raw = [
      { id: 'a', title: 'no done field' },
      { id: 'b', title: 'string done', done: 'yes' },
    ];
    expect(parseLocalTasks(raw)).toEqual([
      { id: 'a', title: 'no done field', done: false },
      { id: 'b', title: 'string done', done: false },
    ]);
  });

  it('drops malformed entries (missing/wrong-typed id or title) but keeps valid siblings', () => {
    const raw = [
      { id: 'ok', title: 'keep me', done: false },
      { id: 123, title: 'numeric id' },
      { id: 'x' }, // no title
      { title: 'no id' },
      null,
      'string-entry',
    ];
    expect(parseLocalTasks(raw)).toEqual([
      { id: 'ok', title: 'keep me', done: false },
    ]);
  });

  it('ignores unknown extra fields', () => {
    const raw = [{ id: 'a', title: 'T', done: true, importance: 'high', junk: 1 }];
    expect(parseLocalTasks(raw)).toEqual([{ id: 'a', title: 'T', done: true }]);
  });
});

describe('addLocalTask [S-43]', () => {
  const base: LocalTask[] = [{ id: 'a', title: 'A', done: false }];

  it('APPENDS a new active task at the bottom, trimming the title [S-55]', () => {
    const next = addLocalTask(base, '  New task  ', 'new-1');
    expect(next).toEqual([
      { id: 'a', title: 'A', done: false },
      { id: 'new-1', title: 'New task', done: false },
    ]);
  });

  it('returns the same list unchanged for an empty / whitespace-only title', () => {
    expect(addLocalTask(base, '', 'x')).toEqual(base);
    expect(addLocalTask(base, '   ', 'x')).toEqual(base);
  });

  it('does not mutate the input array', () => {
    const input: LocalTask[] = [{ id: 'a', title: 'A', done: false }];
    addLocalTask(input, 'B', 'b');
    expect(input).toEqual([{ id: 'a', title: 'A', done: false }]);
  });
});

describe('reorderLocalTasks [S-54]', () => {
  const base: LocalTask[] = [
    { id: 'a', title: 'A', done: false },
    { id: 'b', title: 'B', done: false },
    { id: 'c', title: 'C', done: false },
  ];

  it('rewrites the stored order to the dragged one', () => {
    expect(reorderLocalTasks(base, ['c', 'a', 'b']).map(t => t.id)).toEqual(['c', 'a', 'b']);
  });

  it('leaves a hidden completed task in its own slot [S-46]', () => {
    const withDone: LocalTask[] = [
      { id: 'a', title: 'A', done: false },
      { id: 'done', title: 'Done', done: true },
      { id: 'b', title: 'B', done: false },
    ];
    // Only the two visible ids were dragged; the completed one keeps index 1.
    expect(reorderLocalTasks(withDone, ['b', 'a']).map(t => t.id)).toEqual(['b', 'done', 'a']);
  });

  it('ignores ids that are not in the list', () => {
    expect(reorderLocalTasks(base, ['ghost', 'c', 'a', 'b']).map(t => t.id)).toEqual(['c', 'a', 'b']);
  });

  it('keeps every task when the order names only some of them', () => {
    expect(reorderLocalTasks(base, ['c']).map(t => t.id)).toEqual(['a', 'b', 'c']);
  });

  it('does not mutate the input array', () => {
    const input = base.slice();
    reorderLocalTasks(input, ['c', 'b', 'a']);
    expect(input.map(t => t.id)).toEqual(['a', 'b', 'c']);
  });
});

describe('toggleLocalTask [S-44]', () => {
  const base: LocalTask[] = [
    { id: 'a', title: 'A', done: false },
    { id: 'b', title: 'B', done: true },
  ];

  it('flips done for the matching id and leaves others untouched', () => {
    expect(toggleLocalTask(base, 'a')).toEqual([
      { id: 'a', title: 'A', done: true },
      { id: 'b', title: 'B', done: true },
    ]);
  });

  it('returns an equal list for an unknown id', () => {
    expect(toggleLocalTask(base, 'zzz')).toEqual(base);
  });

  it('does not mutate the input array', () => {
    const input: LocalTask[] = [{ id: 'a', title: 'A', done: false }];
    toggleLocalTask(input, 'a');
    expect(input).toEqual([{ id: 'a', title: 'A', done: false }]);
  });
});

describe('removeLocalTask [S-45]', () => {
  const base: LocalTask[] = [
    { id: 'a', title: 'A', done: false },
    { id: 'b', title: 'B', done: false },
  ];

  it('drops the matching task', () => {
    expect(removeLocalTask(base, 'a')).toEqual([{ id: 'b', title: 'B', done: false }]);
  });

  it('returns an equal list for an unknown id', () => {
    expect(removeLocalTask(base, 'zzz')).toEqual(base);
  });
});

describe('renameLocalTask [S-50]', () => {
  const base: LocalTask[] = [
    { id: 'a', title: 'A', done: false },
    { id: 'b', title: 'B', done: true },
  ];

  it('replaces the title of the matching task, trimmed', () => {
    expect(renameLocalTask(base, 'a', '  Renamed  ')).toEqual([
      { id: 'a', title: 'Renamed', done: false },
      { id: 'b', title: 'B', done: true },
    ]);
  });

  it('keeps the done flag and every sibling untouched', () => {
    expect(renameLocalTask(base, 'b', 'Still done')).toEqual([
      { id: 'a', title: 'A', done: false },
      { id: 'b', title: 'Still done', done: true },
    ]);
  });

  // Committing an emptied editor must not blank (or lose) the task: deleting is
  // the row menu's job, and a task with no title is unrecoverable from the UI.
  it('is a no-op for a blank or whitespace-only title', () => {
    expect(renameLocalTask(base, 'a', '')).toBe(base);
    expect(renameLocalTask(base, 'a', '   ')).toBe(base);
  });

  it('returns an equal list for an unknown id', () => {
    expect(renameLocalTask(base, 'zzz', 'X')).toEqual(base);
  });

  it('does not mutate the input', () => {
    const input: LocalTask[] = [{ id: 'a', title: 'A', done: false }];
    renameLocalTask(input, 'a', 'B');
    expect(input).toEqual([{ id: 'a', title: 'A', done: false }]);
  });
});

describe('visibleLocalTasks [S-46]', () => {
  const base: LocalTask[] = [
    { id: 'a', title: 'A', done: false },
    { id: 'b', title: 'B', done: true },
    { id: 'c', title: 'C', done: false },
  ];

  it('hides completed tasks when showCompleted is false, preserving order', () => {
    expect(visibleLocalTasks(base, false)).toEqual([
      { id: 'a', title: 'A', done: false },
      { id: 'c', title: 'C', done: false },
    ]);
  });

  it('shows every task when showCompleted is true', () => {
    expect(visibleLocalTasks(base, true)).toEqual(base);
  });

  // [S-65] A row that has just been ticked is still on screen for its beat,
  // even though completed rows are hidden — otherwise the tick would be
  // invisible and the row would look like it vanished under the cursor.
  it('keeps a task named in `keep` while it is settling', () => {
    expect(visibleLocalTasks(base, false, new Set(['b'])).map(t => t.id)).toEqual(['a', 'b', 'c']);
    expect(visibleLocalTasks(base, false, new Map([['b', 1]])).map(t => t.id)).toEqual(['a', 'b', 'c']);
  });

  it('drops a settling task again as soon as it is not kept', () => {
    expect(visibleLocalTasks(base, false, new Set()).map(t => t.id)).toEqual(['a', 'c']);
  });
});
