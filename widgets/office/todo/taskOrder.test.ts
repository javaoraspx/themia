import { describe, it, expect } from 'vitest';
import {
  parseTaskOrder, applyTaskOrder, moveId, rowIndexAtY, sameOrder, sinkCompleted, freezeAt,
  countSettledCompleted, rowBand,
} from './taskOrder.ts';

const row = (id: string) => ({ id });
const task = (id: string, status = 'notStarted') => ({ id, status });

describe('parseTaskOrder [S-54]', () => {
  it('returns an empty list for a non-array', () => {
    expect(parseTaskOrder(undefined)).toEqual([]);
    expect(parseTaskOrder(null)).toEqual([]);
    expect(parseTaskOrder('a,b')).toEqual([]);
    expect(parseTaskOrder({ 0: 'a' })).toEqual([]);
  });

  it('drops non-string entries and keeps their siblings', () => {
    expect(parseTaskOrder(['a', 3, null, 'b', {}])).toEqual(['a', 'b']);
  });
});

describe('applyTaskOrder [S-54]', () => {
  it('leaves the backend order alone when nothing has been arranged', () => {
    const rows = [row('a'), row('b')];
    expect(applyTaskOrder(rows, [])).toBe(rows);
  });

  it('arranges known ids first, in the stored order', () => {
    const rows = [row('a'), row('b'), row('c')];
    expect(applyTaskOrder(rows, ['c', 'a', 'b']).map(r => r.id)).toEqual(['c', 'a', 'b']);
  });

  it('appends rows the order has never seen — a new task lands at the BOTTOM [S-55]', () => {
    const rows = [row('fresh'), row('a'), row('b')];
    expect(applyTaskOrder(rows, ['a', 'b']).map(r => r.id)).toEqual(['a', 'b', 'fresh']);
  });

  it('skips ids that no longer exist', () => {
    const rows = [row('a'), row('b')];
    expect(applyTaskOrder(rows, ['gone', 'b', 'a']).map(r => r.id)).toEqual(['b', 'a']);
  });

  it('cannot duplicate a row from a corrupt order', () => {
    const rows = [row('a'), row('b')];
    expect(applyTaskOrder(rows, ['a', 'a', 'b']).map(r => r.id)).toEqual(['a', 'b']);
  });
});

describe('moveId [S-54]', () => {
  it('moves an id into the target slot, keeping the rest in order', () => {
    expect(moveId(['a', 'b', 'c', 'd'], 'a', 'c')).toEqual(['b', 'c', 'a', 'd']);
  });

  it('moves upward too', () => {
    expect(moveId(['a', 'b', 'c'], 'c', 'a')).toEqual(['c', 'a', 'b']);
  });

  it('returns the same reference for a no-op move', () => {
    const ids = ['a', 'b'];
    expect(moveId(ids, 'a', 'a')).toBe(ids);
  });

  it('returns the same reference when either id is unknown', () => {
    const ids = ['a', 'b'];
    expect(moveId(ids, 'ghost', 'b')).toBe(ids);
    expect(moveId(ids, 'a', 'ghost')).toBe(ids);
  });
});

describe('rowIndexAtY [S-54]', () => {
  const rects = [
    { top: 0, bottom: 10 },
    { top: 10, bottom: 20 },
    { top: 20, bottom: 30 },
  ];

  it('resolves a point inside a row', () => {
    expect(rowIndexAtY(rects, 15)).toBe(1);
  });

  it('clamps above the first row and below the last', () => {
    expect(rowIndexAtY(rects, -50)).toBe(0);
    expect(rowIndexAtY(rects, 500)).toBe(2);
  });

  it('returns -1 for an empty list', () => {
    expect(rowIndexAtY([], 5)).toBe(-1);
  });
});

describe('sameOrder [S-54]', () => {
  it('is true only for the same ids in the same order', () => {
    expect(sameOrder(['a', 'b'], ['a', 'b'])).toBe(true);
    expect(sameOrder(['a', 'b'], ['b', 'a'])).toBe(false);
    expect(sameOrder(['a'], ['a', 'b'])).toBe(false);
  });
});

describe('sinkCompleted [S-64]', () => {
  it('moves completed rows below every active one, keeping both groups in order', () => {
    const rows = [task('a'), task('b', 'completed'), task('c'), task('d', 'completed')];
    expect(sinkCompleted(rows).map(r => r.id)).toEqual(['a', 'c', 'b', 'd']);
  });

  it('leaves a list with nothing completed untouched (same reference)', () => {
    const rows = [task('a'), task('b')];
    expect(sinkCompleted(rows)).toBe(rows);
  });

  it('leaves an already-sunk list untouched (same reference)', () => {
    const rows = [task('a'), task('b', 'completed'), task('c', 'completed')];
    expect(sinkCompleted(rows)).toBe(rows);
  });

  it('handles a fully completed list and an empty one', () => {
    const all = [task('a', 'completed'), task('b', 'completed')];
    expect(sinkCompleted(all).map(r => r.id)).toEqual(['a', 'b']);
    const empty: ReturnType<typeof task>[] = [];
    expect(sinkCompleted(empty)).toBe(empty);
  });

  it('runs AFTER the dragged arrangement, so the active rows keep that order [S-54]', () => {
    const rows = [task('a'), task('b', 'completed'), task('c')];
    const arranged = applyTaskOrder(rows, ['c', 'b', 'a']);
    expect(sinkCompleted(arranged).map(r => r.id)).toEqual(['c', 'a', 'b']);
  });

  it('puts a row that was just ticked below the ones still to do', () => {
    // What the toggle hands the row on the next derivation: same list, the
    // ticked row now reads `completed`.
    const before = [task('a'), task('b'), task('c')];
    const after = [task('a'), task('b', 'completed'), task('c')];
    expect(before.map(r => r.id)).toEqual(['a', 'b', 'c']);
    expect(sinkCompleted(after).map(r => r.id)).toEqual(['a', 'c', 'b']);
  });
});

describe('rowBand [S-66]', () => {
  it('puts a settled row in the half its status names', () => {
    expect(rowBand(task('a'), new Map())).toBe('active');
    expect(rowBand(task('a', 'completed'), new Map())).toBe('done');
  });

  // The tick flips `status` in the same frame as the beat starts [S-65], so the
  // frozen half is what keeps a just-completed row out of the finished group
  // (and an un-ticked row in it) until the beat is over.
  it('keeps a frozen row in the half it was ticked in', () => {
    const held = new Map([['a', { band: 'active' as const, index: 0 }]]);
    expect(rowBand(task('a', 'completed'), held)).toBe('active');
    const held2 = new Map([['b', { band: 'done' as const, index: 0 }]]);
    expect(rowBand(task('b'), held2)).toBe('done');
  });
});

describe('freezeAt [S-65]', () => {
  it('leaves the arrangement alone when nothing is frozen', () => {
    const rows = [task('a'), task('b')];
    expect(freezeAt(rows, new Map())).toBe(rows);
  });

  it('holds a just-completed row at the slot it was ticked in', () => {
    // Ticked 'b' in the middle: the sunk arrangement wants it last, the freeze
    // keeps it where the click was — inside the ACTIVE half — until the beat ends.
    const sunk = [task('a'), task('c'), task('b', 'completed')];
    const held = new Map([['b', { band: 'active' as const, index: 1 }]]);
    expect(freezeAt(sunk, held).map(r => r.id)).toEqual(['a', 'b', 'c']);
  });

  it('holds an un-ticked row in the finished half, below the add row', () => {
    // 'b' was the group's last row and has just been un-ticked. Its status no
    // longer says `completed`, but the beat has to keep it down there — the half
    // is carried by the freeze, not by the status.
    const arranged = [task('a'), task('b'), task('d', 'completed')];
    const held = new Map([['b', { band: 'done' as const, index: 1 }]]);
    expect(freezeAt(arranged, held).map(r => r.id)).toEqual(['a', 'd', 'b']);
    expect(rowBand(task('b'), held)).toBe('done');
  });

  it('parks a slot past the end at the end rather than dropping the row', () => {
    const rows = [task('a')];
    expect(freezeAt(rows, new Map([['a', { band: 'active' as const, index: 7 }]])).map(r => r.id)).toEqual(['a']);
    expect(freezeAt(rows, new Map([['a', { band: 'active' as const, index: -3 }]])).map(r => r.id)).toEqual(['a']);
  });

  it('ignores a frozen id that is no longer in the list', () => {
    const rows = [task('a'), task('b')];
    expect(freezeAt(rows, new Map([['gone', { band: 'active' as const, index: 0 }]])).map(r => r.id)).toEqual(['a', 'b']);
  });

  it('inserts several frozen rows in slot order', () => {
    const rows = [task('a'), task('d'), task('b'), task('c')];
    const held = new Map([
      ['b', { band: 'active' as const, index: 1 }],
      ['d', { band: 'active' as const, index: 2 }],
    ]);
    expect(freezeAt(rows, held).map(r => r.id)).toEqual(['a', 'b', 'd', 'c']);
  });

  it('is what makes a tick read as a tick: hold, then sink on release', () => {
    const ticked = [task('a'), task('b', 'completed'), task('c')];
    const held = new Map([['b', { band: 'active' as const, index: 1 }]]);
    expect(freezeAt(sinkCompleted(ticked), held).map(r => r.id)).toEqual(['a', 'b', 'c']);
    expect(freezeAt(sinkCompleted(ticked), new Map()).map(r => r.id)).toEqual(['a', 'c', 'b']);
  });
});

describe('countSettledCompleted [S-66]', () => {
  it('counts only completed rows', () => {
    const rows = [task('a'), task('b', 'completed'), task('c', 'completed')];
    expect(countSettledCompleted(rows, new Map())).toBe(2);
    expect(countSettledCompleted([], new Map())).toBe(0);
  });

  // The divider says how many HAVE arrived. A row still frozen at the slot it
  // was ticked in [S-65] has not arrived yet, so counting it would flash the
  // divider (and its number) in for the length of the beat.
  it('does not count a row that is still settling', () => {
    const rows = [task('a'), task('b', 'completed')];
    expect(countSettledCompleted(rows, new Map([['b', { band: 'active' as const, index: 1 }]]))).toBe(0);
    // Released: it joins the group as it glides down.
    expect(countSettledCompleted(rows, new Map())).toBe(1);
  });
});
