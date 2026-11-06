import { createSignal, createMemo, createEffect, onMount, onCleanup, Show, Switch, Match, Suspense } from 'solid-js';
import { TriangleAlert } from 'lucide-solid';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { getVersion } from '@tauri-apps/api/app';
import { invoke } from './ipc.ts';
import { listen } from '@tauri-apps/api/event';

import { updateWidgetConfig, updateWidgetLabel, getViewLayoutStore, findViewForWidget, atFreeWidgetLimit, isContainerType, TRAY_COLLECTION_ID } from './layout.ts';
import { addTrayWidget, initTraySync, setFlyoutSize, trayIconKeyOf } from './tray.ts';
import { resolveFlyoutSize } from './trayLayout.ts';
import { viewStore, setViewStore } from './viewState.ts';
import { globalDesign, setGlobalDesign, type GlobalDesign } from './design.ts';
import registry, { resolveHeaderMode, type WidgetDefinition } from './widgets/registry.tsx';
import SettingsBody, { type SettingsPage } from './components/SettingsModal.tsx';
import ViewSettingsBody from './components/ViewSettingsModal.tsx';
import DisplaySettingsBody from './components/DisplaySettingsModal.tsx';
import ConfigTree from './components/ConfigTree.tsx';
import ContextMenu from './ContextMenu.tsx';
import AddWidgetPage from './components/AddWidgetPage.tsx';
import { WidgetHeaderContext } from './components/widgetHeaderContext.tsx';
import { type ConfigTarget, parseTarget, encodeTarget, analyticsPath } from './config/target.ts';
import { reportNavigation } from './analytics.ts';
import { createNavHistory } from './config/navHistory.ts';
import { registerHotkey } from './input/hotkeys.ts';
import { postTransient, onTransient } from './sync.ts';
import { attach as attachLogger, logger, setLogForwarding } from './debugLog.ts';

// Wire the frontend log pipeline in the config window too, so the Logs page
// (Settings > Logs, which lives in this window) populates with records from
// this window's own logger calls. Widget windows attach via App.tsx.
attachLogger();
import { initLicense } from './license.ts';
import { initEscapeStack, pushEscape } from './input/escapeStack.ts';
import { initFocusStore } from './input/focusStore.ts';
import { initHotkeys } from './input/hotkeys.ts';
import { maybeInitLanguageFromSystem, t } from './i18n.ts';
import './App.css';
import './styles/config-base.css';
import './ConfigWindow.css';

const initialTarget = parseTarget(new URLSearchParams(window.location.search).get('target'));
const DEFAULT_SELECTION: ConfigTarget = { kind: 'app', page: 'general' };

function closeWindow() {
  logger.info('config', 'close requested');
  getCurrentWindow().close().catch(e =>
    logger.error('config', `close failed: ${String(e)}`)
  );
}

export default function ConfigWindow() {
  maybeInitLanguageFromSystem();
  initLicense();
  initEscapeStack();
  initHotkeys();
  initFocusStore();

  // [LOG-04] Same wiring as App.tsx: this window has its own JS heap, so the
  // console bridge needs its own gate push (the Rust invoke is idempotent).
  createEffect(() => {
    const on = globalDesign.loggingEnabled;
    setLogForwarding(on || import.meta.env.DEV);
    invoke('set_logging_enabled', { enabled: on }).catch(() => { /* ignore: non-tauri/preview */ });
  });

  const [selection, setSelection] = createSignal<ConfigTarget>(DEFAULT_SELECTION);
  const [expandedView, setExpandedView] = createSignal<string | null>(null);
  // Where the Back button on the Add Widget page returns to: the selection
  // active just before the picker was opened (defaults to the general page).
  const [backTarget, setBackTarget] = createSignal<ConfigTarget>(DEFAULT_SELECTION);
  // Imperative handle the ConfigTree registers on mount so F6 can move focus
  // back into it from the content pane.
  let treeFocusHandle: { focusActive(): void; focusFirst(): void } | null = null;
  let panelEl: HTMLDivElement | undefined;
  // True while a widget picked here is awaiting placement on a widget window.
  // Keeps the config window open so post-place navigation works and Esc has
  // a focused window to cancel from.
  const [placingActive, setPlacingActive] = createSignal(false);

  // App-settings page state (was previously owned by ConfigWindow's SettingsPage).
  const [autostart, setAutostart] = createSignal(false);
  // User-facing "share anonymous usage data", the inverse of the backend
  // opt-out flag. Seeded from disk at mount (default on / opt-out false).
  // Off until the backend says consent was granted: `unset` shares nothing,
  // so it must not render as an enabled toggle ([S-03]).
  const [analyticsEnabled, setAnalyticsEnabled] = createSignal(false);
  const [appVersion, setAppVersion] = createSignal('');
  const winLabel = getCurrentWindow().label;

  // Bump _refresh in a widget's config so its (separate) widget window re-fetches.
  function bumpWidgetRefresh(widgetId: string) {
    const viewId = findViewForWidget(widgetId);
    if (!viewId) return;
    const w = getViewLayoutStore(viewId).layout.widgets.find(x => x.id === widgetId);
    if (w) updateWidgetConfig(widgetId, { ...w.config, _refresh: Date.now() });
  }

  // Validate a target and apply it: activate the owning view (so the view
  // settings panel, which reads global proxies, resolves correctly), expand it
  // in the tree, and set the selection. Invalid targets fall back to the
  // default node. This is the apply primitive; it does NOT record navigation
  // history; user navigations go through `navigate()`, back/forward call this
  // directly so stepping through history never re-records.
  function applyTarget(target: ConfigTarget | null, opts?: { toggleExpand?: boolean }) {
    // Opening the picker: remember where we came from so Back can return there.
    if (target?.kind === 'add' && selection().kind !== 'add') setBackTarget(selection());
    if (!target || target.kind === 'app' || target.kind === 'add' || target.kind === 'display') {
      setSelection(target ?? DEFAULT_SELECTION);
      return;
    }
    if (target.kind === 'view') {
      if (!viewStore.list.some(s => s.id === target.viewId)) {
        logger.warn('config', `config target view not found: ${target.viewId}`);
        setSelection(DEFAULT_SELECTION);
        return;
      }
      setViewStore({ activeId: target.viewId });
      if (opts?.toggleExpand) {
        setExpandedView(c => c === target.viewId ? null : target.viewId);
      } else {
        setExpandedView(target.viewId);
      }
      setSelection(target);
      return;
    }
    // widget
    const viewId = findViewForWidget(target.widgetId);
    if (!viewId) {
      logger.warn('config', `config target widget not found: ${target.widgetId}`);
      setSelection(DEFAULT_SELECTION);
      return;
    }
    // A TRAY widget belongs to no view, so there is no active view to switch to;
    // making `activeId` the tray would point the view-settings panel and every
    // other `activeId` reader at a collection that is not a view (Tray.SPEC
    // [S-01]). Expanding its node is the only part that carries over.
    if (viewId !== TRAY_COLLECTION_ID) setViewStore({ activeId: viewId });
    setExpandedView(viewId);
    setSelection(target);
  }

  // ── Back/forward history ───────────────────────────────────────────────────
  // Every user navigation goes through `navigate()`; the mouse side buttons and
  // Alt+←/→ step through the recorded trail. back/forward apply the stored
  // target via the primitive so they never re-record.
  const history = createNavHistory();

  function navigate(target: ConfigTarget | null, opts?: { toggleExpand?: boolean }) {
    applyTarget(target, opts);
    // Record what actually resolved (invalid targets fall back to default).
    history.push(selection());
  }

  function goBack() {
    const target = history.back();
    if (target) applyTarget(target);
  }

  function goForward() {
    const target = history.forward();
    if (target) applyTarget(target);
  }

  // Report moves between config pages (analytics [S-23]). Driven off the
  // selection rather than `navigate()` so back/forward and deep links from the
  // grid are counted too. `analyticsPath` strips every per-install id: only the
  // settings page and the widget TYPE survive.
  let prevPath = analyticsPath(selection());
  createEffect(() => {
    const path = analyticsPath(selection());
    if (path === prevPath) return;
    reportNavigation(prevPath, path);
    prevPath = path;
  });

  // [S-13] Keep a tray widget's flyout open, at its real size and in its real
  // place, for as long as its config page is the one on screen. Configuring a
  // panel you cannot see is guesswork, and the panel is the thing being
  // configured. Selecting anything else — or closing this window — releases it.
  createEffect(() => {
    const sel = selection();
    const widget = sel.kind === 'widget' && findViewForWidget(sel.widgetId) === TRAY_COLLECTION_ID
      ? sel.widgetId
      : null;
    invoke('tray_flyout_preview', { widget }).catch(e =>
      logger.error('config', `tray_flyout_preview failed: ${String(e)}`)
    );
  });
  onCleanup(() => {
    invoke('tray_flyout_preview', { widget: null }).catch(() => {
      // ignore: the window is going away, so there is nobody left to tell.
    });
  });

  // Bump _refresh on the widget we just navigated *away from*; the unified
  // window lets the user hop between widgets without closing it.
  let prevSelection: ConfigTarget | null = null;
  createEffect(() => {
    const sel = selection();
    const prev = prevSelection;
    prevSelection = sel;
    if (prev && prev.kind === 'widget' &&
        !(sel.kind === 'widget' && sel.widgetId === prev.widgetId)) {
      bumpWidgetRefresh(prev.widgetId);
    }
  });

  // Keep the URL's `target` param in sync with the live selection. Otherwise
  // a `config-navigate` event swaps the selection in-place but the URL stays
  // pinned to whatever opened the window, which confuses external tools
  // (e.g. e2e helpers that identify the window by its URL params).
  createEffect(() => {
    const sel = selection();
    const sp = new URLSearchParams(window.location.search);
    sp.set('target', encodeTarget(sel));
    const next = `${window.location.pathname}?${sp.toString()}${window.location.hash}`;
    if (next !== window.location.pathname + window.location.search + window.location.hash) {
      window.history.replaceState(null, '', next);
    }
  });

  onMount(() => {
    // Instantiate every view's layout store so findViewForWidget works and
    // the tree can list each view's widgets.
    for (const s of viewStore.list) getViewLayoutStore(s.id);
    // The tray collection is not in the view list, so instantiate it explicitly:
    // its store is what `findViewForWidget` resolves a tray widget's config page
    // through, and what the tray page + tree node read (Tray.SPEC [S-01]).
    getViewLayoutStore(TRAY_COLLECTION_ID);
    // Editing the tray happens in THIS window, so this window must be one of the
    // ones mirroring it to the shell ([S-02]).
    initTraySync();

    // [AS-07] Reports whether the task will actually RUN at logon, not merely
    // whether one exists. A rejection means we could not ask (schtasks
    // unavailable), which is not the same as "off": log it rather than let the
    // toggle's `false` default pass for a confident answer.
    invoke('autostart_is_enabled')
      .then(v => setAutostart(v))
      .catch(e => logger.warn('config', `autostart_is_enabled failed, toggle may not reflect reality: ${e}`));
    invoke('analytics_consent').then(c => setAnalyticsEnabled(c === 'granted')).catch(() => {});
    getVersion().then(v => setAppVersion(v)).catch(() => {});

    navigate(initialTarget);

    // Escape: cancel an in-flight widget placement if one is active, otherwise
    // close the config window. The config window is minimized while placing so
    // this branch is mostly defensive; a minimized window can't catch keydown.
    // Esc during placement is actually handled by App.tsx on the focused widget
    // window, which broadcasts `picker-placing-end`; our listener below
    // unminimizes us in response.
    const offEscape = pushEscape(() => {
      if (placingActive()) {
        postTransient({ type: 'picker-placing-end' });
        setPlacingActive(false);
        getCurrentWindow().unminimize().catch(() => {});
        return true;
      }
      closeWindow();
      return true;
    });

    // Widget windows broadcast these when placement commits or is cancelled.
    // On commit, App.tsx also calls open_config_window which unminimizes us
    // via the existing-window branch, so calling unminimize here too is
    // idempotent. On cancel, only the broadcast fires, so we must unminimize
    // ourselves.
    const offPickerEnd = onTransient('picker-placing-end', () => {
      setPlacingActive(false);
      getCurrentWindow().unminimize().catch(() => {});
    });
    const offClipEnd = onTransient('clipboard-placing-end', () => {
      setPlacingActive(false);
      getCurrentWindow().unminimize().catch(() => {});
    });

    // A second open_config_window invocation focuses this window and emits
    // config-navigate instead of spawning a duplicate.
    const unlistenNav = listen<string | null>('config-navigate', (e) => {
      navigate(parseTarget(e.payload ?? null));
    });

    // F6: hop focus between the sidebar tree and the content pane. Mirrors the
    // standard Windows convention for cycling between major UI regions. We
    // listen at the window level so it works regardless of where focus sits.
    function onF6(e: KeyboardEvent) {
      if (e.key !== 'F6') return;
      e.preventDefault();
      const inTree = !!(document.activeElement as HTMLElement | null)?.closest('.config-tree');
      if (inTree) {
        // Move focus to the first focusable thing in the content pane.
        const target = panelEl?.querySelector<HTMLElement>(
          '.kbd-focusable, button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])',
        );
        target?.focus();
        return;
      }
      treeFocusHandle?.focusActive();
    }
    window.addEventListener('keydown', onF6);

    // Browser-style back/forward across the config navigation trail.
    //  - Alt+←/→ (Windows convention) via the shared hotkey registry.
    //  - Mouse side buttons (XButton1=3 back, XButton2=4 forward). The SPA only
    //    uses replaceState, so the webview has no real history to fight over.
    const offNavBack = registerHotkey('alt+arrowleft', () => goBack());
    const offNavForward = registerHotkey('alt+arrowright', () => goForward());
    function onSideButton(e: MouseEvent) {
      if (e.button === 3) { e.preventDefault(); goBack(); }
      else if (e.button === 4) { e.preventDefault(); goForward(); }
    }
    window.addEventListener('mouseup', onSideButton);

    // Bump _refresh on the current widget when the window itself closes.
    const win = getCurrentWindow();
    const unlistenClose = win.onCloseRequested(() => {
      logger.info('config', 'onCloseRequested fired');
      const sel = selection();
      if (sel.kind === 'widget') bumpWidgetRefresh(sel.widgetId);
    });

    return () => {
      offEscape();
      offPickerEnd();
      offClipEnd();
      offNavBack();
      offNavForward();
      window.removeEventListener('keydown', onF6);
      window.removeEventListener('mouseup', onSideButton);
      unlistenNav.then(fn => fn()).catch(() => {});
      unlistenClose.then(fn => fn()).catch(() => {});
    };
  });

  const settingsPage = (): SettingsPage => {
    const s = selection();
    return s.kind === 'app' ? s.page : 'general';
  };

  // The container the Add Widget page is filling, when it was opened from one
  // ([S-34]); null for the normal click-to-place flow. RESOLVED, not just the
  // raw id: a target naming a widget that no longer exists (or is no longer a
  // container) degrades to the plain picker, so the banner and the pick handler
  // can never disagree about whether this pick lands in a container.
  const intoContainer = createMemo<{ id: string; label: string } | null>(() => {
    const s = selection();
    const id = s.kind === 'add' ? s.into : undefined;
    if (!id) return null;
    const viewId = findViewForWidget(id);
    const w = viewId ? getViewLayoutStore(viewId).layout.widgets.find(x => x.id === id) : undefined;
    if (!w || !isContainerType(w.type)) {
      logger.warn('config', `add-into target is not a live container: ${id}`);
      return null;
    }
    return { id, label: w.label || registry[w.type]?.label || w.type };
  });

  const selectedWidget = createMemo(() => {
    const s = selection();
    if (s.kind !== 'widget') return undefined;
    const viewId = findViewForWidget(s.widgetId);
    if (!viewId) return undefined;
    return getViewLayoutStore(viewId).layout.widgets.find(w => w.id === s.widgetId);
  });

  // Picking a widget on the Add Widget page broadcasts it to the widget
  // windows (they enter placing mode) and minimizes this window so the user
  // can see the grid. After placement, App.tsx's handlePlace calls
  // open_config_window which finds this still-open (just minimized) window,
  // unminimizes it, and emits config-navigate to swap us onto the new widget's
  // config page. On cancel, the picker-placing-end listener unminimizes us.
  function handlePickWidget(type: string, def: WidgetDefinition) {
    // Targeting the tray (Tray.SPEC [S-01]): like the container case below, a
    // tray widget has no position to choose, so it is created here and we land
    // on its config page. Deep-cloned for the same reason.
    if (selection().kind === 'add' && (selection() as { toTray?: boolean }).toTray) {
      if (atFreeWidgetLimit()) { navigate({ kind: 'app', page: 'license' }); return; }
      const newId = addTrayWidget(type, JSON.parse(JSON.stringify(def.defaultConfig ?? {})));
      if (!newId) {
        logger.error('config', `add to tray failed: type=${type}`);
        return;
      }
      navigate({ kind: 'widget', widgetId: newId });
      return;
    }
    // Targeting a container ([S-34]): the widget has no position to choose, so
    // it is created inside the container right here and we land on its config
    // page. No placing broadcast, no minimize -- the user never leaves this
    // window.
    const into = intoContainer();
    if (into) {
      // Same limit routing as click-to-place: the user meets the paywall, not
      // a silent no-op.
      if (atFreeWidgetLimit()) { navigate({ kind: 'app', page: 'license' }); return; }
      const viewId = findViewForWidget(into.id);
      // Deep-cloned: the registry's defaultConfig object must never become a
      // widget instance's live config (the placing path clones via the
      // transient message; this path has to do it itself).
      const newId = viewId
        ? getViewLayoutStore(viewId).addChildWidget(type, into.id, JSON.parse(JSON.stringify(def.defaultConfig ?? {})))
        : null;
      if (!newId) {
        logger.error('config', `add into container failed: type=${type} container=${into.id}`);
        return;
      }
      navigate({ kind: 'widget', widgetId: newId });
      return;
    }
    postTransient({
      type: 'picker-place-start',
      widgetType: type,
      defaultSize: def.defaultSize,
      defaultConfig: def.defaultConfig,
    });
    setPlacingActive(true);
    getCurrentWindow().minimize().catch(e =>
      logger.error('config', `minimize on pick failed: ${String(e)}`)
    );
  }

  return (
    <>
    <div class="config-root">
      {/* Developer Mode unlocks experimental, not-fully-tested surfaces
          (pre-release widgets, extra providers, demo/dev views). A primary-
          accented banner spans the FULL width at the very top (over the
          sidebar tree too) whenever devMode is on, so it's unmissable on every
          config page. */}
      <Show when={globalDesign.devMode}>
        <div class="config-dev-banner" role="status">
          <TriangleAlert size={14} strokeWidth={1.5} />
          <span>{t('settings.devBanner.text')}</span>
        </div>
      </Show>
      <div class="config-shell">
      {/* The Add Widget page is full-bleed: it hides the sidebar tree and
          offers a Back button instead (rendered inside AddWidgetPage). */}
      <Show when={selection().kind !== 'add'}>
        <ConfigTree
          selection={selection}
          expanded={expandedView}
          onSelect={(t) => navigate(t, { toggleExpand: true })}
          onToggleExpand={(viewId) => setExpandedView(c => c === viewId ? null : viewId)}
          registerHandle={(h) => { treeFocusHandle = h; }}
        />
      </Show>
      <div class="config-panel" ref={panelEl}>
        <Switch>
          <Match when={selection().kind === 'add'}>
            <AddWidgetPage
              onPick={handlePickWidget}
              onBack={() => navigate(backTarget())}
              intoContainer={intoContainer()?.label}
              intoTray={selection().kind === 'add' && (selection() as { toTray?: boolean }).toTray}
            />
          </Match>

          <Match when={selection().kind === 'app'}>
            <SettingsBody
              activePage={settingsPage}
              setActivePage={(page) => navigate({ kind: 'app', page })}
              setGlobalDesign={(key, value) =>
                setGlobalDesign(key as keyof GlobalDesign, value as GlobalDesign[keyof GlobalDesign])}
              autostart={autostart}
              setAutostart={setAutostart}
              analyticsEnabled={analyticsEnabled}
              setAnalyticsEnabled={setAnalyticsEnabled}
              winLabel={winLabel}
              appVersion={appVersion}
            />
          </Match>

          <Match when={selection().kind === 'view'}>
            <ViewSettingsBody
              onAfterDelete={() => navigate(DEFAULT_SELECTION)}
            />
          </Match>

          <Match when={selection().kind === 'display'}>
            {/* keyed: remount when the selected display changes so the panel
                re-reads its monitor-scoped state cleanly. */}
            <Show when={selection().kind === 'display' && (selection() as { monitorId: string }).monitorId} keyed>
              {(monitorId) => <DisplaySettingsBody monitorId={monitorId} />}
            </Show>
          </Match>

          <Match when={selection().kind === 'widget'}>
            {/* keyed: fully remount the config component when the selected
                widget changes, so components with internal state seeded from
                `config` don't leak one widget's values into another. */}
            <Show
              when={selectedWidget()}
              keyed
              fallback={<div class="config-window-empty">{t('app.widgetNotFound')}</div>}
            >
              {(widget) => {
                const def = registry[widget.type];
                if (!def?.configComponent) {
                  return (
                    <div class="config-window-empty">{t('app.configNoOptions')}</div>
                  );
                }
                const C = def.configComponent;
                return (
                  <WidgetHeaderContext.Provider value={{
                    label: () => widget.label ?? '',
                    defaultLabel: () => def.label ?? 'Default',
                    noHeader: () => !!def.noHeader,
                    headerMode: () => resolveHeaderMode(widget.config, def),
                    canTransparent: () => !!def.canTransparent,
                    transparentBg: () => widget.config?.transparentBg === true,
                    onLabelChange: (v) => updateWidgetLabel(widget.id, v),
                    onHeaderModeChange: (mode) => {
                      // Write `headerMode` going forward; drop the stale legacy
                      // `hideHeader` key from this config (WidgetHeader.SPEC [S-15]).
                      const { hideHeader: _drop, ...rest } = (widget.config ?? def.defaultConfig ?? {});
                      void _drop;
                      updateWidgetConfig(widget.id, { ...rest, headerMode: mode });
                    },
                    // [S-32] Stored as a plain boolean on the instance config;
                    // the grid reads it back with a strict `=== true`, so a
                    // legacy/absent key is opaque, never "maybe transparent".
                    onTransparentBgChange: (v) => {
                      updateWidgetConfig(widget.id, {
                        ...(widget.config ?? def.defaultConfig ?? {}),
                        transparentBg: v,
                      });
                    },
                    // [S-45] Only a TOP-LEVEL DESKTOP widget owns a layer: a
                    // tray widget lives in a flyout, and a container's child
                    // follows its ancestor, so neither offers the row.
                    pinnable: () =>
                      widget.parentId === undefined &&
                      findViewForWidget(widget.id) !== TRAY_COLLECTION_ID,
                    alwaysOnTop: () => widget.config?.alwaysOnTop === true,
                    onAlwaysOnTopChange: (v) => {
                      updateWidgetConfig(widget.id, {
                        ...(widget.config ?? def.defaultConfig ?? {}),
                        alwaysOnTop: v,
                      });
                    },
                    // Tray.SPEC [S-07]: null for a desktop widget, which is
                    // what keeps the flyout-size rows off its page.
                    trayFlyout: () =>
                      findViewForWidget(widget.id) === TRAY_COLLECTION_ID
                        ? resolveFlyoutSize(widget.config, def.defaultSize)
                        : null,
                    onTrayFlyoutChange: (size) => setFlyoutSize(widget.id, size),
                    // [S-14] Raw stored values, not the resolved ones: the field
                    // shows what the USER set, and the fallback is the
                    // placeholder beside it. Resolving here would make an
                    // inherited name look like an explicit one, so clearing it
                    // back to the default would be impossible.
                    trayName: () => typeof widget.config?.trayName === 'string' ? widget.config.trayName : '',
                    trayNameFallback: () => widget.label?.trim() || def.label || widget.type,
                    trayIcon: () => trayIconKeyOf(widget),
                    onTrayNameChange: (v) => {
                      updateWidgetConfig(widget.id, { ...(widget.config ?? {}), trayName: v });
                    },
                    onTrayIconChange: (key) => {
                      updateWidgetConfig(widget.id, { ...(widget.config ?? {}), trayIcon: key });
                    },
                  }}>
                    {/* [R1] Every config panel is `lazy`, so its chunk is not in
                        the overlay renderers that never show one. This is the
                        only place a config panel is rendered, so one boundary
                        covers all of them. The fallback is an empty box rather
                        than a spinner: the chunk is local and resolves within a
                        frame or two, and a flashing spinner would be worse than
                        a brief blank panel. */}
                    <Suspense fallback={<div class="config-window-empty" />}>
                      <C
                        config={widget.config ?? def.defaultConfig ?? {}}
                        onChange={(cfg: Record<string, unknown>) => updateWidgetConfig(widget.id, cfg)}
                        _widgetId={widget.id}
                        _viewId={findViewForWidget(widget.id) ?? ''}
                        globals={{
                          // Always opens the full Connections page; per-widget
                          // provider scoping lives only in the widget's own toggle
                          // list, never on the Connections page itself.
                          onManageAccounts: () => {
                            navigate({ kind: 'app', page: 'accounts' });
                          },
                          // Container widgets ([S-34]): open the Add Widget
                          // page bound to this container, so the picked widget
                          // is created inside it.
                          onAddToContainer: (containerId: string) => {
                            navigate({ kind: 'add', into: containerId });
                          },
                        }}
                      />
                    </Suspense>
                  </WidgetHeaderContext.Provider>
                );
              }}
            </Show>
          </Match>
        </Switch>
      </div>
    </div>
    </div>
    <ContextMenu />
    </>
  );
}
