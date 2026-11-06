// @vitest-environment jsdom
//
// The To Do widget must MOUNT. That sounds trivial; it is the one thing the rest
// of the suite cannot see.
//
// A component body runs top to bottom, and Solid's `createMemo`/`createEffect`
// compute EAGERLY, so a memo near the top that reads a signal declared near the
// bottom throws in the temporal dead zone and takes the whole widget down: the
// grid catches it and paints "failed to render" on a dead tile. That is exactly
// what shipped with [S-65]/[S-66] — typecheck, lint and every unit test were
// green, because none of them ever builds the component.
//
// So this test does nothing clever: it renders the real component against a real
// device list, and asserts that rows and the group divider came out. Only the
// widget's SURROUNDINGS are stubbed (Tauri's window + event plumbing, the
// keyboard hook, the shared header). No deep mocks: the component, its memos,
// its stores and its pure helpers are the ones that ship.
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render } from 'solid-js/web';
import { createComponent } from 'solid-js';

vi.mock('@tauri-apps/api/window', () => ({
  getCurrentWindow: () => ({
    label: 'monitor-test',
    innerPosition: () => Promise.reject(new Error('no tauri window in jsdom')),
  }),
}));
vi.mock('../../../src/input/widgetKeyboard.ts', () => ({
  registerWidgetKeyboard: () => ({ activate: () => {}, dispose: () => {} }),
}));
vi.mock('../../../src/input/appBlur.ts', () => ({ onAppBlur: () => () => {} }));
vi.mock('../../../src/demo/invoke.ts', () => ({
  invoke: vi.fn(async () => []),
  isDemo: () => false,
}));
vi.mock('../../../src/components/WidgetHeader.tsx', () => ({ default: () => null }));
vi.mock('../../../src/viewContext.tsx', () => ({
  useLayout: () => ({ layout: { widgets: [] }, updateWidgetConfig: () => {} }),
}));

import Todo, { type TodoProps } from './Todo.tsx';
import { seedLocalList } from './localListStore.ts';

function mount(props: TodoProps): { host: HTMLElement; dispose: () => void } {
  const host = document.createElement('div');
  document.body.appendChild(host);
  const dispose = render(() => createComponent(Todo, props), host);
  return { host, dispose };
}

describe('todo widget mounts [S-65] [S-66]', () => {
  beforeEach(() => { document.body.innerHTML = ''; });

  it('renders a device list with its bands instead of crashing', () => {
    seedLocalList('list-mount', 'My tasks', [
      { id: 'a', title: 'Still to do', done: false },
      { id: 'b', title: 'Already done', done: true },
    ]);

    const { host, dispose } = mount({
      storageMode: 'local',
      localListId: 'list-mount',
      showCompleted: true,
      _widgetId: 'widget-mount',
    });

    // Rows, in the order the halves say: active first, finished after.
    const titles = Array.from(host.querySelectorAll('.todo-title'), el => el.textContent);
    expect(titles).toEqual(['Still to do', 'Already done']);
    expect(host.querySelectorAll('.todo-item.in-done').length).toBe(1);

    // The band between them, with the count it is there to explain.
    const divider = host.querySelector('.todo-divider');
    expect(divider, 'the finished group needs its divider').not.toBeNull();
    expect(divider!.textContent).toContain('1');

    // The add row closes the ACTIVE half instead of following the archive.
    const addRow = host.querySelector('.todo-add-row');
    expect(addRow, 'the add row is part of every loaded list').not.toBeNull();

    dispose();
  });

  it('leaves the divider out when nothing is finished', () => {
    seedLocalList('list-plain', 'Plain', [{ id: 'a', title: 'Only one', done: false }]);

    const { host, dispose } = mount({
      storageMode: 'local',
      localListId: 'list-plain',
      showCompleted: true,
      _widgetId: 'widget-plain',
    });

    expect(Array.from(host.querySelectorAll('.todo-title'), el => el.textContent)).toEqual(['Only one']);
    expect(host.querySelector('.todo-divider')).toBeNull();
    expect(host.querySelector('.todo-add-row')).not.toBeNull();

    dispose();
  });
});
