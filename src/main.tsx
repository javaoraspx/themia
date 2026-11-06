import { render } from 'solid-js/web';
import { maybeInitLanguageFromSystem } from './i18n.ts';
import { logger, seedLogForwardingFromStorage } from './debugLog.ts';
import { installErrorReporting, reportWindowOpened } from './analytics.ts';
import { beginBoot } from './safeMode.ts';
import { isPinLayerLabel } from './pinLayer.ts';
import { getCurrentWindow } from '@tauri-apps/api/window';
import type { ShortcutItem } from '../widgets/others/shortcut/items.ts';
import type { LayoutNode } from '../widgets/others/shortcut/layout.ts';
import type { CalendarConfigData } from '../widgets/office/calendar/CalendarConfig.tsx';

// Minimal shape of the Tauri IPC bridge the DEV-only browser harnesses below
// stub when running outside a real Tauri window. The full object is provided by
// Tauri at runtime; we only assign the few members widget modules read at import.
interface TauriInternalsStub {
  metadata: {
    currentWindow: { label: string };
    currentWebview: { label: string };
  };
  invoke: () => Promise<unknown>;
  transformCallback: () => number;
}

declare global {
  interface Window {
    __TAURI_INTERNALS__?: TauriInternalsStub;
  }
}

// [LOG-04] Seed the log bridge from the persisted toggle for EVERY window
// type. Aux windows (connect/layout/picker) have no reactive design-store
// effect and would otherwise never forward records even with logging on.
// App.tsx/ConfigWindow.tsx re-push reactively when the toggle changes.
seedLogForwardingFromStorage();

// Global crash handlers (1.18): route uncaught errors + unhandled promise
// rejections into the log pipeline so they surface in Settings > Logs (and the
// file / stdout sinks) instead of dying silently in the webview console.
// Installed before anything else so setup-time failures are captured too, and
// for every window type (this module is the entry for all of them).
window.addEventListener('error', (e) => {
  const err = e.error;
  const detail = err instanceof Error ? (err.stack || err.message) : (e.message || 'unknown error');
  const where = e.filename ? ` @ ${e.filename}:${e.lineno}:${e.colno}` : '';
  logger.error('uncaught', `${detail}${where}`);
});
window.addEventListener('unhandledrejection', (e) => {
  const r = e.reason;
  let detail: string;
  if (r instanceof Error) detail = r.stack || r.message;
  else if (typeof r === 'string') detail = r;
  else { try { detail = JSON.stringify(r); } catch { detail = String(r); } }
  logger.error('unhandledrejection', detail);
});

// Report ERROR records to TelemetryDeck's Errors preset (analytics [S-24]).
// Installed here, for every window type and right after the crash handlers
// above, so an uncaught exception during setup is reported too. Only the log
// SOURCE travels, never the message; the backend still gates on consent, so an
// opted-out user sends nothing.
installErrorReporting();

// Suppress the native browser right-click menu everywhere. The app renders its
// own context menus (see ctxMenuStore.ts), which call preventDefault on their
// own targets; this blanket listener covers empty space, text, and any surface
// without a custom handler. Runs for every window type (widget/config/etc).
window.addEventListener('contextmenu', (e) => e.preventDefault());

// First-run system locale detection. Best-effort and async, so render
// immediately; if a language flip happens after detection, all consumers
// re-render reactively because t()/tm() read the global signal.
maybeInitLanguageFromSystem();

// The detached config window is spawned with `?config=1` (see Rust
// `open_config_window`). It mounts a tiny shell that renders only the
// settings / config surface, never the widget grid.
const params = new URLSearchParams(window.location.search);

// Which auxiliary window this is, as a navigation signal (analytics [S-23]).
// The grid window reports nothing here: it IS the app, and its opening is
// already the session signal.
reportWindowOpened(params);

if (import.meta.env.DEV && params.has('pickeronly')) {
  // DEV-ONLY browser harness: render just the Add-Widget picker (with its live
  // aspect tuner) in a plain browser (no Tauri). Stub the IPC internals so
  // widget modules that touch getCurrentWindow()/invoke at import time don't
  // throw; demo fixtures supply the data. Stripped from production builds.
  if (!window.__TAURI_INTERNALS__) {
    window.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: 'preview' }, currentWebview: { label: 'preview' } },
      invoke: () => Promise.resolve(),
      transformCallback: () => 0,
    };
  }
  document.body.classList.add('config-window');
  const appEl = document.getElementById('app')!;
  // Mimic the config window's content-pane box so the height chain resolves.
  appEl.style.cssText = 'position:fixed;left:50%;top:50%;transform:translate(-50%,-50%);width:720px;height:680px;display:flex;';
  import('./App.css');
  // The docs screenshot exporter drives this same harness and reads the widget
  // manifest off `window`, so the shots and the metadata that describes them
  // always come from one registry read.
  import('./dev/docsManifest.ts').then(({ exposeDocsManifest }) => exposeDocsManifest());
  import('./components/AddWidgetPage.tsx').then(({ default: AddWidgetPage }) => {
    render(() => <div style="flex:1;min-width:0;display:flex"><AddWidgetPage onPick={() => {}} onBack={() => {}} /></div>, appEl);
  });
} else if (import.meta.env.DEV && params.has('consentcard')) {
  // DEV-ONLY browser harness: the first-run analytics consent card ([S-27])
  // over a stand-in wallpaper, so its copy and buttons can be eyeballed
  // without a Tauri session (the real card only appears on an install that
  // has not answered yet). Stripped from production builds.
  const appEl = document.getElementById('app')!;
  appEl.style.cssText = 'position:fixed;inset:0;background:linear-gradient(135deg,#2b4162,#12100e);';
  import('./App.css');
  import('./components/AnalyticsConsent.tsx').then(({ default: AnalyticsConsent }) => {
    render(() => (
      <div class="corner-badges">
        <AnalyticsConsent onAnswered={() => {}} />
      </div>
    ), appEl);
  });
} else if (import.meta.env.DEV && params.has('shortcutcfg')) {
  // DEV-ONLY browser harness: render the Shortcut widget's config panel (with
  // its inline app-layout editor) in a plain browser (no Tauri). Stripped from
  // production builds.
  if (!window.__TAURI_INTERNALS__) {
    window.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: 'preview' }, currentWebview: { label: 'preview' } },
      invoke: () => Promise.resolve(),
      transformCallback: () => 0,
    };
  }
  document.body.classList.add('config-window');
  const appEl = document.getElementById('app')!;
  appEl.style.cssText = 'position:fixed;left:50%;top:50%;transform:translate(-50%,-50%);width:720px;height:680px;display:flex;';
  import('./App.css');
  Promise.all([
    import('./demo/invoke.ts'),
    import('../widgets/others/shortcut/ShortcutConfig.tsx'),
    import('solid-js'),
  ]).then(([{ setPreviewDemo }, { default: ShortcutConfig }, { createSignal }]) => {
    setPreviewDemo(true);
    const [cfg, setCfg] = createSignal<Record<string, unknown>>({
      viewMode: 'grid',
      items: [
        { id: 'a', path: 'https://github.com', label: 'GitHub' },
        { id: 'b', path: 'shell:RecycleBinFolder' },
        { id: 'l', path: 'layout:', label: 'Office', layout: { kind: 'leaf', id: 'L0' } },
      ],
    });
    render(() => (
      <div style="flex:1;min-width:0;display:flex">
        <ShortcutConfig config={cfg()} onChange={setCfg} />
      </div>
    ), appEl);
  });
} else if (import.meta.env.DEV && params.has('shortcutaddwin')) {
  // DEV-ONLY browser harness: render the "Add shortcut" window ([S-66]) in a
  // plain browser. Stubs the IPC (icons resolve to a placeholder PNG) and seeds
  // one shortcut widget in the root layout so the rows have a target to append
  // to. Stripped from production builds.
  if (!window.__TAURI_INTERNALS__) {
    const PNG = 'data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==';
    window.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: 'preview' }, currentWebview: { label: 'preview' } },
      invoke: (() => Promise.resolve(PNG)) as unknown as () => Promise<unknown>,
      transformCallback: () => 0,
    };
  }
  localStorage.setItem('widget-layout', JSON.stringify({
    widgets: [{
      id: 'w1', type: 'shortcut', x: 0.05, y: 0.05, w: 0.4, h: 0.4,
      config: { items: [{ id: 'a', path: 'shell:MyComputerFolder' }] },
    }],
    widgetGap: 12, bgImage: '', bgImageFit: 'cover',
  }));
  history.replaceState(null, '', `${location.pathname}?shortcutadd=1&widget=w1&view=default`);
  document.body.classList.add('config-window');
  import('./ShortcutAddWindow.tsx').then(({ default: ShortcutAddWindow }) => {
    render(() => <ShortcutAddWindow />, document.getElementById('app')!);
  });
} else if (import.meta.env.DEV && params.has('shortcutwidget')) {
  // DEV-ONLY browser harness: render the Shortcut widget itself (grid view,
  // rename, per-item context menu) in a plain browser. Stripped from prod.
  if (!window.__TAURI_INTERNALS__) {
    window.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: 'preview' }, currentWebview: { label: 'preview' } },
      invoke: () => Promise.resolve(),
      transformCallback: () => 0,
    };
  }
  document.body.classList.add('widget-window');
  const appEl = document.getElementById('app')!;
  appEl.style.cssText = 'position:fixed;left:40px;top:40px;width:360px;height:320px;display:flex;';
  import('./App.css');
  Promise.all([
    import('./demo/invoke.ts'),
    import('../widgets/others/shortcut/Shortcut.tsx'),
    import('./ContextMenu.tsx'),
  ]).then(([{ setPreviewDemo }, { default: Shortcut }, { default: ContextMenu }]) => {
    setPreviewDemo(true);
    const items: ShortcutItem[] = [
      { id: 'a', path: 'https://github.com', label: 'GitHub' },
      { id: 'b', path: 'C:/Tools/build.exe', label: 'Build' },
      { id: 'c', path: 'shell:RecycleBinFolder' },
      { id: 'd', path: 'layout:', label: 'Office', layout: { kind: 'leaf', id: 'L0' } },
    ];
    render(() => <><Shortcut items={items} viewMode="grid" gridItemSize="medium" _widgetId="w1" /><ContextMenu /></>, appEl);
  });
} else if (import.meta.env.DEV && params.has('emailwidget')) {
  // DEV-ONLY browser harness: render the Email widget (list view, demo
  // messages) in a plain browser (no Tauri). Stripped from production builds.
  if (!window.__TAURI_INTERNALS__) {
    window.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: 'preview' }, currentWebview: { label: 'preview' } },
      invoke: () => Promise.resolve(),
      transformCallback: () => 0,
    };
  }
  document.body.classList.add('widget-window');
  const appEl = document.getElementById('app')!;
  appEl.style.cssText = 'position:fixed;left:40px;top:40px;width:360px;height:420px;display:flex;';
  import('./App.css');
  Promise.all([
    import('./demo/invoke.ts'),
    import('../widgets/office/email/Email.tsx'),
    import('../widgets/office/email/Email.css'),
    import('./ContextMenu.tsx'),
  ]).then(([{ setPreviewDemo }, { default: Email }, _css, { default: ContextMenu }]) => {
    setPreviewDemo(true);
    // Optional query overrides so the harness can exercise view/field toggles:
    // ?emailwidget&view=compact&fields=subject,account&accounts=a,b
    const vm = params.get('view') ?? 'list';
    const fields = params.get('fields')?.split(',').filter(Boolean);
    const accs = params.get('accounts')?.split(',').filter(Boolean) ?? ['demo-account'];
    render(() => <><Email accountIds={accs} viewMode={vm} messageCount={12} listFields={fields} /><ContextMenu /></>, appEl);
  });
} else if (import.meta.env.DEV && params.has('rsswidget')) {
  // DEV-ONLY browser harness: render the RSS widget (demo feed) in a plain
  // browser (no Tauri). Stripped from production builds.
  if (!window.__TAURI_INTERNALS__) {
    window.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: 'preview' }, currentWebview: { label: 'preview' } },
      invoke: () => Promise.resolve(),
      transformCallback: () => 0,
    };
  }
  document.body.classList.add('widget-window');
  const appEl = document.getElementById('app')!;
  appEl.style.cssText = 'position:fixed;left:40px;top:40px;width:360px;height:420px;display:flex;';
  import('./App.css');
  Promise.all([
    import('./demo/invoke.ts'),
    import('../widgets/online/rss/Rss.tsx'),
    import('../widgets/online/rss/Rss.css'),
    import('./ContextMenu.tsx'),
  ]).then(([{ setPreviewDemo }, { default: Rss }, _css, { default: ContextMenu }]) => {
    setPreviewDemo(true);
    render(() => <><Rss label="Demo Feed" maxItems={20} /><ContextMenu /></>, appEl);
  });
} else if (import.meta.env.DEV && params.has('sunmoonwidget')) {
  // DEV-ONLY browser harness: render the Sun & Moon widget in a plain browser.
  // It needs no backend at all -- the astronomy is computed locally -- so this
  // is the real widget, not a fixture. `?sunmoonwidget&lat=-33.87&lon=151.21
  // &w=250&h=330&mode=plain&events=1` exercises the globe and the responsive ladder.
  // Stripped from production builds.
  if (!window.__TAURI_INTERNALS__) {
    window.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: 'preview' }, currentWebview: { label: 'preview' } },
      invoke: () => Promise.resolve(),
      transformCallback: () => 0,
    };
  }
  document.body.classList.add('widget-window');
  const appEl = document.getElementById('app')!;
  const w = params.get('w') ?? '250';
  const h = params.get('h') ?? '400';
  appEl.style.cssText = `position:fixed;left:40px;top:40px;width:${w}px;height:${h}px;display:flex;`;
  import('./App.css');
  Promise.all([
    import('./demo/invoke.ts'),
    import('../widgets/others/sun-moon/SunMoon.tsx'),
    import('./ContextMenu.tsx'),
  ]).then(([{ setPreviewDemo }, { default: SunMoon }, { default: ContextMenu }]) => {
    setPreviewDemo(true);
    render(() => <>
      <SunMoon
        lat={Number(params.get('lat') ?? 47.37)}
        lon={Number(params.get('lon') ?? 8.54)}
        place={params.get('place') ?? 'Zurich'}
        sunPathMode={params.get('mode') ?? 'numbers'}
        showEvents={params.get('events') === '1'}
      />
      <ContextMenu />
    </>, appEl);
  });
} else if (import.meta.env.DEV && params.has('screentimewidget')) {
  // DEV-ONLY browser harness: render the Screen Time widget (demo data) in a
  // plain browser (no Tauri). `?screentimewidget&view=list|doughnut` picks the
  // display style ([S-35]). Stripped from production builds.
  if (!window.__TAURI_INTERNALS__) {
    window.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: 'preview' }, currentWebview: { label: 'preview' } },
      invoke: () => Promise.resolve(),
      transformCallback: () => 0,
    };
  }
  document.body.classList.add('widget-window');
  const appEl = document.getElementById('app')!;
  appEl.style.cssText = 'position:fixed;left:40px;top:40px;width:260px;height:300px;display:flex;';
  import('./App.css');
  Promise.all([
    import('./demo/invoke.ts'),
    import('../widgets/system/screentime/ScreentimeWidget.tsx'),
    import('./ContextMenu.tsx'),
  ]).then(([{ setPreviewDemo }, { default: Screentime }, { default: ContextMenu }]) => {
    setPreviewDemo(true);
    const style = params.get('view') === 'doughnut' ? 'doughnut' : 'list';
    const hide = params.get('hide')?.split(',').filter(Boolean) ?? [];
    render(() => <><Screentime displayStyle={style} maxApps={7} label="" hiddenApps={hide} /><ContextMenu /></>, appEl);
  });
} else if (import.meta.env.DEV && params.has('viewswitcherwidget')) {
  // DEV-ONLY browser harness: render the View Switcher widget in a plain
  // browser (no Tauri). Stripped from production builds.
  if (!window.__TAURI_INTERNALS__) {
    window.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: 'preview' }, currentWebview: { label: 'preview' } },
      invoke: () => Promise.resolve(),
      transformCallback: () => 0,
    };
  }
  document.body.classList.add('widget-window');
  const appEl = document.getElementById('app')!;
  appEl.style.cssText = 'position:fixed;left:40px;top:40px;width:360px;height:120px;display:flex;';
  import('./App.css');
  Promise.all([
    import('./demo/invoke.ts'),
    import('./views.ts'),
    import('../widgets/others/view-switcher/ViewSwitcher.tsx'),
    import('./ContextMenu.tsx'),
  ]).then(([{ setPreviewDemo }, { setViewStore }, { default: ViewSwitcher }, { default: ContextMenu }]) => {
    setPreviewDemo(true);
    setViewStore({ list: [
      { id: 'a', name: 'Office', icon: 'home' },
      { id: 'b', name: 'Online', icon: 'home' },
      { id: 'c', name: 'System', icon: 'home' },
    ], activeId: 'a' });
    render(() => <><ViewSwitcher label="Views" /><ContextMenu /></>, appEl);
  });
} else if (import.meta.env.DEV && params.has('folderwidget')) {
  // DEV-ONLY browser harness: render the Folder widget (demo entries) in a plain
  // browser (no Tauri). `?view=list|details|grid|icons` picks the view;
  // `?gridsize=small|medium|large` sizes grid/icon tiles. Stripped from
  // production builds.
  if (!window.__TAURI_INTERNALS__) {
    window.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: 'preview' }, currentWebview: { label: 'preview' } },
      invoke: () => Promise.resolve(),
      transformCallback: () => 0,
    };
  }
  document.body.classList.add('widget-window');
  const appEl = document.getElementById('app')!;
  appEl.style.cssText = 'position:fixed;left:40px;top:40px;width:380px;height:460px;display:flex;';
  import('./App.css');
  Promise.all([
    import('./demo/invoke.ts'),
    import('../widgets/office/folder/FolderView.tsx'),
    import('../widgets/office/folder/FolderView.css'),
    import('./ContextMenu.tsx'),
  ]).then(([{ setPreviewDemo }, { default: FolderView }, _css, { default: ContextMenu }]) => {
    setPreviewDemo(true);
    const view = (params.get('view') || 'list') as 'list' | 'details' | 'grid' | 'icons';
    const gridSize = (params.get('gridsize') || 'small') as 'small' | 'medium' | 'large';
    // `path` makes currentPath non-empty so the attribute/stat batch loaders run
    // (they bail on an empty path); demo fixtures answer the batch commands.
    render(() => <><FolderView viewMode={view} gridItemSize={gridSize} path="C:/demo" _widgetId="w1" /><ContextMenu /></>, appEl);
  });
} else if (import.meta.env.DEV && params.has('calendarwidget')) {
  // DEV-ONLY browser harness: render the Calendar widget (demo events) in a
  // plain browser (no Tauri). `?view=month|week|day|agenda` picks the view.
  // Stripped from production builds.
  if (!window.__TAURI_INTERNALS__) {
    window.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: 'preview' }, currentWebview: { label: 'preview' } },
      invoke: () => Promise.resolve(),
      transformCallback: () => 0,
    };
  }
  document.body.classList.add('widget-window');
  const appEl = document.getElementById('app')!;
  appEl.style.cssText = 'position:fixed;left:40px;top:40px;width:380px;height:460px;display:flex;';
  import('./App.css');
  Promise.all([
    import('./demo/invoke.ts'),
    import('../widgets/office/calendar/Calendar.tsx'),
    import('../widgets/office/calendar/Calendar.css'),
    import('./ContextMenu.tsx'),
  ]).then(([{ setPreviewDemo }, { default: Calendar }, _css, { default: ContextMenu }]) => {
    setPreviewDemo(true);
    const view = params.get('view') || 'agenda';
    // ?calendarwidget&view=agenda&cals=demo-cal-work,demo-cal-personal exercises
    // the per-calendar selection [S-49]; absent → the account's default calendar.
    const cals = params.get('cals')?.split(',').filter(Boolean);
    render(() => <>
      <Calendar
        accountIds={['demo-account']}
        defaultView={view}
        agendaCount={10}
        calendars={cals ? { 'demo-account': cals } : undefined}
      />
      <ContextMenu />
    </>, appEl);
  });
} else if (import.meta.env.DEV && params.has('calendarconfig')) {
  // DEV-ONLY browser harness: render the real CalendarConfig panel with a stubbed
  // Google account exposing three calendars, so the per-account calendar picker
  // ([S-49], nested under the account row) can be eyeballed. Stripped from prod.
  const CALS = [
    { id: 'cal-primary', name: 'Primary', is_default: true, color: null },
    { id: 'cal-work', name: 'Work', is_default: false, color: null },
    { id: 'cal-personal', name: 'Personal', is_default: false, color: null },
  ];
  window.__TAURI_INTERNALS__ = {
    metadata: { currentWindow: { label: 'config' }, currentWebview: { label: 'config' } },
    invoke: (cmd?: string) => {
      if (cmd === 'email_list_accounts') {
        return Promise.resolve([{ id: 'g1', email: 'me@gmail.com', display_name: 'Me', provider: 'google' }]);
      }
      if (cmd === 'calendar_list_calendars') return Promise.resolve(CALS);
      return Promise.resolve(null);
    },
    transformCallback: () => 0,
  };
  document.body.classList.add('config-window');
  const appEl = document.getElementById('app')!;
  appEl.style.cssText = 'position:fixed;inset:0;overflow:auto;padding:24px;max-width:520px;';
  Promise.all([
    import('solid-js'),
    import('../widgets/office/calendar/CalendarConfig.tsx'),
    import('./ContextMenu.tsx'),
  ]).then(([{ createSignal }, { default: CalendarConfig }, { default: ContextMenu }]) => {
    const [cfg, setCfg] = createSignal<CalendarConfigData>({ accountIds: ['g1'] });
    render(() => <>
      <CalendarConfig config={cfg()} onChange={(c) => setCfg(c)} />
      <ContextMenu />
    </>, appEl);
  });
} else if (import.meta.env.DEV && params.has('weatherforecastwidget')) {
  // DEV-ONLY browser harness: render the Weather Forecast widget (demo data) in
  // a plain browser (no Tauri). `?layout=rows|cols|grid` picks the layout.
  // Stripped from production builds.
  if (!window.__TAURI_INTERNALS__) {
    window.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: 'preview' }, currentWebview: { label: 'preview' } },
      invoke: () => Promise.resolve(),
      transformCallback: () => 0,
    };
  }
  document.body.classList.add('widget-window');
  const appEl = document.getElementById('app')!;
  appEl.style.cssText = 'position:fixed;left:40px;top:40px;width:420px;height:360px;display:flex;';
  import('./App.css');
  Promise.all([
    import('./demo/invoke.ts'),
    import('../widgets/online/weather-forecast/WeatherForecast.tsx'),
    import('../widgets/online/weather-forecast/WeatherForecast.css'),
    import('./ContextMenu.tsx'),
  ]).then(([{ setPreviewDemo }, { default: WeatherForecast }, _css, { default: ContextMenu }]) => {
    setPreviewDemo(true);
    const layout = params.get('layout') || 'rows';
    render(() => <><WeatherForecast locationName="Demo City" layout={layout} days={7} /><ContextMenu /></>, appEl);
  });
} else if (import.meta.env.DEV && params.has('weathercurrentwidget')) {
  // DEV-ONLY browser harness: render the Weather (current) widget (demo data) in
  // a plain browser (no Tauri). Stripped from production builds.
  if (!window.__TAURI_INTERNALS__) {
    window.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: 'preview' }, currentWebview: { label: 'preview' } },
      invoke: () => Promise.resolve(),
      transformCallback: () => 0,
    };
  }
  document.body.classList.add('widget-window');
  const appEl = document.getElementById('app')!;
  appEl.style.cssText = 'position:fixed;left:40px;top:40px;width:320px;height:280px;display:flex;';
  import('./App.css');
  Promise.all([
    import('./demo/invoke.ts'),
    import('../widgets/online/weather-current/WeatherCurrent.tsx'),
    import('../widgets/online/weather-current/WeatherCurrent.css'),
    import('./ContextMenu.tsx'),
  ]).then(([{ setPreviewDemo }, { default: WeatherCurrent }, _css, { default: ContextMenu }]) => {
    setPreviewDemo(true);
    render(() => <><WeatherCurrent locationName="Demo City" /><ContextMenu /></>, appEl);
  });
} else if (import.meta.env.DEV && params.has('claudeusagewidget')) {
  // DEV-ONLY browser harness: render the Claude Usage widget (demo data) in a
  // plain browser (no Tauri). Stripped from production builds.
  if (!window.__TAURI_INTERNALS__) {
    window.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: 'preview' }, currentWebview: { label: 'preview' } },
      invoke: () => Promise.resolve(),
      transformCallback: () => 0,
    };
  }
  document.body.classList.add('widget-window');
  const appEl = document.getElementById('app')!;
  // `&w=`/`&h=` size the tile, so the fit-to-box densities ([S-17]) and the
  // label abbreviation ([S-18]) can be checked at a given size from a fresh
  // load — a ResizeObserver never fires in a hidden preview tab.
  const cuW = Number(params.get('w')) || 260;
  const cuH = Number(params.get('h')) || 220;
  appEl.style.cssText =
    `position:fixed;left:40px;top:40px;width:${cuW}px;height:${cuH}px;display:flex;`;
  import('./App.css');
  Promise.all([
    import('./demo/invoke.ts'),
    import('../widgets/online/claude-usage/ClaudeUsage.tsx'),
    import('../widgets/online/claude-usage/ClaudeUsage.css'),
    import('./ContextMenu.tsx'),
  ]).then(([{ setPreviewDemo }, { default: ClaudeUsage }, _css, { default: ContextMenu }]) => {
    setPreviewDemo(true);
    render(() => <><ClaudeUsage showOpus showSonnet /><ContextMenu /></>, appEl);
  });
} else if (import.meta.env.DEV && params.has('pingwidget')) {
  // DEV-ONLY browser harness: render the Ping widget in a plain browser (no
  // Tauri). `?empty` renders the no-host empty state; otherwise the demo host.
  // Stripped from production builds.
  if (!window.__TAURI_INTERNALS__) {
    window.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: 'preview' }, currentWebview: { label: 'preview' } },
      invoke: () => Promise.resolve(),
      transformCallback: () => 0,
    };
  }
  document.body.classList.add('widget-window');
  const appEl = document.getElementById('app')!;
  appEl.style.cssText = 'position:fixed;left:40px;top:40px;width:240px;height:240px;display:flex;';
  import('./App.css');
  Promise.all([
    import('./demo/invoke.ts'),
    import('../widgets/system/ping/Ping.tsx'),
    import('./ContextMenu.tsx'),
  ]).then(([{ setPreviewDemo }, { default: Ping }, { default: ContextMenu }]) => {
    setPreviewDemo(true);
    const empty = params.has('empty');
    const host = empty ? { label: '', address: '' } : { label: 'Google DNS', address: '8.8.8.8' };
    render(() => <><Ping host={host} interval={30} /><ContextMenu /></>, appEl);
  });
} else if (import.meta.env.DEV && params.has('volumewidget')) {
  // DEV-ONLY browser harness: render the Volume widget in a plain browser (no
  // Tauri). Stripped from production builds.
  if (!window.__TAURI_INTERNALS__) {
    window.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: 'preview' }, currentWebview: { label: 'preview' } },
      invoke: () => Promise.resolve(),
      transformCallback: () => 0,
    };
  }
  document.body.classList.add('widget-window');
  const appEl = document.getElementById('app')!;
  appEl.style.cssText = 'position:fixed;left:40px;top:40px;width:240px;height:72px;display:flex;';
  import('./App.css');
  Promise.all([
    import('./demo/invoke.ts'),
    import('../widgets/system/volume/Volume.tsx'),
    import('./ContextMenu.tsx'),
  ]).then(([{ setPreviewDemo }, { default: Volume }, { default: ContextMenu }]) => {
    setPreviewDemo(true);
    render(() => <><Volume headerMode="none" /><ContextMenu /></>, appEl);
  });
} else if (import.meta.env.DEV && params.has('musicwidget')) {
  // DEV-ONLY browser harness: render the Now Playing widget (demo track) in a
  // plain browser (no Tauri). `?w=&h=` size the tile so the container queries
  // can be exercised. Stripped from production builds.
  if (!window.__TAURI_INTERNALS__) {
    window.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: 'preview' }, currentWebview: { label: 'preview' } },
      invoke: () => Promise.resolve(),
      transformCallback: () => 0,
    };
  }
  document.body.classList.add('widget-window');
  const appEl = document.getElementById('app')!;
  const muW = Number(params.get('w')) || 300;
  const muH = Number(params.get('h')) || 140;
  appEl.style.cssText =
    `position:fixed;left:40px;top:40px;width:${muW}px;height:${muH}px;display:flex;`;
  import('./App.css');
  Promise.all([
    import('./demo/invoke.ts'),
    import('../widgets/others/music/Music.tsx'),
    import('./ContextMenu.tsx'),
  ]).then(([{ setPreviewDemo }, { default: Music }, { default: ContextMenu }]) => {
    setPreviewDemo(true);
    render(() => <><Music showAlbumArt /><ContextMenu /></>, appEl);
  });
} else if (import.meta.env.DEV && params.has('gallerywidget')) {
  // DEV-ONLY browser harness: render the Gallery widget (demo data) in a plain
  // browser (no Tauri). `?kind=folder|file|bing` picks the source. Stripped from
  // production builds.
  if (!window.__TAURI_INTERNALS__) {
    window.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: 'preview' }, currentWebview: { label: 'preview' } },
      invoke: () => Promise.resolve(),
      transformCallback: () => 0,
    };
  }
  document.body.classList.add('widget-window');
  const appEl = document.getElementById('app')!;
  appEl.style.cssText = 'position:fixed;left:40px;top:40px;width:330px;height:330px;display:flex;';
  import('./App.css');
  const kind = (params.get('kind') || 'folder') as 'folder' | 'file' | 'bing';
  const galMode = (params.get('mode') || 'grid') as 'grid' | 'slideshow' | 'scatter' | 'mosaic';
  const galFit = (params.get('fit') || 'cover') as 'cover' | 'contain';
  Promise.all([
    import('./demo/invoke.ts'),
    import('../widgets/others/gallery/Gallery.tsx'),
    import('./ContextMenu.tsx'),
  ]).then(([{ setPreviewDemo }, { default: Gallery }, { default: ContextMenu }]) => {
    setPreviewDemo(true);
    render(() => <><Gallery
      source={kind} path="C:/demo" filePath="C:/demo/pic.jpg" recursive={false}
      showCaption={true} mode={galMode} fit={galFit} maxImages={12}
      slideshowInterval={2} refreshInterval={60} refreshNonce={0}
    /><ContextMenu /></>, appEl);
  });
} else if (import.meta.env.DEV && params.has('githubwidget')) {
  // DEV-ONLY browser harness: render a GitHub widget (demo data) in a plain
  // browser (no Tauri). `?kind=repo|notifications|review|assigned` picks which.
  // Stripped from production builds.
  if (!window.__TAURI_INTERNALS__) {
    window.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: 'preview' }, currentWebview: { label: 'preview' } },
      invoke: () => Promise.resolve(),
      transformCallback: () => 0,
    };
  }
  document.body.classList.add('widget-window');
  const appEl = document.getElementById('app')!;
  appEl.style.cssText = 'position:fixed;left:40px;top:40px;width:360px;height:420px;display:flex;';
  import('./App.css');
  const kind = params.get('kind') || 'repo';
  const load =
    kind === 'notifications' ? import('../widgets/online/github-notifications/GitHubNotifications.tsx').then(m => ({ C: m.default, css: import('../widgets/online/github-notifications/GitHubNotifications.css'), props: { accountIds: ['demo'] } }))
    : kind === 'review' ? import('../widgets/online/github-review-requested/GitHubReviewRequested.tsx').then(m => ({ C: m.default, css: import('../widgets/online/github-review-requested/GitHubReviewRequested.css'), props: { accountIds: ['demo'] } }))
    : kind === 'assigned' ? import('../widgets/online/github-assigned-issues/GitHubAssignedIssues.tsx').then(m => ({ C: m.default, css: import('../widgets/online/github-assigned-issues/GitHubAssignedIssues.css'), props: { accountIds: ['demo'] } }))
    : import('../widgets/online/github/GitHub.tsx').then(m => ({ C: m.default, css: import('../widgets/online/github/GitHub.css'), props: { repo: 'demo/demo' } }));
  Promise.all([import('./demo/invoke.ts'), load, import('./ContextMenu.tsx')])
    .then(([{ setPreviewDemo }, { C, props }, { default: ContextMenu }]) => {
      setPreviewDemo(true);
      render(() => <><C {...props} /><ContextMenu /></>, appEl);
    });
} else if (import.meta.env.DEV && params.has('layoutwindow')) {
  // DEV-ONLY browser harness: render the detached layout-editor WINDOW chrome
  // (title field + editor) in a plain browser. Stripped from production builds.
  if (!window.__TAURI_INTERNALS__) {
    window.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: 'layout' }, currentWebview: { label: 'layout' } },
      invoke: () => Promise.resolve(),
      transformCallback: () => 0,
    };
  }
  localStorage.setItem('widget-layout', JSON.stringify({
    cols: 12, rows: 8, widgetGap: 12,
    widgets: [{
      id: 'w1', type: 'shortcut', col: 1, row: 1, colSpan: 4, rowSpan: 4,
      config: { items: [{ id: 'lay', path: 'layout:', label: 'My Office Layout', layout: { kind: 'leaf', id: 'L0' } }] },
    }],
  }));
  const u = new URL(window.location.href);
  u.searchParams.set('widget', 'w1'); u.searchParams.set('view', 'default'); u.searchParams.set('item', 'lay');
  window.history.replaceState(null, '', u.toString());
  document.body.classList.add('config-window');
  import('./LayoutWindow.tsx').then(({ default: LayoutWindow }) => {
    render(() => <LayoutWindow />, document.getElementById('app')!);
  });
} else if (import.meta.env.DEV && params.has('widgetwindowdemo')) {
  // DEV-ONLY browser harness: render the generic widget pop-out WINDOW hosting a
  // seeded widget in a plain browser. `?widgetwindowdemo&type=notes|todo` picks
  // which. Stripped from production builds.
  if (!window.__TAURI_INTERNALS__) {
    window.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: 'widget-window' }, currentWebview: { label: 'widget-window' } },
      invoke: () => Promise.resolve(),
      transformCallback: () => 0,
    };
  }
  const wwType = params.get('type') || 'notes';
  // `habit` seeds the PRE-POOL config shape on purpose, so the harness exercises
  // the one-shot migration into a device list (habit-tracker [S-21]).
  const seeded = wwType === 'todo'
    ? { id: 'todo-1', type: 'todo', x: 0.1, y: 0.1, w: 0.3, h: 0.5, label: 'Tasks',
        config: { accountIds: ['demo-account'], accountId: 'demo-account', listId: 'demo-list', listName: 'Tasks', viewMode: 'list', showCompleted: false, refreshInterval: 60 } }
    : wwType === 'habit'
    ? { id: 'habit-1', type: 'habitTracker', x: 0.1, y: 0.1, w: 0.3, h: 0.4, label: 'Habits',
        config: { habits: [{ id: 'h1', name: 'Read 20 min' }, { id: 'h2', name: 'Stretch' }],
                  log: { h1: ['x'] }, resetPeriod: 'daily', customMinutes: 30, historyDots: 7 } }
    : { id: 'notes-1', type: 'notes', x: 0.1, y: 0.1, w: 0.3, h: 0.4, label: 'Shopping list',
        config: { content: 'milk\neggs\nbread', fontSize: 13 } };
  localStorage.setItem('widget-layout', JSON.stringify({ widgetGap: 12, widgets: [seeded] }));
  const u = new URL(window.location.href);
  u.searchParams.set('widget', seeded.id); u.searchParams.set('view', 'default');
  window.history.replaceState(null, '', u.toString());
  document.body.classList.add('config-window');
  Promise.all([
    import('./demo/invoke.ts'),
    import('./WidgetWindow.tsx'),
  ]).then(([{ setPreviewDemo }, { default: WidgetWindow }]) => {
    setPreviewDemo(true);
    render(() => <WidgetWindow />, document.getElementById('app')!);
  });
} else if (import.meta.env.DEV && params.has('noteswidget')) {
  // DEV-ONLY browser harness: render the Notes markdown editor in a plain
  // browser (no Tauri), seeded with one of every construct it styles. Type into
  // it to check the live projection. Stripped from production builds.
  if (!window.__TAURI_INTERNALS__) {
    window.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: 'preview' }, currentWebview: { label: 'preview' } },
      invoke: () => Promise.resolve(),
      transformCallback: () => 0,
    };
  }
  document.body.classList.add('widget-window');
  const appEl = document.getElementById('app')!;
  appEl.style.cssText = 'position:fixed;left:40px;top:40px;width:320px;height:520px;display:flex;';
  import('./App.css');
  const sample = [
    '# Shopping', '', 'Milk **whole** and *fresh*.', '',
    '- [ ] milk', '- [x] bread', '- plain bullet', '  - nested', '',
    '## Notes', '> a quote here', '', 'Run `npm run check` first.', '',
    '```js', 'const x = 1;', '```', '', '---', '', 'snake_case_name stays plain',
  ].join('\n');
  Promise.all([
    import('./demo/invoke.ts'),
    import('../widgets/others/notes/Notes.tsx'),
    import('./ContextMenu.tsx'),
  ]).then(([{ setPreviewDemo }, { default: Notes }, { default: ContextMenu }]) => {
    setPreviewDemo(true);
    const headerMode = (params.get('header') ?? 'normal') as 'normal' | 'compact' | 'none';
    render(() => <><Notes
      content={params.get('content') ?? sample}
      fontSize={13}
      headerMode={headerMode}
    /><ContextMenu /></>, appEl);
  });
} else if (import.meta.env.DEV && params.has('gridchrome')) {
  // DEV-ONLY browser harness: render the REAL WidgetGrid (non-edit) with a seeded
  // poppable widget, to exercise the widget chrome (the hover "open in window"
  // button + context menu). Stripped from production builds.
  if (!window.__TAURI_INTERNALS__) {
    window.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: 'widget' }, currentWebview: { label: 'widget' } },
      invoke: () => Promise.resolve(),
      transformCallback: () => 0,
    };
  }
  localStorage.setItem('widget-layout', JSON.stringify({ widgetGap: 12, widgets: [
    { id: 'notes-1', type: 'notes', x: 0.06, y: 0.08, w: 0.4, h: 0.5, label: 'Notes', config: { content: 'hello', fontSize: 13 } },
  ]}));
  document.body.classList.add('widget-window');
  const appEl = document.getElementById('app')!;
  appEl.classList.add('app-layer');
  appEl.style.cssText = 'position:fixed;inset:0;';
  import('./App.css');
  Promise.all([
    import('./demo/invoke.ts'),
    import('./WidgetGrid.tsx'),
    import('./ContextMenu.tsx'),
    import('./layout.ts'),
    import('./viewContext.tsx'),
  ]).then(([{ setPreviewDemo }, { default: WidgetGrid }, { default: ContextMenu }, { getViewLayoutStore }, { LayoutContext }]) => {
    setPreviewDemo(true);
    render(() => (
      <LayoutContext.Provider value={getViewLayoutStore('default')}>
        <WidgetGrid editMode={false} placing={null} onConfigure={() => {}} onEnterEditMode={() => {}} onPlace={() => {}} onStartClipboardPlacing={() => {}} onAddToContainer={() => {}} />
        <ContextMenu />
      </LayoutContext.Provider>
    ), appEl);
  });
} else if (import.meta.env.DEV && params.has('layouteditor')) {
  // DEV-ONLY browser harness: render the app-layout editor (in-pane assignment)
  // in a plain browser (no Tauri). Stripped from production builds.
  if (!window.__TAURI_INTERNALS__) {
    window.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: 'preview' }, currentWebview: { label: 'preview' } },
      invoke: () => Promise.resolve(),
      transformCallback: () => 0,
    };
  }
  document.body.classList.add('config-window');
  const appEl = document.getElementById('app')!;
  appEl.style.cssText = 'position:fixed;left:50%;top:50%;transform:translate(-50%,-50%);width:760px;height:560px;display:flex;flex-direction:column;padding:16px;box-sizing:border-box;';
  import('./App.css');
  import('./styles/config-base.css');
  Promise.all([
    import('./demo/invoke.ts'),
    import('../widgets/others/shortcut/LayoutEditor.tsx'),
    import('../widgets/others/shortcut/layout.ts'),
    import('solid-js'),
  ]).then(([{ setPreviewDemo }, { default: LayoutEditor }, lay, { createSignal }]) => {
    setPreviewDemo(true);
    const seed = {
      kind: 'split', id: 's1', direction: 'vertical', ratio: 0.5,
      a: { kind: 'leaf', id: 'l1', assignment: { kind: 'url', target: 'https://github.com', label: 'github.com', reuseIfRunning: false } },
      b: { kind: 'leaf', id: 'l2' },
    };
    const [node, setNode] = createSignal<LayoutNode>(lay.validateNode(seed));
    render(() => <div class="cfg" style="padding:0"><LayoutEditor node={node()} onChange={setNode} /></div>, appEl);
  });
} else if (import.meta.env.DEV && (params.has('hyperspace') || params.has('hacker'))) {
  // DEV-ONLY browser harness: play a boot overlay with stand-in widget boxes
  // animating in over a fake wallpaper. Pick the style with `?hyperspace`
  // ([S-16], crash warp) or `?hacker` ([S-23], terminal breach). One path,
  // parametrized by `style`. Stripped from production builds.
  const style = params.has('hacker') ? 'hacker' as const
    : 'hyperspace' as const;
  if (!window.__TAURI_INTERNALS__) {
    window.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: 'preview' }, currentWebview: { label: 'preview' } },
      invoke: () => Promise.resolve(),
      transformCallback: () => 0,
    };
  }
  // A wallpaper-ish backdrop for both: the hacker style no longer dims the
  // desktop, so its tiles should read against a normal wallpaper.
  document.body.style.background = 'linear-gradient(135deg, #2b5876, #4e4376)';
  const appEl = document.getElementById('app')!;
  appEl.style.cssText = 'position:fixed;inset:0;';
  import('./App.css');
  import('./WidgetGrid.css');
  Promise.all([
    import('./bootOverlay.ts'),
    import('./hyperspace.ts'),
    import('./components/HyperspaceOverlay.tsx'),
    import('./components/HackerTile.tsx'),
    import('solid-js'),
  ]).then(([bo, hs, { default: HyperspaceOverlay }, { default: HackerTile }, { Show, For }]) => {
    bo.beginBootOverlay(style, false);
    const boxes = [
      { x: 0.06, y: 0.08, w: 0.24, h: 0.30 },
      { x: 0.36, y: 0.08, w: 0.28, h: 0.44 },
      { x: 0.70, y: 0.08, w: 0.24, h: 0.30 },
      { x: 0.06, y: 0.48, w: 0.24, h: 0.40 },
      { x: 0.70, y: 0.48, w: 0.24, h: 0.40 },
    ];
    render(() => (
      <div
        class="view-root active"
        style={`position:fixed;inset:0;${style !== 'hacker' && bo.bootOverlayActive() ? hs.hyperspaceShakeStyle(style) : ''}`}
      >
        <For each={boxes}>
          {(b, i) => (
            <div
              class={`widget${style !== 'hacker' ? ` intro intro-${style}` : ''}`}
              style={
                `position:absolute;left:${b.x * 100}%;top:${b.y * 100}%;` +
                `width:${b.w * 100}%;height:${b.h * 100}%;` +
                (style !== 'hacker'
                  ? `animation-delay:${bo.introBaseDelayMs(style) + i() * 45}ms;` +
                    hs.hyperspaceFromVars(b, window.innerWidth, window.innerHeight, style)
                  : '') +
                'background:var(--card);'
              }
            >
              {style === 'hacker' && <HackerTile index={i()} durationMs={500} />}
            </div>
          )}
        </For>
        <Show when={style !== 'hacker' && bo.bootOverlayActive()}>
          <HyperspaceOverlay />
        </Show>
        <button
          style="position:fixed;left:50%;bottom:18px;transform:translateX(-50%);z-index:10001;padding:6px 14px;"
          onClick={() => bo.replayStartupAnimation(style, false)}
        >Replay</button>
      </div>
    ), appEl);
  });
} else if (params.has('connect')) {
  // Connect window: a focused "add a connection" wizard in its own OS window.
  // Shares the config window's `--ui-scale` surface treatment. Importing
  // design.ts (via the wizard's deps) applies the theme palette to :root.
  document.body.classList.add('config-window');
  import('./ConnectWindow.tsx').then(({ default: ConnectWindow }) => {
    render(() => <ConnectWindow />, document.getElementById('app')!);
  });
} else if (params.has('layout')) {
  // App-layout editor window: the visual pane-tree editor for a Shortcut
  // widget's `layout` item, popped out into its own OS window.
  document.body.classList.add('config-window');
  import('./LayoutWindow.tsx').then(({ default: LayoutWindow }) => {
    render(() => <LayoutWindow />, document.getElementById('app')!);
  });
} else if (params.has('shortcutadd')) {
  // "Add shortcut" catalogue window: every kind of shortcut a Shortcut widget
  // can hold, in one list. Appends through the shared per-view layout store.
  document.body.classList.add('config-window');
  import('./ShortcutAddWindow.tsx').then(({ default: ShortcutAddWindow }) => {
    render(() => <ShortcutAddWindow />, document.getElementById('app')!);
  });
} else if (params.has('layoutpicker')) {
  // "Assign app" picker window for a single layout pane. Broadcasts the choice
  // back to the layout editor window.
  document.body.classList.add('config-window');
  import('./LayoutPickerWindow.tsx').then(({ default: LayoutPickerWindow }) => {
    render(() => <LayoutPickerWindow />, document.getElementById('app')!);
  });
} else if (import.meta.env.DEV && params.has('trayflyoutdemo')) {
  // DEV-ONLY browser harness: render the tray flyout ([S-05]) around a seeded
  // tray widget in a plain browser, at its real flyout size, so the panel's
  // frame + the hosted widget can be eyeballed without a shell.
  // `?trayflyoutdemo&type=notes|todo|calculator`. Stripped from prod builds.
  if (!window.__TAURI_INTERNALS__) {
    window.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: 'tray-flyout' }, currentWebview: { label: 'tray-flyout' } },
      invoke: () => Promise.resolve(),
      transformCallback: () => 0,
    };
  }
  const trayType = params.get('type') || 'notes';
  const trayW = Number(params.get('w')) || 260;
  const trayH = Number(params.get('h')) || 320;
  const seededTray = trayType === 'calculator'
    ? { id: 'calc-1', type: 'calculator', x: 0, y: 0, w: 0.2, h: 0.2, label: '',
        config: { trayWidth: trayW, trayHeight: trayH } }
    : trayType === 'todo'
    ? { id: 'todo-1', type: 'todo', x: 0, y: 0, w: 0.2, h: 0.2, label: 'Tasks',
        config: { accountIds: ['demo-account'], accountId: 'demo-account', listId: 'demo-list',
                  listName: 'Tasks', viewMode: 'list', showCompleted: false, refreshInterval: 60,
                  trayWidth: trayW, trayHeight: trayH } }
    : { id: 'notes-1', type: 'notes', x: 0, y: 0, w: 0.2, h: 0.2, label: 'Shopping list',
        config: { content: 'milk\neggs\nbread', fontSize: 13, trayWidth: trayW, trayHeight: trayH } };
  localStorage.setItem('widget-layout-tray', JSON.stringify({ widgetGap: 12, widgets: [seededTray] }));
  const u = new URL(window.location.href);
  u.searchParams.set('widget', seededTray.id);
  window.history.replaceState(null, '', u.toString());
  document.body.classList.add('config-window');
  const appEl = document.getElementById('app')!;
  // The real window is exactly the flyout size, so the harness box must be too:
  // the widget's responsive ladders read this box, not the viewport. The frame
  // is `position:fixed;inset:0` because in production the window IS the flyout,
  // so the harness re-anchors it to the box instead of the viewport.
  appEl.style.cssText =
    `position:fixed;left:40px;top:40px;width:${trayW}px;height:${trayH}px;display:flex;`;
  const trayDemoStyle = document.createElement('style');
  trayDemoStyle.textContent = '#app .tray-flyout { position: absolute; }';
  document.head.appendChild(trayDemoStyle);
  Promise.all([
    import('./demo/invoke.ts'),
    import('./TrayFlyout.tsx'),
  ]).then(([{ setPreviewDemo }, { default: TrayFlyout }]) => {
    setPreviewDemo(true);
    render(() => <TrayFlyout />, appEl);
  });
} else if (params.has('trayflyout')) {
  // Tray flyout: one tray widget in a small borderless panel anchored beside
  // the notification area. Shares the config window's `--ui-scale` surface
  // treatment; Rust reuses this one window for every tray icon.
  document.body.classList.add('config-window');
  import('./TrayFlyout.tsx').then(({ default: TrayFlyout }) => {
    render(() => <TrayFlyout />, document.getElementById('app')!);
  });
} else if (params.has('widgetwindow')) {
  // Widget pop-out window: a single widget, popped out into its own OS window.
  // Shares the config window's `--ui-scale` surface treatment; renders the
  // target widget bound to the shared per-view layout store.
  document.body.classList.add('config-window');
  import('./WidgetWindow.tsx').then(({ default: WidgetWindow }) => {
    render(() => <WidgetWindow />, document.getElementById('app')!);
  });
} else if (import.meta.env.DEV && params.has('configdemo')) {
  // DEV-ONLY browser harness: render the REAL unified config window in a plain
  // browser (no Tauri), to exercise the settings surfaces (theme/preset chrome,
  // contrast under light presets, etc.) without launching the app. Stub the IPC
  // internals + demo data so the window mounts. Stripped from production builds.
  if (!window.__TAURI_INTERNALS__) {
    window.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: 'config' }, currentWebview: { label: 'config' } },
      invoke: () => Promise.resolve(),
      transformCallback: () => 0,
    };
  }
  document.body.classList.add('config-window');
  Promise.all([
    import('./demo/invoke.ts'),
    import('./demo/views.ts'),
    import('./ConfigWindow.tsx'),
  ]).then(([{ setPreviewDemo }, { injectDemoViews }, { default: ConfigWindow }]) => {
    setPreviewDemo(true);
    injectDemoViews();
    render(() => <ConfigWindow />, document.getElementById('app')!);
  });
} else if (import.meta.env.DEV && params.has('connectdemo')) {
  // DEV-ONLY browser harness: render the REAL Connect window in a plain browser
  // (no Tauri), to check the provider picker and its brand marks — plate
  // contrast on both themes, sizing across the three places a mark renders
  // ([S-47]) — without launching the app. Add `&provider=imap` to land on the
  // preset page. Stripped from production builds.
  if (!window.__TAURI_INTERNALS__) {
    window.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: 'connect' }, currentWebview: { label: 'connect' } },
      invoke: () => Promise.resolve(),
      transformCallback: () => 0,
    };
  }
  document.body.classList.add('config-window');
  Promise.all([
    import('./demo/invoke.ts'),
    import('./ConnectWindow.tsx'),
  ]).then(([{ setPreviewDemo }, { default: ConnectWindow }]) => {
    setPreviewDemo(true);
    render(() => <ConnectWindow />, document.getElementById('app')!);
  });
} else if (params.has('config')) {
  // Config window: the whole surface is "popups + windows", so scale `body` by
  // `--ui-scale` via the `config-window` class (see App.css).
  document.body.classList.add('config-window');
  import('./ConfigWindow.tsx').then(({ default: ConfigWindow }) => {
    render(() => <ConfigWindow />, document.getElementById('app')!);
  });
} else {
  // Widget window: two transformed layers.
  //  - `#app.app-layer`  → scaled by `--monitor-scale` (widgets + chrome).
  //  - `.popup-layer`    → scaled by `--ui-scale` (portal mount for popups).
  document.body.classList.add('widget-window');
  const appEl = document.getElementById('app')!;
  appEl.classList.add('app-layer');
  const popupLayer = document.createElement('div');
  popupLayer.className = 'popup-layer';
  document.body.appendChild(popupLayer);
  // Safe-mode guard (1.19): bump THIS window's boot-attempt counter BEFORE loading
  // App so a crash during App import/render is counted. After SAFE_MODE_THRESHOLD
  // failures in a row, render the recovery UI instead; App.onMount clears the
  // counter once we're past the synchronous render crash point. Keyed per window
  // label so a normal multi-monitor startup (many windows sharing localStorage)
  // can't falsely trip it.
  let windowKey = 'widget';
  try { windowKey = getCurrentWindow().label; } catch { /* non-Tauri/preview → default */ }
  // Test-only (webdriver): expose the exact safe-mode counter key so the e2e can
  // seed it without re-deriving the window label (bare-specifier imports don't
  // resolve inside browser.execute).
  if (navigator.webdriver) (window as unknown as { __themiaWindowKey?: string }).__themiaWindowKey = windowKey;
  if (beginBoot(windowKey)) {
    import('./SafeMode.tsx').then(({ default: SafeMode }) => {
      render(() => <SafeMode windowKey={windowKey} />, appEl);
    });
  } else if (isPinLayerLabel(windowKey)) {
    // [S-45] A `monitor-{i}-pin` window is the always-on-top layer for that
    // monitor: same overlay geometry, only the pinned widgets, none of App's
    // per-monitor chrome or one-shot startup work. The window LABEL decides,
    // not a URL flag, so the two can never disagree.
    import('./PinLayer.tsx').then(({ default: PinLayer }) => {
      render(() => <PinLayer />, appEl);
    });
  } else {
    import('./App.tsx').then(({ default: App }) => {
      render(() => <App />, appEl);
    });
  }
}
