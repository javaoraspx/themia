import { createSignal, createMemo, createEffect, onCleanup, onMount, untrack } from 'solid-js';
import { createPolling } from '../../../src/createPolling';
import { Show, For } from 'solid-js';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { invoke, isDemo } from '../../../src/demo/invoke.ts';
import { CheckSquare, Square, RefreshCw, Circle, CheckCircle2, Plus, Trash2, Pencil } from 'lucide-solid';
import { showCtxMenu } from '../../../src/ctxMenuStore.ts';
import { registerWidgetKeyboard, type WidgetKeyEvent } from '../../../src/input/widgetKeyboard.ts';
import { onAppBlur } from '../../../src/input/appBlur.ts';
import { caretOffsetFromPoint, focusWithCaret } from '../../../src/input/caret.ts';
import { pastDragThreshold } from '../../office/folder/gesture.ts';
import { t, lang } from '../../../src/i18n.ts';
import WidgetHeader from '../../../src/components/WidgetHeader.tsx';
import type { HeaderMode } from '../../../src/widgets/registry.tsx';
import { useLayout } from '../../../src/viewContext.tsx';
import {
  parseLocalTasks, addLocalTask, toggleLocalTask, removeLocalTask, renameLocalTask,
  reorderLocalTasks, visibleLocalTasks, type LocalTask,
} from './localTasks.ts';
import { findList } from './localLists.ts';
import { localLists, setLocalListTasks, seedLocalList } from './localListStore.ts';
import { parseTaskOrder, applyTaskOrder, moveId, rowIndexAtY, sameOrder, sinkCompleted } from './taskOrder.ts';
import { resolveDue, isDuePast } from './dueDate.ts';
import { isTodayList, todayTasks, todayDueDate } from './todayList.ts';
import { clampTitleLines } from './titleLines.ts';
import './Todo.css';

export interface TodoProps {
  accountIds?: string[];
  accountId?: string;
  listId?: string;
  listName?: string;
  viewMode?: string;
  showCompleted?: boolean;
  refreshInterval?: number;
  /** [S-49] How many wrapped lines of a task title to show before clamping.
   *  Untrusted config value — narrowed through `clampTitleLines`. */
  titleLines?: number;
  label?: string;
  headerMode?: HeaderMode;
  /** 'account' (Microsoft/Google, the default) or 'local' (tasks stored on this
   *  device, in the shared list pool). [S-41] */
  storageMode?: string;
  /** Which device list this widget shows ([S-57]). An id into the device-wide
   *  pool, not the tasks themselves. */
  localListId?: string;
  /** LEGACY local-mode task array, from before device lists existed. Read once
   *  and migrated into a real list ([S-59]); never written again. Untrusted
   *  config value — validated via parseLocalTasks at the trust boundary. [S-42] */
  localTasks?: unknown;
  /** [S-54] Account-mode row order, as task ids. Untrusted config value —
   *  validated via `parseTaskOrder`. Local mode has no use for it: there the
   *  stored `localTasks` array IS the order. */
  taskOrder?: unknown;
  /** Stable per-instance id, injected by WidgetGrid/WidgetWindow. The write key
   *  for local tasks, so each placed widget owns an independent list. [S-41] */
  _widgetId?: string;
  _refresh?: number;
}

interface TodoTask {
  id: string;
  title: string;
  status: string;
  importance: string;
  due_date: string | null;
  completed_at: string | null;
  list_id: string;
}

// [S-09] Both providers encode the due *day* as a datetime; dueDate.ts owns
// which day that is (#101), this only translates the verdict.
function formatDue(iso: string | null): string {
  const due = resolveDue(iso);
  if (!due) return '';
  switch (due.label) {
    case 'overdue':  return t('todo.overdue');
    case 'today':    return t('todo.today');
    case 'tomorrow': return t('todo.tomorrow');
    case 'weekday':  return due.day.toLocaleDateString(lang(), { weekday: 'short' });
    case 'date':     return due.day.toLocaleDateString(lang(), { month: 'short', day: 'numeric' });
  }
}

function duePast(iso: string | null): boolean {
  return isDuePast(iso);
}

/** [S-50] Size the title editor to its content, so a long title is fully
 *  visible while it is being edited (the display clamps, the editor never
 *  does). Reset to `auto` first or the box can only ever grow. */
function growToFit(el: HTMLTextAreaElement): void {
  el.style.height = 'auto';
  el.style.height = `${el.scrollHeight}px`;
}

export default function Todo(props: TodoProps) {
  const layoutStore = useLayout();
  const [state, setState] = createSignal<'loading' | 'loaded' | 'empty' | 'nolist' | 'noauth' | 'error'>('loading');
  const [tasks, setTasks] = createSignal<TodoTask[]>([]);
  const [error, setError] = createSignal<string | null>(null);
  const [canScrollDown, setCanScrollDown] = createSignal(false);
  const [creating, setCreating] = createSignal(false);
  const [newTitle, setNewTitle] = createSignal('');
  const [focusedIdx, setFocusedIdx] = createSignal<number>(-1);
  // [S-50] Which task is being renamed, and the draft title. Only the id is
  // tracked, not the task object, so a poll that replaces the task list mid-edit
  // can't strand the editor on a stale row.
  const [editingId, setEditingId] = createSignal<string | null>(null);
  const [editTitle, setEditTitle] = createSignal('');

  let listEl: HTMLDivElement | undefined;
  // Request-id guard: each load() bumps this before awaiting; a slower in-flight
  // poll for the previously-selected list then bails instead of clobbering the
  // tasks of the newly-selected one (#31). Mirrors the `seq` guard in stocks/
  // SymbolSearch.
  let loadSeq = 0;
  // [S-63] Request id of the fetch currently out, or null. A POLL tick skips
  // while one is out; a selection change still starts one (the request-id guard
  // above sorts out which result wins). Holding the id rather than a flag keeps
  // it self-healing: a superseded load only clears the guard if IT is still the
  // one out, and a load that returns before fetching (no account, no list) never
  // strands it.
  let inFlightSeq: number | null = null;

  const effectiveIds = createMemo(() => {
    const ids = props.accountIds;
    const id = props.accountId;
    return ids?.length ? ids : id ? [id] : [];
  });

  const viewMode = () => props.viewMode ?? 'list';
  const showCompleted = () => props.showCompleted ?? false;
  const titleLines = () => clampTitleLines(props.titleLines);
  const refreshInterval = () => (props.refreshInterval ?? 60) * 1000;

  // ── Local storage mode ([S-41], [S-57]) ────────────────────────────────────
  // In local mode the tasks come from the device-wide list pool, selected by the
  // `localListId` in this widget's config — so the list is still a pure
  // derivation of state the widget doesn't own, with no account, fetch or
  // polling. The store is shared and cross-window synced, which keeps the grid
  // tile, a pop-out and the config window showing the same thing.
  const isLocal = () => props.storageMode === 'local';
  const activeLocalList = createMemo(() => (isLocal() ? findList(localLists(), props.localListId) : undefined));
  const localList = createMemo(() => activeLocalList()?.tasks ?? []);

  // ── Today, the virtual cross-list view ([S-52]) ────────────────────────────
  const isToday = () => !isLocal() && isTodayList(props.listId);
  // Today is the widget's own list, not the account's, so its name comes from
  // the current language rather than the `listName` frozen into config at pick
  // time — which would stay in the language it was picked in. A device list's
  // name comes from the pool ([S-57]), so renaming it in the config retitles
  // every widget showing it, live.
  const title = createMemo(() =>
    props.label
    || (isLocal() ? activeLocalList()?.name : isToday() ? t('todo.todayList') : props.listName)
    || t('todo.defaultTitle'),
  );

  function localToDisplay(tk: LocalTask): TodoTask {
    return {
      id: tk.id, title: tk.title,
      status: tk.done ? 'completed' : 'notStarted',
      importance: 'normal', due_date: null, completed_at: null, list_id: 'local',
    };
  }

  // The single list the UI renders + navigates: local-derived rows in local
  // mode, the fetched account signal otherwise — narrowed to what is due when
  // the selected list is the virtual Today one ([S-52]). The backend hands
  // Today every list's tasks precisely so the day filter can live here, next to
  // the day resolution it depends on ([S-48]).
  // [S-54] …then arranged by the user's own order. Local rows are already in
  // stored order (the array IS the order); account rows go through the id list
  // in config, which also puts a task the order has never seen — a brand new
  // one — at the BOTTOM ([S-55]).
  // [S-64] …and finally SUNK: completed tasks are shown after every active one,
  // in both modes. A display rule, so ticking a box writes nothing and the
  // arrangement of the active tasks (and a task's own slot when it is un-ticked)
  // survives. Local mode reaches it through `localToDisplay`, which maps `done`
  // onto the same `status` this reads.
  const taskOrder = createMemo(() => parseTaskOrder(props.taskOrder));
  const displayTasks = createMemo<TodoTask[]>(() => {
    if (isLocal()) return sinkCompleted(visibleLocalTasks(localList(), showCompleted()).map(localToDisplay));
    const rows = isToday() ? todayTasks(tasks()) : tasks();
    return sinkCompleted(applyTaskOrder(rows, taskOrder()));
  });

  // Which render block to show. A local widget with a list is always "loaded"
  // (an empty list still shows the add-task row); one whose list is gone or
  // never picked shows the same "pick a list" prompt account mode uses ([S-58]),
  // rather than silently adopting some other list's tasks. Account mode follows
  // the fetch state machine.
  const view = createMemo(() =>
    isLocal() ? (activeLocalList() ? 'loaded' as const : 'nolist' as const) : state(),
  );

  /** Merge a patch into this widget instance's config. `updateWidgetConfig`
   *  REPLACES the bag wholesale, so the current config is spread first to
   *  preserve unrelated keys (viewMode, showCompleted, storageMode, label…). */
  function writeConfig(patch: Record<string, unknown>): void {
    const id = props._widgetId;
    if (!id) return;
    const current = layoutStore.layout.widgets.find(w => w.id === id)?.config ?? {};
    layoutStore.updateWidgetConfig(id, { ...current, ...patch });
  }

  /** The single write path for every local task mutation: it lands in the
   *  SELECTED device list ([S-57]). With no list selected there is nothing on
   *  screen to mutate, so this is a no-op rather than a write that would have to
   *  invent a list. */
  function writeLocal(next: LocalTask[]): void {
    const id = activeLocalList()?.id;
    if (!id) return;
    setLocalListTasks(id, next);
  }

  // [S-59] One-shot migration of the pre-pool `localTasks` config bag into a
  // real device list. The new list's id is DERIVED from the widget id, so the
  // grid tile and a pop-out of the same widget — which mount at the same time
  // and both see the legacy bag — converge on one list instead of seeding two;
  // `seedLocalList` then leaves it alone if it is already there. The bag is
  // cleared in the same config write that points at the new list, so the tasks
  // exist in exactly one place afterwards and this can never run twice.
  createEffect(() => {
    if (!isLocal() || props.localListId) return;
    const legacy = parseLocalTasks(props.localTasks);
    if (!legacy.length) return;
    const widgetId = props._widgetId;
    if (!widgetId) return;
    untrack(() => {
      const listId = `list-migrated-${widgetId}`;
      seedLocalList(listId, props.label || t('todo.localLists.defaultName'), legacy);
      writeConfig({ localListId: listId, localTasks: [] });
    });
  });

  const activeCount = createMemo(() => displayTasks().filter(tk => tk.status !== 'completed').length);

  function checkScroll() {
    if (!listEl) return;
    setCanScrollDown(listEl.scrollHeight - listEl.scrollTop - listEl.clientHeight > 4);
  }

  createEffect(() => {
    void displayTasks().length;
    queueMicrotask(checkScroll);
  });

  async function load() {
    // Local mode never fetches: the list is a pure derivation of config. Guard
    // here so the polling timer + reload effect below both no-op in local mode.
    if (isLocal()) return;
    // Bump the request id up front so any in-flight poll for the old list
    // becomes stale the moment a fresh load starts (#31).
    const my = ++loadSeq;
    let ids = effectiveIds();
    let listId = props.listId;
    if (!ids.length && isDemo()) ids = ['demo-account'];
    if (!listId && isDemo()) listId = 'demo-list';
    if (!ids.length) {
      setState('empty');
      return;
    }
    if (!listId) {
      setState('nolist');
      return;
    }

    inFlightSeq = my;
    try {
      const result = await invoke('todo_get_tasks', {
        accountId: ids[0],
        listId,
        showCompleted: showCompleted(),
      });
      if (my !== loadSeq) return;
      setTasks(result);
      setState('loaded');
    } catch (e) {
      if (my !== loadSeq) return;
      const err = String(e);
      if (err === 'todo_not_authorized') {
        setState('noauth');
      } else if (err.includes('Account not found') || err === 'no_accounts') {
        setState(ids.length ? 'nolist' : 'empty');
      } else {
        // [S-63] The backend's transport-failure sentinel is the one error the
        // user can act on, so it gets a sentence in their language instead of
        // reqwest's URL-shaped one.
        setError(err === 'network_error' ? t('todo.networkError') : err);
        setState('error');
      }
    } finally {
      if (inFlightSeq === my) inFlightSeq = null;
    }
  }

  createEffect(() => {
    void effectiveIds();
    void props.listId;
    void showCompleted();
    void props._refresh;
    if (isLocal()) return; // reads props.storageMode → re-runs (and loads) on switch to account
    untrack(() => load());
  });

  // [S-63] A poll never stacks on top of the previous one. `createPolling` is a
  // plain `setInterval`, so it does not wait for `load()`: when a Today fan-out
  // ([S-52], one request per list) outlasts the interval — which is exactly what
  // a throttled Graph makes it do — every tick fired ANOTHER round of requests
  // at the server already rate-limiting us, and the throttle could never clear.
  // The request-id guard ([S-40]) discards the stale RESULT; the request had
  // already gone out. Skipping the tick is what keeps the load off the wire.
  createPolling(() => { if (inFlightSeq === null) void load(); }, refreshInterval, 'todo');

  /** The list a write about `task` must address. Every backend write is scoped
   *  to a list, and under Today the rows come from many lists — so the task's
   *  own list wins over the selected one, which there is only a virtual id
   *  ([S-52]). Falls back to the selection for rows created optimistically
   *  before the server answered. */
  const writeListId = (task: TodoTask) => task.list_id || props.listId || '';

  async function toggleTask(task: TodoTask) {
    if (isLocal()) { writeLocal(toggleLocalTask(localList(), task.id)); return; }
    const newStatus = task.status === 'completed' ? 'notStarted' : 'completed';
    // Optimistic update
    setTasks(prev => prev.map(t => t.id === task.id ? { ...t, status: newStatus } : t));

    try {
      await invoke('todo_update_task', {
        accountId: effectiveIds()[0],
        listId: writeListId(task),
        taskId: task.id,
        status: newStatus,
      });
      // If hiding completed and we just completed a task, remove it after a brief delay
      if (!showCompleted() && newStatus === 'completed') {
        setTimeout(() => {
          setTasks(prev => prev.filter(t => t.id !== task.id));
        }, 600);
      }
    } catch {
      // Revert
      setTasks(prev => prev.map(t => t.id === task.id ? { ...t, status: task.status } : t));
    }
  }

  // ── Drag to reorder ([S-54]) ───────────────────────────────────────────────
  // Driven by POLLING the OS cursor, not by mouse events: widget windows are
  // WS_EX_NOACTIVATE, so Chromium withholds mousemove while a button is held,
  // and Rust owns the window's OLE drop target, so WebView2 never delivers
  // `dragover` to the DOM either. Same mechanism the folder, shortcut and habit
  // widgets use, reusing that gesture module's 5px threshold so a click on a
  // title still opens the editor ([S-50]).
  //
  // The drag reorders a LOCAL copy and commits once on release: persisting every
  // hop would be a config write plus a cross-window broadcast each time, and an
  // interrupted drag would leave it half-applied.
  interface CursorState { x: number; y: number; left_down: boolean; ctrl_down: boolean }

  const rowEls = new Map<string, HTMLElement>();
  const [dragId, setDragId] = createSignal<string | null>(null);
  const [dragRows, setDragRows] = createSignal<TodoTask[] | null>(null);
  /** What the list renders: the in-flight arrangement while dragging, else the
   *  ordered display list. */
  const rows = () => dragRows() ?? displayTasks();

  let press: { id: string; x: number; y: number } | null = null;
  let polling = false;
  let generation = 0;
  /** A real drag ran; consumed by the click that follows the release so the
   *  gesture that ends on a title cannot also open the editor. */
  let dragRan = false;

  // Physical-pixel origin of this window, to turn GetCursorPos into client px.
  let winOriginPx = { x: 0, y: 0 };
  try {
    getCurrentWindow().innerPosition()
      .then((p) => { winOriginPx = { x: p.x, y: p.y }; })
      .catch(() => {});
  } catch {
    // ignore: no Tauri window in the picker preview / dev harness
  }

  function physToClient(px: number, py: number): [number, number] {
    const dpr = window.devicePixelRatio || 1;
    return [(px - winOriginPx.x) / dpr, (py - winOriginPx.y) / dpr];
  }

  function resetDrag(): void {
    press = null;
    polling = false;
    generation++;
    setDragId(null);
    setDragRows(null);
  }

  function commitDrag(): void {
    const next = dragRows();
    resetDrag();
    if (!next) return;
    const nextIds = next.map(tk => tk.id);
    if (sameOrder(nextIds, displayTasks().map(tk => tk.id))) return; // came home
    if (isLocal()) {
      // Local mode stores the order in the task array itself, so the drag is
      // replayed against the FULL stored list — the rendered one may be hiding
      // completed tasks ([S-46]).
      writeLocal(reorderLocalTasks(localList(), nextIds));
      return;
    }
    writeConfig({ taskOrder: nextIds });
  }

  async function runDragPoll(gen: number): Promise<void> {
    while (polling && generation === gen) {
      let st: CursorState;
      try {
        st = await invoke('get_cursor_state');
      } catch {
        resetDrag();
        return;
      }
      if (!polling || generation !== gen) return;

      if (!st.left_down) { commitDrag(); return; }

      const p = press;
      if (!p) { resetDrag(); return; }
      const [cx, cy] = physToClient(st.x, st.y);

      if (!dragId()) {
        if (pastDragThreshold(cx - p.x, cy - p.y)) {
          dragRan = true;
          setDragId(p.id);
          setDragRows(displayTasks().slice());
        }
      }

      const list = dragRows();
      const id = dragId();
      if (list && id) {
        // Rects are re-read each tick because the rows move under the cursor as
        // the order changes; a snapshot taken at drag start would aim at where
        // the rows USED to be.
        const rects = list.map(tk => rowEls.get(tk.id)?.getBoundingClientRect());
        if (rects.every((r): r is DOMRect => !!r)) {
          const target = list[rowIndexAtY(rects, cy)];
          if (target && target.id !== id) {
            const byId = new Map(list.map(tk => [tk.id, tk]));
            const ids = moveId(list.map(tk => tk.id), id, target.id);
            // [S-64] The in-flight preview is sunk exactly like the settled
            // list, so dragging a completed row can never lift it into the
            // active block and then have it snap back on release. It still
            // reorders freely *within* the sunk group, which is the only place
            // its position is meaningful.
            setDragRows(sinkCompleted(ids.map(i => byId.get(i)).filter((tk): tk is TodoTask => !!tk)));
          }
        }
      }

      await new Promise(r => setTimeout(r, 16)); // ~60 fps
    }
  }

  function onRowMouseDown(e: MouseEvent, task: TodoTask): void {
    if (e.button !== 0) return;
    const target = e.target as HTMLElement | null;
    // The dot completes a task and the editor owns its caret; neither is a drag
    // handle, so a press there arms nothing.
    if (target?.closest('.todo-check, .todo-edit-input')) return;
    // Wipe any leftover gesture BEFORE arming, so this press can never inherit a
    // stale origin from an aborted one.
    resetDrag();
    dragRan = false;
    press = { id: task.id, x: e.clientX, y: e.clientY };
    polling = true;
    const gen = generation;
    runDragPoll(gen).catch(() => resetDrag());
  }

  // ── Rename ([S-50]) ────────────────────────────────────────────────────────
  // Where the caret goes when the editor opens. A click carries the offset it
  // landed on; F2 / the row menu carry `null` and get the end of the title.
  // Non-reactive: it is read once by the editor's ref, in the same tick.
  let pendingCaret: number | null = null;

  /** True if the gesture that just ended was a real drag, which the CLICK that
   *  follows the release must swallow — a reorder that finished on a title is
   *  not a rename ([S-54]). Consumed here, re-armed by the next press, never on
   *  a timer. Only the click path asks: F2 and the row menu are not the tail of
   *  a drag, so they must still open the editor. */
  function consumeDrag(): boolean {
    if (!dragRan) return false;
    dragRan = false;
    return true;
  }

  function startEdit(task: TodoTask, caret: number | null = null) {
    pendingCaret = caret;
    setEditingId(task.id);
    setEditTitle(task.title);
  }

  function cancelEdit() {
    setEditingId(null);
    setEditTitle('');
  }

  async function commitEdit(task: TodoTask) {
    const next = editTitle().trim();
    cancelEdit();
    // An emptied editor keeps the task as it was; deleting is the row menu's
    // job, and losing a task to a stray Ctrl+A + Enter would be unrecoverable.
    if (!next || next === task.title) return;

    if (isLocal()) { writeLocal(renameLocalTask(localList(), task.id, next)); return; }

    setTasks(prev => prev.map(t => t.id === task.id ? { ...t, title: next } : t));
    try {
      await invoke('todo_rename_task', {
        accountId: effectiveIds()[0],
        listId: writeListId(task),
        taskId: task.id,
        title: next,
      });
    } catch {
      setTasks(prev => prev.map(t => t.id === task.id ? { ...t, title: task.title } : t));
    }
  }

  // The editor is a textarea so a long title wraps and shows in full, but a task
  // title is still one line of text: Enter commits it rather than inserting a
  // newline the backends would have to carry.
  function handleEditKeyDown(e: KeyboardEvent, task: TodoTask) {
    if (e.key === 'Enter') { e.preventDefault(); commitEdit(task); }
    else if (e.key === 'Escape') { e.preventDefault(); cancelEdit(); }
    // [S-56] Tab commits and opens the NEXT row's editor (Shift+Tab the
    // previous), so a whole list can be corrected without reaching for the
    // mouse. Past either end the editor just closes — wrapping around mid-edit
    // would put the caret somewhere the user was not looking. The Rust hook is
    // stood down while the editor has focus ([S-38]), so this is a real DOM
    // keydown, not a routed one.
    else if (e.key === 'Tab') {
      e.preventDefault();
      const list = rows();
      const idx = list.findIndex(tk => tk.id === task.id);
      commitEdit(task);
      const next = idx < 0 ? undefined : list[e.shiftKey ? idx - 1 : idx + 1];
      if (next) {
        setFocusedIdx(e.shiftKey ? idx - 1 : idx + 1);
        startEdit(next);
      }
    }
  }

  async function deleteTask(task: TodoTask) {
    if (isLocal()) { writeLocal(removeLocalTask(localList(), task.id)); return; }
    const idx = tasks().findIndex(t => t.id === task.id);
    setTasks(prev => prev.filter(t => t.id !== task.id));
    try {
      await invoke('todo_delete_task', {
        accountId: effectiveIds()[0],
        listId: writeListId(task),
        taskId: task.id,
      });
    } catch {
      setTasks(prev => [...prev.slice(0, idx), task, ...prev.slice(idx)]);
    }
  }

  function handleTaskCtx(e: MouseEvent, task: TodoTask) {
    e.preventDefault();
    e.stopPropagation();
    const done = task.status === 'completed';
    showCtxMenu(e, [
      {
        icon: done ? Square : CheckSquare,
        label: done ? t('ctxMenu.markIncomplete') : t('ctxMenu.markComplete'),
        action: () => toggleTask(task),
      },
      {
        icon: Pencil,
        label: t('common.rename'),
        action: () => startEdit(task),
      },
      { sep: true },
      {
        icon: Trash2,
        label: t('common.delete'),
        action: () => deleteTask(task),
        danger: true,
      },
    ]);
  }

  async function createTask() {
    const title = newTitle().trim();
    if (!title) { setCreating(false); return; }

    if (isLocal()) {
      writeLocal(addLocalTask(localList(), title, `local-${crypto.randomUUID()}`));
      setNewTitle('');
      setCreating(false);
      return;
    }

    const tempId = `temp-${Date.now()}`;
    // [S-52] A task added from Today is due today — otherwise it would be
    // created undated and vanish from the very view it was typed into. The
    // optimistic row carries the same day so it survives the Today filter until
    // the server task replaces it.
    const dueDate = isToday() ? todayDueDate() : undefined;
    const optimistic: TodoTask = {
      id: tempId, title, status: 'notStarted', importance: 'normal',
      due_date: dueDate ? `${dueDate}T00:00:00.0000000` : null,
      completed_at: null,
      // Left empty under Today: the real list is the account's default one,
      // which only the backend knows. The server task that replaces this row
      // carries it, and `writeListId` falls back to the selection until then.
      list_id: isToday() ? '' : props.listId ?? '',
    };
    // [S-55] APPEND: the "Add task" row is at the bottom, so that is where the
    // task the user just typed belongs. It used to be prepended, which threw the
    // new row to the top of the list, away from the caret that made it. The
    // server task that replaces this one stays put for the same reason —
    // `applyTaskOrder` puts an id it has never seen last ([S-54]).
    setTasks(prev => [...prev, optimistic]);
    setNewTitle('');
    setCreating(false);

    try {
      const created = await invoke('todo_create_task', {
        accountId: effectiveIds()[0],
        listId: props.listId ?? '',
        title,
        ...(dueDate ? { dueDate } : {}),
      });
      setTasks(prev => prev.map(t => t.id === tempId ? created : t));
      // [S-55] Freeze the arrangement the user is looking at, with the new task
      // last. Without this the next poll would re-impose the backend's own
      // sorting — Graph hands back its lists most-recently-changed first, which
      // is exactly how a just-added task kept reappearing at the TOP.
      pinOrder();
    } catch {
      setTasks(prev => prev.filter(t => t.id !== tempId));
    }
  }

  /** Persist the order currently on screen ([S-54]). Rebuilt from the rendered
   *  rows, so ids of deleted tasks prune themselves instead of accumulating. */
  function pinOrder(): void {
    writeConfig({ taskOrder: displayTasks().map(tk => tk.id) });
  }

  function handleAddKeyDown(e: KeyboardEvent) {
    if (e.key === 'Enter') { e.preventDefault(); createTask(); }
    else if (e.key === 'Escape') { setNewTitle(''); setCreating(false); }
    // [S-56] Tab commits and leaves the add row OPEN for the next task, so a
    // list can be typed in one go; Shift+Tab commits and closes it.
    else if (e.key === 'Tab') {
      e.preventDefault();
      createTask();
      if (!e.shiftKey) setCreating(true);
    }
  }

  // ── Keyboard nav ──────────────────────────────────────────────────────────
  function scrollFocusedIntoView(): void {
    queueMicrotask(() => {
      listEl?.querySelectorAll<HTMLElement>('.todo-item')[focusedIdx()]
        ?.scrollIntoView({ block: 'nearest', inline: 'nearest' });
    });
  }

  function handleKey(e: WidgetKeyEvent): void {
    const tag = document.activeElement?.tagName;
    if (tag === 'INPUT' || tag === 'TEXTAREA') return;
    const list = rows();
    if (!list.length) return;
    const curr = focusedIdx();
    const last = list.length - 1;
    // [S-56] Tab steps the focus forward, Shift+Tab back. Unlike the arrows it
    // WRAPS at the ends, the way Tab does everywhere else — it is a "next one"
    // key, not a directional one.
    if (e.key === 'Tab') {
      e.preventDefault();
      setFocusedIdx(e.shiftKey ? (curr <= 0 ? last : curr - 1) : (curr < 0 || curr >= last ? 0 : curr + 1));
      scrollFocusedIntoView();
    }
    else if (e.key === 'ArrowDown') { e.preventDefault(); setFocusedIdx(curr < 0 ? 0 : Math.min(last, curr + 1)); scrollFocusedIntoView(); }
    else if (e.key === 'ArrowUp')  { e.preventDefault(); setFocusedIdx(curr < 0 ? last : Math.max(0, curr - 1)); scrollFocusedIntoView(); }
    else if (e.key === 'Home')     { e.preventDefault(); setFocusedIdx(0); scrollFocusedIntoView(); }
    else if (e.key === 'End')      { e.preventDefault(); setFocusedIdx(last); scrollFocusedIntoView(); }
    else if (e.key === 'Escape')   { setFocusedIdx(-1); }
    else if (e.key === 'Enter')    {
      if (curr >= 0 && curr < list.length) { e.preventDefault(); toggleTask(list[curr]); }
    }
    // [S-50] F2 is the Windows rename key, and it is the ONLY keyboard route to
    // the editor now that a title click opens it — Enter stays the toggle.
    else if (e.key === 'F2')       {
      if (curr >= 0 && curr < list.length) { e.preventDefault(); startEdit(list[curr]); }
    }
    else if (e.key === 'Delete')   {
      if (curr >= 0 && curr < list.length) {
        e.preventDefault();
        deleteTask(list[curr]);
        setFocusedIdx(Math.min(last - 1, curr));
      }
    }
  }

  // Clear the focused-row highlight when clicking outside any task row (its own
  // chrome, another widget). Mirrors the email widget.
  function handleGlobalMouseDown(e: MouseEvent): void {
    const target = e.target as HTMLElement | null;
    if (target?.closest('.ctx-menu, .ctx-submenu, .ctx-backdrop')) return;
    if (!target?.closest('.todo-item')) setFocusedIdx(-1);
  }

  let _kbd: ReturnType<typeof registerWidgetKeyboard> | undefined;
  let _cleanupBlur: (() => void) | undefined;
  onMount(() => {
    _kbd = registerWidgetKeyboard(handleKey);
    document.addEventListener('mousedown', handleGlobalMouseDown);
    // Clicking outside the Themia app clears the focused-row highlight too.
    _cleanupBlur = onAppBlur(() => setFocusedIdx(-1));
  });
  onCleanup(() => {
    _kbd?.dispose();
    document.removeEventListener('mousedown', handleGlobalMouseDown);
    _cleanupBlur?.();
  });
  function activateKbd(e: MouseEvent): void { if (e.button === 0) _kbd?.activate(); }

  createEffect(() => {
    const n = displayTasks().length;
    if (focusedIdx() >= n) setFocusedIdx(n - 1);
  });

  return (
    <div
      class="todo-widget"
      onMouseDown={activateKbd}
      /* [S-49] One variable drives the clamp for every row. */
      style={{ '--todo-title-lines': String(titleLines()) }}
    >
      <WidgetHeader
        mode={props.headerMode ?? 'normal'}
        icon={CheckSquare}
        title={title()}
        secondary={
          /* [S-02] `view()`, not `state()`: the fetch state machine never leaves
             'loading' in local mode, which is why the count badge used to be an
             account-mode-only feature. */
          <Show when={view() === 'loaded' && activeCount() > 0}>
            <span class="todo-header-count">{activeCount()}</span>
          </Show>
        }
      />

      <div class="widget-body">
      <Show when={view() === 'empty'}>
        <div class="todo-empty">
          <span class="todo-empty-text" innerHTML={t('todo.selectAccount')} />
        </div>
      </Show>

      <Show when={view() === 'nolist'}>
        <div class="todo-empty">
          {/* [S-58] Local mode has its own copy: there may be no lists on the
              device at all, so "select" would name something that isn't there. */}
          <span class="todo-empty-text" innerHTML={isLocal() ? t('todo.selectLocalList') : t('todo.selectList')} />
        </div>
      </Show>

      <Show when={view() === 'noauth'}>
        <div class="todo-empty">
          <span class="todo-empty-text">{t('todo.reauth')}</span>
        </div>
      </Show>

      <Show when={view() === 'error'}>
        <div class="todo-empty">
          <span class="todo-empty-text">{error()}</span>
        </div>
      </Show>

      <Show when={view() === 'loading' && displayTasks().length === 0}>
        <div class="todo-empty">
          <RefreshCw size={14} strokeWidth={1.5} class="todo-spinner" />
        </div>
      </Show>

      <Show when={view() === 'loaded'}>
        <div
          class="todo-list"
          classList={{ reordering: dragId() !== null }}
          ref={listEl}
          onScroll={checkScroll}
        >
          <For each={rows()}>
            {(task, i) => (
              <div
                class={`todo-item ${viewMode() === 'compact' ? 'compact' : ''} ${task.status === 'completed' ? 'done' : ''} ${focusedIdx() === i() ? 'kbd-focused' : ''}`}
                classList={{ dragging: dragId() === task.id }}
                ref={(el) => { rowEls.set(task.id, el); onCleanup(() => rowEls.delete(task.id)); }}
                onContextMenu={(e) => handleTaskCtx(e, task)}
                onMouseDown={(e) => onRowMouseDown(e, task)}
                onClick={() => setFocusedIdx(i())}
              >
                {/* [S-31] Completing a task is the DOT's job and nothing else's.
                    A row-wide toggle meant every click meant to read, focus or
                    rename a task also ticked it off. */}
                <button
                  class="todo-check"
                  aria-label={task.status === 'completed' ? t('ctxMenu.markIncomplete') : t('ctxMenu.markComplete')}
                  onClick={(e) => { e.stopPropagation(); setFocusedIdx(i()); toggleTask(task); }}
                >
                  <Show when={task.status === 'completed'} fallback={<Circle size={16} strokeWidth={1.5} />}>
                    <CheckCircle2 size={16} strokeWidth={1.5} />
                  </Show>
                </button>
                <div class="todo-content">
                  {/* [S-49] Titles wrap and clamp to `titleLines`; the native
                      tooltip carries the rest. [S-50] Clicking one edits it,
                      and the editor is unclamped so the whole title is there. */}
                  <Show
                    when={editingId() === task.id}
                    fallback={
                      <span
                        class="todo-title"
                        title={task.title}
                        onClick={(e) => {
                          if (consumeDrag()) return;
                          startEdit(task, caretOffsetFromPoint(e.clientX, e.clientY, task.title.length));
                        }}
                      >
                        {task.title}
                      </span>
                    }
                  >
                    <textarea
                      class="todo-edit-input"
                      rows={1}
                      value={editTitle()}
                      onInput={(e) => { setEditTitle(e.currentTarget.value); growToFit(e.currentTarget); }}
                      onKeyDown={(e) => handleEditKeyDown(e, task)}
                      onBlur={() => commitEdit(task)}
                      onClick={(e) => e.stopPropagation()}
                      /* The caret lands where the click did; only a keyboard
                         route (F2 / row menu) falls back to the end. Selecting
                         the whole title would arm the next keystroke to wipe
                         it. */
                      ref={(el) => queueMicrotask(() => { growToFit(el); focusWithCaret(el, pendingCaret); })}
                    />
                  </Show>
                  <Show when={viewMode() !== 'compact' && task.due_date}>
                    <span class={`todo-due ${duePast(task.due_date) && task.status !== 'completed' ? 'overdue' : ''}`}>
                      {formatDue(task.due_date)}
                    </span>
                  </Show>
                </div>
                <Show when={viewMode() === 'compact' && task.due_date}>
                  <span class={`todo-due ${duePast(task.due_date) && task.status !== 'completed' ? 'overdue' : ''}`}>
                    {formatDue(task.due_date)}
                  </span>
                </Show>
                <Show when={task.importance === 'high'}>
                  <span class="todo-importance">!</span>
                </Show>
              </div>
            )}
          </For>

          <Show when={creating()} fallback={
            <div class="todo-add-row" data-focusable="true" onClick={() => setCreating(true)}>
              <Plus size={16} strokeWidth={1.5} />
              <span>{t('todo.addTask')}</span>
            </div>
          }>
            <div class="todo-add-row editing">
              <Plus size={16} strokeWidth={1.5} />
              {/* [S-32] A textarea, not an input: a long task WRAPS while it is
                  being typed, the same way the rename editor does ([S-50]), so
                  the text never scrolls sideways out of view. Enter still
                  commits rather than inserting a newline. */}
              <textarea
                class="todo-add-input"
                rows={1}
                placeholder={t('todo.titlePlaceholder')}
                value={newTitle()}
                onInput={(e) => { setNewTitle(e.currentTarget.value); growToFit(e.currentTarget); }}
                onKeyDown={handleAddKeyDown}
                onBlur={() => createTask()}
                ref={(el) => queueMicrotask(() => { growToFit(el); el.focus(); })}
              />
            </div>
          </Show>
        </div>
        <Show when={canScrollDown()}>
          <div class="todo-fade" />
        </Show>
      </Show>
      </div>
    </div>
  );
}
