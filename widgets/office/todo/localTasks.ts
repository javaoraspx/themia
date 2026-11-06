// Pure helpers for the To Do widget's LOCAL storage mode ([S-41]). Local tasks
// live in the widget's own config bag (per-instance, keyed by `_widgetId`) the
// same way Notes stores its text and Countdown its timers — so nothing here
// touches Tauri, IPC, or accounts, and every rule below is unit-tested.
//
// The stored shape is deliberately lean: an account task carries importance /
// due-date / list metadata a local task has no source for, so we keep only what
// the local UI can actually edit. `parseLocalTasks` owns the trust boundary
// (config values arrive as `unknown`), distinguishing "absent/corrupt" (→ empty
// list) from a single malformed entry (→ dropped, siblings kept).

export interface LocalTask {
  id: string;
  title: string;
  done: boolean;
}

/** Narrow an untrusted config value to a validated `LocalTask[]`. A non-array
 *  (missing or corrupt config) yields `[]`; individual entries missing a string
 *  `id`/`title` are dropped rather than trusted; a non-boolean `done` defaults
 *  to false. Never throws. */
export function parseLocalTasks(raw: unknown): LocalTask[] {
  if (!Array.isArray(raw)) return [];
  const out: LocalTask[] = [];
  for (const entry of raw) {
    if (entry === null || typeof entry !== 'object') continue;
    const o = entry as Record<string, unknown>;
    if (typeof o.id !== 'string' || typeof o.title !== 'string') continue;
    out.push({ id: o.id, title: o.title, done: o.done === true });
  }
  return out;
}

/** APPEND a new active task, so it lands where the user was typing — at the
 *  bottom of the list, under the "Add task" row ([S-55]). It used to prepend;
 *  a task typed at the bottom then jumped to the top, which read as the widget
 *  having added something else. A blank/whitespace title is a no-op that
 *  returns the input list unchanged. Immutable. */
export function addLocalTask(tasks: LocalTask[], title: string, id: string): LocalTask[] {
  const trimmed = title.trim();
  if (!trimmed) return tasks;
  return [...tasks, { id, title: trimmed, done: false }];
}

/** Flip the `done` flag of the task with `id`. Unknown id → equal list.
 *  Immutable. */
export function toggleLocalTask(tasks: LocalTask[], id: string): LocalTask[] {
  return tasks.map(t => (t.id === id ? { ...t, done: !t.done } : t));
}

/** Replace the title of the task with `id`. A blank/whitespace title is a no-op
 *  that returns the input list unchanged, so committing an emptied editor keeps
 *  the task rather than silently blanking it. Unknown id → equal list.
 *  Immutable. [S-50] */
export function renameLocalTask(tasks: LocalTask[], id: string, title: string): LocalTask[] {
  const trimmed = title.trim();
  if (!trimmed) return tasks;
  return tasks.map(t => (t.id === id ? { ...t, title: trimmed } : t));
}

/** Rewrite the stored order so the tasks named in `ids` appear in that order
 *  ([S-54]). In local mode the stored array IS the order — there is no separate
 *  order list that could drift from it.
 *
 *  Tasks the display was HIDING (completed, with `showCompleted` off — [S-46])
 *  are not in `ids`; they keep their absolute slot, and the dragged rows fill
 *  the slots around them. So a reorder can never resurrect, drop, or shuffle a
 *  task the user could not see. Unknown ids are ignored. Immutable. */
export function reorderLocalTasks(tasks: LocalTask[], ids: readonly string[]): LocalTask[] {
  const byId = new Map(tasks.map(t => [t.id, t]));
  const moving = ids.map(id => byId.get(id)).filter((t): t is LocalTask => !!t);
  const movingIds = new Set(moving.map(t => t.id));
  let next = 0;
  return tasks.map(t => (movingIds.has(t.id) ? moving[next++] : t));
}

/** Remove the task with `id`. Unknown id → equal list. Immutable. */
export function removeLocalTask(tasks: LocalTask[], id: string): LocalTask[] {
  return tasks.filter(t => t.id !== id);
}

/** The subset to render: completed tasks are hidden unless `showCompleted`.
 *  Order is preserved.
 *
 *  `keep` names tasks that must stay on screen even though they are completed —
 *  a row that is still settling after a tick is drawn while it fades out
 *  ([S-65]), and dropping it the instant it is ticked is exactly the "it
 *  vanished under the cursor" the settle exists to fix. */
export function visibleLocalTasks(
  tasks: LocalTask[],
  showCompleted: boolean,
  keep?: { has(id: string): boolean },
): LocalTask[] {
  if (showCompleted) return tasks;
  return tasks.filter(t => !t.done || (keep?.has(t.id) ?? false));
}
