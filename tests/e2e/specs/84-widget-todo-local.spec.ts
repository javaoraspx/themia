/**
 * To Do widget — LOCAL storage mode.
 *
 * Covers widgets/office/todo/SPEC.md [S-41..S-47] and [S-57..S-59]: an
 * account-free To Do list whose tasks live in the DEVICE-WIDE list pool
 * (`todo-local-lists`), with each widget's config holding only the id of the
 * list it shows. The headline guarantees are that a widget shows exactly the
 * list it points at — and that two widgets pointing at the same one are two
 * views of one list, not two copies.
 */
import { expect } from 'chai';
import { browser, $, $$ } from '@wdio/globals';
import { resetAppState, LS_KEYS } from '../helpers/app.js';

const LISTS_KEY = 'todo-local-lists';

interface SeedTask { id: string; title: string; done: boolean }
interface SeedList { id: string; name: string; tasks: SeedTask[] }
interface TodoDef { id: string; listId?: string; config?: Record<string, any> }

/** Seed the device list pool + one To Do widget per def. Both writes fire a
 *  storage event, which is how the live stores in the page pick them up. */
async function seedTodos(lists: SeedList[], defs: TodoDef[]): Promise<void> {
  await browser.execute((layoutKey: string, listsKey: string, ls: SeedList[], ds: TodoDef[]) => {
    const put = (key: string, value: unknown) => {
      const serialized = JSON.stringify(value);
      const prior = localStorage.getItem(key);
      localStorage.setItem(key, serialized);
      window.dispatchEvent(new StorageEvent('storage', {
        key, newValue: serialized, oldValue: prior, storageArea: localStorage,
      }));
    };
    put(listsKey, ls);
    put(layoutKey, {
      widgets: ds.map((d, i) => ({
        id: d.id, type: 'todo',
        col: 1 + i * 3, row: 1, colSpan: 3, rowSpan: 5,
        config: {
          storageMode: 'local', localListId: d.listId ?? '',
          viewMode: 'list', showCompleted: false,
          ...(d.config ?? {}),
        },
      })),
      widgetGap: 12, bgImage: '', bgImageFit: 'cover',
    });
  }, LS_KEYS.layoutRoot, LISTS_KEY, lists, defs);
  await $('.widget').waitForExist({ timeout: 5_000 });
  await $('.todo-widget').waitForExist({ timeout: 5_000 });
}

/** One list ('l1'), one widget showing it — the common case. */
async function seedTodoLocal(tasks: SeedTask[], config: Record<string, any> = {}): Promise<void> {
  await seedTodos(
    [{ id: 'l1', name: 'My tasks', tasks }],
    [{ id: 'todo-local-1', listId: 'l1', config }],
  );
}

async function storedTasks(listId = 'l1'): Promise<SeedTask[]> {
  return browser.execute((key: string, id: string) => {
    try {
      const parsed = JSON.parse(localStorage.getItem(key) ?? '[]');
      const list = (Array.isArray(parsed) ? parsed : []).find((l: any) => l?.id === id);
      return (list?.tasks ?? []) as SeedTask[];
    } catch { return []; }
  }, LISTS_KEY, listId);
}

async function storedLists(): Promise<SeedList[]> {
  return browser.execute((key: string) => {
    try {
      const parsed = JSON.parse(localStorage.getItem(key) ?? '[]');
      return (Array.isArray(parsed) ? parsed : []) as SeedList[];
    } catch { return [] as SeedList[]; }
  }, LISTS_KEY);
}

async function widgetConfig(widgetId: string): Promise<Record<string, any>> {
  return browser.execute((key: string, id: string) => {
    try {
      const parsed = JSON.parse(localStorage.getItem(key) ?? '{}');
      return ((parsed.widgets ?? []).find((w: any) => w.id === id)?.config ?? {}) as Record<string, any>;
    } catch { return {}; }
  }, LS_KEYS.layoutRoot, widgetId);
}

async function renderedTitles(widgetIndex = 0): Promise<string[]> {
  return browser.execute((idx: number) => {
    const w = document.querySelectorAll('.todo-widget')[idx];
    return Array.from(w?.querySelectorAll('.todo-title') ?? []).map(e => (e.textContent ?? '').trim());
  }, widgetIndex);
}

// Open the inline add-row of a To Do widget and commit a task via Enter.
async function addTaskToWidget(title: string, widgetIndex = 0): Promise<void> {
  await browser.execute((idx: number) => {
    const w = document.querySelectorAll('.todo-widget')[idx];
    (w?.querySelector('.todo-add-row') as HTMLElement | null)?.click();
  }, widgetIndex);
  await $('.todo-add-input').waitForDisplayed({ timeout: 3_000 });
  await browser.execute((val: string, idx: number) => {
    const w = document.querySelectorAll('.todo-widget')[idx];
    // [S-32] The add field is a <textarea>, not an <input>. Take the setter off
    // the element's OWN prototype: HTMLInputElement's setter applied to a
    // textarea throws "Illegal invocation", and hardcoding either one makes the
    // helper break again the next time the field changes shape.
    const input = w?.querySelector('.todo-add-input') as HTMLInputElement | HTMLTextAreaElement | null;
    if (!input) return;
    input.focus();
    const setter = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(input), 'value')?.set;
    setter?.call(input, val);
    input.dispatchEvent(new Event('input', { bubbles: true }));
    input.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true }));
  }, title, widgetIndex);
}

describe('widget: To Do (local storage)', () => {
  beforeEach(async () => { await resetAppState(); });

  // ── Display ────────────────────────────────────────────────────────────────

  it('[S-41] renders the selected device list with no account prompt', async () => {
    await seedTodoLocal([
      { id: 'a', title: 'Buy milk', done: false },
      { id: 'b', title: 'Call bank', done: false },
    ]);
    const items = await $$('.todo-item');
    expect(await items.length).to.equal(2);
    expect(await renderedTitles()).to.deep.equal(['Buy milk', 'Call bank']);

    // No "select account / select list" empty state in local mode.
    const empties = await $$('.todo-empty');
    expect(await empties.length).to.equal(0);
  });

  it('[S-02] the header takes its title from the list, and counts the active tasks', async () => {
    await seedTodos(
      [{ id: 'l1', name: 'Groceries', tasks: [
        { id: 'a', title: 'Milk', done: false },
        { id: 'b', title: 'Eggs', done: true },
      ] }],
      [{ id: 'todo-local-1', listId: 'l1', config: { showCompleted: true } }],
    );
    const header = await browser.execute(() => {
      const w = document.querySelector('.todo-widget');
      return {
        title: (w?.querySelector('.wh-text')?.textContent ?? '').trim(),
        count: (w?.querySelector('.todo-header-count')?.textContent ?? '').trim(),
      };
    });
    expect(header.title).to.equal('Groceries');
    expect(header.count).to.equal('1'); // the completed one doesn't count
  });

  // [S-64] A finished task stops competing for the eye: it is drawn after every
  // task still to do, wherever it happens to sit in storage.
  it('[S-64] completed tasks sink below the active ones', async () => {
    await seedTodoLocal([
      { id: 'b', title: 'Already done', done: true },
      { id: 'a', title: 'Still to do', done: false },
      { id: 'c', title: 'Also done', done: true },
    ], { showCompleted: true });
    expect(await renderedTitles()).to.deep.equal(['Still to do', 'Already done', 'Also done']);
    // Storage is display-ordered only at render time: the array is untouched.
    expect((await storedTasks()).map(t => t.id)).to.deep.equal(['b', 'a', 'c']);
  });

  // [S-58] A widget pointing at a list that isn't there must not fall through to
  // some other list — an empty prompt is better than someone else's tasks.
  it('[S-58] a missing or unset list shows the pick-a-list prompt, not another list', async () => {
    await seedTodos(
      [{ id: 'l1', name: 'Mine', tasks: [{ id: 'a', title: 'Secret', done: false }] }],
      [{ id: 'todo-local-1', listId: 'deleted-list' }],
    );
    expect(await renderedTitles()).to.deep.equal([]);
    const empty = await $('.todo-widget .todo-empty-text');
    expect(await empty.getText()).to.contain('list');
  });

  it('[S-46] showCompleted hides completed tasks but keeps them stored', async () => {
    await seedTodoLocal([
      { id: 'a', title: 'Active', done: false },
      { id: 'b', title: 'Done', done: true },
    ], { showCompleted: false });
    expect(await renderedTitles()).to.deep.equal(['Active']);
    // Still persisted (only hidden from the view).
    expect((await storedTasks()).map(t => t.id)).to.deep.equal(['a', 'b']);
  });

  // ── Mutations ────────────────────────────────────────────────────────────

  it('[S-43][S-55] Add task APPENDS an active task to the selected list', async () => {
    await seedTodoLocal([{ id: 'a', title: 'Existing', done: false }]);
    await addTaskToWidget('Fresh task');

    await browser.waitUntil(async () => (await storedTasks()).length === 2, {
      timeout: 3_000, timeoutMsg: 'add-task did not persist',
    });
    const ts = await storedTasks();
    // [S-55] It lands where it was typed: at the bottom, under the add row.
    expect(ts[0].id).to.equal('a');
    expect(ts[1].title).to.equal('Fresh task');
    expect(ts[1].done).to.equal(false);
    expect(typeof ts[1].id).to.equal('string').and.not.equal('');
  });

  it('[S-44] clicking the dot toggles done and persists', async () => {
    await seedTodoLocal([{ id: 'a', title: 'Toggle me', done: false }], { showCompleted: true });
    await browser.execute(() => {
      const w = document.querySelectorAll('.todo-widget')[0];
      (w?.querySelector('.todo-check') as HTMLElement | null)?.click();
    });
    await browser.waitUntil(async () => (await storedTasks())[0]?.done === true, {
      timeout: 3_000, timeoutMsg: 'toggle did not persist done=true',
    });
  });

  // [S-31] Regression (themia-releases#104): the whole row used to be one big
  // toggle, so every click meant to read, focus or rename a task ticked it off.
  it('[S-31] clicking the row itself does NOT toggle done', async () => {
    await seedTodoLocal([{ id: 'a', title: 'Leave me alone', done: false }], { showCompleted: true });
    await browser.execute(() => {
      const w = document.querySelectorAll('.todo-widget')[0];
      (w?.querySelector('.todo-item') as HTMLElement | null)?.click();
    });
    await browser.pause(400); // a toggle would have persisted well inside this
    expect((await storedTasks())[0]?.done).to.equal(false);
  });

  // [S-49] A long title wraps to `titleLines` lines and only then clamps; the
  // full text stays reachable via the tooltip and the editor.
  it('[S-49] titleLines drives the wrap-then-clamp line count', async () => {
    const long = 'A really extremely long task title that will not fit on one line at all';
    await seedTodoLocal([{ id: 'a', title: long, done: false }], { titleLines: 3 });
    const read = () => browser.execute(() => {
      const el = document.querySelector('.todo-widget .todo-title') as HTMLElement | null;
      if (!el) return null;
      const cs = getComputedStyle(el);
      return {
        clamp: cs.webkitLineClamp,
        whiteSpace: cs.whiteSpace,
        overflowX: cs.overflowX,
        tooltip: el.getAttribute('title'),
        lines: Math.round(el.clientHeight / parseFloat(cs.lineHeight)),
      };
    });

    const three = await read();
    expect(three?.clamp).to.equal('3');
    expect(three?.tooltip).to.equal(long);
    // The old drag-to-pan answer made this a nowrap horizontal scroller.
    expect(three?.whiteSpace).to.not.equal('nowrap');
    expect(three?.overflowX).to.not.equal('auto');
    expect(three?.lines).to.equal(3);

    await seedTodoLocal([{ id: 'a', title: long, done: false }], { titleLines: 1 });
    const one = await read();
    expect(one?.clamp).to.equal('1');
    expect(one?.lines).to.equal(1);
  });

  it('[S-50] clicking the title renames the task in place', async () => {
    await seedTodoLocal([{ id: 'a', title: 'Old name', done: false }]);
    await browser.execute(() => {
      const w = document.querySelectorAll('.todo-widget')[0];
      (w?.querySelector('.todo-title') as HTMLElement | null)?.click();
    });
    await $('.todo-widget .todo-edit-input').waitForDisplayed({ timeout: 2_000 });
    await browser.execute(() => {
      const el = document.querySelector('.todo-widget .todo-edit-input') as HTMLTextAreaElement | null;
      if (!el) return;
      el.value = 'New name';
      el.dispatchEvent(new Event('input', { bubbles: true }));
      el.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
    });
    await browser.waitUntil(async () => (await storedTasks())[0]?.title === 'New name', {
      timeout: 3_000, timeoutMsg: 'rename did not persist',
    });
    expect((await storedTasks())[0]?.done).to.equal(false);
  });

  it('[S-50] Escape cancels the rename and blank keeps the old title', async () => {
    await seedTodoLocal([{ id: 'a', title: 'Keep me', done: false }]);
    const editTitleTo = async (value: string, key: 'Enter' | 'Escape') => {
      await browser.execute(() => {
        const w = document.querySelectorAll('.todo-widget')[0];
        (w?.querySelector('.todo-title') as HTMLElement | null)?.click();
      });
      await $('.todo-widget .todo-edit-input').waitForDisplayed({ timeout: 2_000 });
      await browser.execute((v: string, k: string) => {
        const el = document.querySelector('.todo-widget .todo-edit-input') as HTMLTextAreaElement | null;
        if (!el) return;
        el.value = v;
        el.dispatchEvent(new Event('input', { bubbles: true }));
        el.dispatchEvent(new KeyboardEvent('keydown', { key: k, bubbles: true }));
      }, value, key);
      await browser.pause(300);
    };

    await editTitleTo('Discarded', 'Escape');
    expect((await storedTasks())[0]?.title).to.equal('Keep me');

    await editTitleTo('   ', 'Enter');
    expect((await storedTasks())[0]?.title).to.equal('Keep me');
  });

  it('[S-45] row context menu → Delete removes the task from the list', async () => {
    await seedTodoLocal([
      { id: 'a', title: 'Keep', done: false },
      { id: 'b', title: 'Remove', done: false },
    ]);
    await browser.execute(() => {
      const items = document.querySelectorAll('.todo-widget .todo-item');
      (items[1] as HTMLElement | null)?.dispatchEvent(
        new MouseEvent('contextmenu', { bubbles: true, cancelable: true, button: 2 }),
      );
    });
    await $('.ctx-menu').waitForDisplayed({ timeout: 2_000 });
    await browser.execute(() => {
      const items = Array.from(document.querySelectorAll('.ctx-menu .ctx-item')) as HTMLElement[];
      const del = items.find(i => (i.querySelector('.ctx-label')?.textContent ?? '').trim() === 'Delete');
      del?.click();
    });
    await browser.waitUntil(async () => {
      const ts = await storedTasks();
      return ts.length === 1 && ts[0]?.id === 'a';
    }, { timeout: 3_000, timeoutMsg: 'Delete did not persist' });
  });

  // ── Many lists, one pool ([S-57]) ──────────────────────────────────────────

  it('[S-57] two widgets on DIFFERENT lists stay independent', async () => {
    await seedTodos(
      [
        { id: 'l1', name: 'Groceries', tasks: [{ id: 'a1', title: 'Groceries item', done: false }] },
        { id: 'l2', name: 'Gym',       tasks: [{ id: 'b1', title: 'Gym item', done: false }] },
      ],
      [
        { id: 'todo-A', listId: 'l1' },
        { id: 'todo-B', listId: 'l2' },
      ],
    );
    const widgets = await $$('.todo-widget');
    expect(await widgets.length).to.equal(2);

    expect(await renderedTitles(0)).to.deep.equal(['Groceries item']);
    expect(await renderedTitles(1)).to.deep.equal(['Gym item']);

    // Mutating widget A must not touch the other list.
    await addTaskToWidget('New A task', 0);
    await browser.waitUntil(async () => (await storedTasks('l1')).length === 2, {
      timeout: 3_000, timeoutMsg: 'add to A did not persist',
    });
    expect((await storedTasks('l2')).map(t => t.id)).to.deep.equal(['b1']);
    expect(await renderedTitles(1)).to.deep.equal(['Gym item']);
  });

  it('[S-57] two widgets on the SAME list are two views of one list, not two copies', async () => {
    await seedTodos(
      [{ id: 'l1', name: 'Shared', tasks: [{ id: 'a1', title: 'Shared item', done: false }] }],
      [{ id: 'todo-A', listId: 'l1' }, { id: 'todo-B', listId: 'l1' }],
    );
    expect(await renderedTitles(0)).to.deep.equal(['Shared item']);
    expect(await renderedTitles(1)).to.deep.equal(['Shared item']);

    await addTaskToWidget('Typed into A', 0);
    await browser.waitUntil(async () => (await renderedTitles(1)).length === 2, {
      timeout: 3_000, timeoutMsg: 'the second widget did not follow the shared list',
    });
    expect(await renderedTitles(1)).to.deep.equal(['Shared item', 'Typed into A']);
    // One list in the pool, not one per widget.
    expect((await storedLists()).map(l => l.id)).to.deep.equal(['l1']);
  });

  // ── Legacy migration ([S-59]) ──────────────────────────────────────────────

  it('[S-59] a pre-pool `localTasks` widget migrates into a real list, once', async () => {
    // Seeded the OLD way: tasks in the widget's own config, no list id.
    await seedTodos([], [{
      id: 'todo-legacy',
      config: { localTasks: [
        { id: 'a', title: 'From the old bag', done: false },
        { id: 'b', title: 'Also old', done: true },
      ] },
    }]);

    await browser.waitUntil(async () => (await storedLists()).length === 1, {
      timeout: 3_000, timeoutMsg: 'legacy tasks were not migrated into a list',
    });
    const lists = await storedLists();
    expect(lists[0].id).to.equal('list-migrated-todo-legacy'); // derived, so two windows converge
    expect(lists[0].tasks.map(t => t.id)).to.deep.equal(['a', 'b']);

    // The widget now points at it, and the old bag is emptied so the tasks live
    // in exactly one place. The config write-back rides the layout store's
    // persist debounce, so the pool list lands slightly before the config does.
    await browser.waitUntil(
      async () => (await widgetConfig('todo-legacy')).localListId === 'list-migrated-todo-legacy',
      { timeout: 3_000, timeoutMsg: 'the widget config was not repointed at the migrated list' },
    );
    const cfg = await widgetConfig('todo-legacy');
    expect(cfg.localListId).to.equal('list-migrated-todo-legacy');
    expect(cfg.localTasks ?? []).to.deep.equal([]);

    expect(await renderedTitles()).to.deep.equal(['From the old bag']); // completed hidden
  });

  // ── Persistence ────────────────────────────────────────────────────────────

  it('[S-41] local tasks survive a page reload', async () => {
    await seedTodoLocal([{ id: 'a', title: 'Persistent', done: false }]);
    await browser.refresh();
    await $('.todo-widget').waitForExist({ timeout: 5_000 });
    expect(await renderedTitles()).to.deep.equal(['Persistent']);
  });
});
