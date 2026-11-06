// Client-side task ORDER for the To Do widget ([S-54]).
//
// Neither backend can store this for us. Microsoft Graph's `todoTask` exposes no
// ordering property at all (the To Do apps' own hand-sorted order is not in the
// API), and Google Tasks only has a move endpoint for its own list. So a dragged
// order lives in the widget's config as an array of task ids, and the rows are
// arranged through it on the way to the screen.
//
// The rule is: ids the user has arranged come first, in that order; anything the
// order doesn't know about follows in the backend's own order. That is what puts
// a NEWLY created task at the BOTTOM ([S-55]) without a second mechanism — it is
// simply an id the order has never seen.
//
// On top of that arrangement, COMPLETED rows are sunk to the bottom ([S-64]) as
// a display-only pass — see `sinkCompleted`.

/** Narrow an untrusted config value to a validated id list. A non-array
 *  (absent/corrupt) yields `[]`; non-string entries are dropped rather than
 *  trusted. Never throws. */
export function parseTaskOrder(raw: unknown): string[] {
  if (!Array.isArray(raw)) return [];
  return raw.filter((v): v is string => typeof v === 'string');
}

/**
 * Arrange `rows` by `order`: every id the order lists (that is actually present)
 * first, then the rest in the order the backend returned them. An empty order
 * returns the input array unchanged — an untouched list keeps the server's
 * sorting, including Today's oldest-first ([S-52]).
 *
 * Ids in `order` that no longer exist are skipped, so a deleted task can't leave
 * a hole, and duplicates in a corrupt order can't duplicate a row.
 */
export function applyTaskOrder<T extends { id: string }>(rows: T[], order: readonly string[]): T[] {
  if (!order.length) return rows;
  const byId = new Map(rows.map(r => [r.id, r]));
  const out: T[] = [];
  const taken = new Set<string>();
  for (const id of order) {
    const row = byId.get(id);
    if (row && !taken.has(id)) { out.push(row); taken.add(id); }
  }
  for (const row of rows) {
    if (!taken.has(row.id)) out.push(row);
  }
  return out;
}

/**
 * Move `id` into `targetId`'s slot, keeping every other id's relative order.
 * Returns the INPUT array (same reference) when the move is a no-op or either id
 * is unknown, so a drag that comes home writes no config — the same contract as
 * the habit tracker's `moveHabit`, which this mirrors deliberately: the two
 * widgets' drags should not disagree about what "drop on that row" means.
 */
export function moveId(ids: readonly string[], id: string, targetId: string): readonly string[] {
  const from = ids.indexOf(id);
  const to = ids.indexOf(targetId);
  if (from < 0 || to < 0 || from === to) return ids;
  const next = ids.slice();
  const [moved] = next.splice(from, 1);
  next.splice(to, 0, moved);
  return next;
}

/** Index of the row whose box contains `y`, for the drag reorder. Above the
 *  first row or below the last one the nearest row wins, so a cursor that runs
 *  past either end still parks the task at that end instead of dropping out of
 *  the gesture. Returns -1 only for an empty list. Mirrors `rowIndexAtY` in the
 *  habit tracker. */
export function rowIndexAtY(rects: readonly { top: number; bottom: number }[], y: number): number {
  if (rects.length === 0) return -1;
  if (y <= rects[0].top) return 0;
  for (let i = 0; i < rects.length; i++) {
    if (y < rects[i].bottom) return i;
  }
  return rects.length - 1;
}

/** Do two id lists hold the same ids in the same order? The drag commits only
 *  when this is false. */
export function sameOrder(a: readonly string[], b: readonly string[]): boolean {
  return a.length === b.length && a.every((id, i) => id === b[i]);
}

/**
 * Sink every COMPLETED row to the bottom ([S-64]), keeping both groups in the
 * order they arrived in. A task that has been ticked off has stopped competing
 * for the eye with the ones still to do, so it belongs after them — the same
 * place Microsoft To Do and most task apps put it.
 *
 * This is a DISPLAY rule, not another storage write: it runs after the user's
 * arrangement ([S-54]) has been applied, so the dragged order of the ACTIVE
 * tasks survives untouched and a task that is un-ticked returns to the slot it
 * was arranged in. Ticking a box therefore costs no config write and no
 * cross-window broadcast — it just re-derives the list, like any other
 * reactive change.
 *
 * Stable on both sides: completed rows keep their relative order among
 * themselves, so a reorder inside the sunk group ([S-54]) still reads back the
 * way it was dragged. Returns the INPUT array when there is nothing to move
 * (no completed row, or the list is already sunk) so an unchanged list keeps
 * its identity for the memo.
 */
export function sinkCompleted<T extends { status: string }>(rows: T[]): T[] {
  let firstDone = -1;
  for (let i = 0; i < rows.length; i++) {
    if (rows[i].status === 'completed') { firstDone = i; break; }
  }
  if (firstDone < 0) return rows; // nothing completed
  const active: T[] = [];
  const done: T[] = [];
  for (const row of rows) (row.status === 'completed' ? done : active).push(row);
  // Every completed row already sits after every active one — leave it alone.
  if (firstDone === active.length) return rows;
  return [...active, ...done];
}

/**
 * Draw the rows named in `slots` at the index they were frozen at ([S-65]),
 * leaving every other row where the arrangement put it.
 *
 * This is the whole trick behind the tick's beat: ticking a box changes the
 * arrangement ([S-64]) in the same frame as the tick itself, so the row leaves
 * before the eye finds it. Freezing its slot holds it still for the beat, then
 * releasing the freeze lets it go — and because this is a rendering step, no
 * state, no config and no backend call is involved either way.
 *
 * Frozen slots are inserted in ascending order so one insertion cannot shift
 * where the next one belongs, and a slot past the end parks the row at the end
 * rather than vanishing. Ids that are no longer in `rows` are ignored.
 */
export function freezeAt<T extends { id: string }>(rows: T[], slots: ReadonlyMap<string, number>): T[] {
  if (!slots.size) return rows;
  const frozen = rows.filter(r => slots.has(r.id));
  if (!frozen.length) return rows;
  const out = rows.filter(r => !slots.has(r.id));
  frozen.sort((a, b) => slots.get(a.id)! - slots.get(b.id)!);
  for (const row of frozen) {
    const at = Math.min(Math.max(slots.get(row.id)!, 0), out.length);
    out.splice(at, 0, row);
  }
  return out;
}
