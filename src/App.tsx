import { createSignal, createMemo, createEffect, onMount, onCleanup } from 'solid-js';
import { For, Show } from 'solid-js';
import WidgetGrid from './WidgetGrid.tsx';
import { beginPointerDrag } from './input/pointerDrag.ts';
import { markBootSuccess } from './safeMode.ts';
import type { PlacingState } from './placing.ts';
import ViewRoot from './ViewRoot.tsx';
import ContextMenu from './ContextMenu.tsx';
import { addWidget, removeWidgetTree, removeWidgetFromKey, getViewLayoutStore, atFreeWidgetLimit } from './layout.ts';
import { initTraySync } from './tray.ts';
import { pxSizeToFrac } from './gridGeometry.ts';
import { ctxMenu, closeCtxMenu, showCtxMenu } from './ctxMenuStore.ts';
import { applyGlobalDesign, globalDesign, setGlobalDesign, introAnimFor } from './design.ts';
import { beginBootOverlay, bootOverlayActive, replayStartupAnimation, type IntroAnimMode } from './bootOverlay.ts';
import { playStartupSound } from './startupSound.ts';
import HyperspaceOverlay from './components/HyperspaceOverlay.tsx';
import ToggleSwitch from './components/ToggleSwitch.tsx';
import { viewStore, switchView, addView, atFreeViewLimit, nextViewName, duplicateView, moveView, viewReorderIndices, initForMonitor, defaultNewDisplayView, isDemoView, getViewBarPosition, setViewBarPosition, getViewBarSize, getViewBarIconOnly, getViewBarHidden, getMonitorDisabled, getViewBarVisibleViewsMap, getCurrentMonitorId, syncMonitorDisabledToBackend, type ViewBarPosition } from './views.ts';
import { viewInitials } from './viewState.ts';
import { injectDemoViews } from './demo/views.ts';
import { postTransient, onTransient } from './sync.ts';
import { widgetClipboard, pasteWidget, hasClipboard } from './widgetClipboard.ts';
import { VIEW_ICON_MAP } from './components/IconPicker.tsx';
import { type ConfigTarget, encodeTarget } from './config/target.ts';
import { invoke } from './ipc.ts';

import { getCurrentWindow } from '@tauri-apps/api/window';
import { listen } from '@tauri-apps/api/event';
import { Settings, X, Plus, Monitor, Copy, GripVertical, Pause, Layers, Grid3x3 } from 'lucide-solid';
import { regenerateDevViewsOnBoot } from './dev/labView.ts';
import { initLicense } from './license.ts';
import { initNativeDragListener } from './nativeDragDrop.ts';
import { undoLast } from './operationLog.ts';
import { showToast } from './toastStore.ts';
import Toast from './components/Toast.tsx';
import { initEscapeStack, pushEscape, onEscapeStackActive } from './input/escapeStack.ts';
import { initHotkeys, registerHotkey } from './input/hotkeys.ts';
import { initFocusStore, inputFocused, hoverFocusable } from './input/focusStore.ts';
import { useInteractive } from './input/interactivityStore.ts';
import { editMode, setEditMode } from './editMode.ts';
import { anyPinned } from './pinLayer.ts';
import { installOverlayBridge } from './overlayBridge.ts';
import AnalyticsConsent from './components/AnalyticsConsent.tsx';
import { attach as attachLogger, logger, setLogForwarding } from './debugLog.ts';
import { initGameMode, gameModeSuspended } from './gameMode.ts';
import { startAnalytics } from './analytics.ts';
import { t, tm } from './i18n.ts';
import './App.css';


// Wire frontend log pipeline once (runs at module init, before App mounts).
attachLogger();

/**
 * One-shot migration: legacy RSS widgets stored a single `feedUrl` string.
 * Feeds are now managed connections (provider 'rss'). For each rss widget that
 * still has a `feedUrl` and no `feedIds`, create a feed connection from that URL
 * and rewrite the widget to reference it. Idempotent; on failure (e.g. offline)
 * the `feedUrl` is left in place so the widget's fallback keeps it working.
 */
async function migrateRssFeeds(): Promise<void> {
  const cache = new Map<string, string>(); // url → new feed account id
  for (const s of viewStore.list) {
    const store = getViewLayoutStore(s.id);
    for (const w of store.layout.widgets) {
      if (w.type !== 'rss') continue;
      const cfg = w.config as Record<string, unknown> | undefined;
      const url = String(cfg?.feedUrl ?? '').trim();
      if (!url || (Array.isArray(cfg?.feedIds) ? cfg.feedIds.length : 0) > 0) continue;
      try {
        let id = cache.get(url);
        if (!id) {
          const acc = await invoke('email_add_rss_feed', { url });
          id = acc.id;
          cache.set(url, id);
        }
        const { feedUrl: _drop, ...rest } = cfg!;
        void _drop;
        store.updateWidgetConfig(w.id, { ...rest, feedIds: [id] });
      } catch {
        // Leave feedUrl as the fallback; retry on a later launch.
      }
    }
  }
}

export default function App() {
  // Edit mode is a module singleton (editMode.ts) so the always-on-top pin
  // overlay ([S-45]) reads the same flag and can stand down while editing.
  const [placing,        setPlacing]        = createSignal<PlacingState | null>(null);

  // Boot overlay ([S-16] hyperspace / [S-23] hacker), armed in the component
  // body, BEFORE the grid renders, so widgets see the active state and delay
  // their intro past the overlay's reveal point. One-shot per window; inert
  // unless the startup animation is an overlay style; suppressed under reduced
  // motion (mirrors [S-15]).
  beginBootOverlay(
    globalDesign.widgetStartupAnimation,
    window.matchMedia('(prefers-reduced-motion: reduce)').matches,
  );
  // Matching startup sound ([S-26]): one-shot per window boot, alongside the
  // intro/overlay. Opt-in + reduced-motion gated inside playStartupSound, so it
  // mirrors the animation (no animation → no sound). The component body runs
  // once per window mount, so this fires exactly once like beginBootOverlay.
  //
  // SINGLE PLAYER across windows: the animation replays on EVERY monitor's
  // widget window, but only the PRIMARY one (`monitor-0`) emits the sound. N
  // windows each playing the same cue would echo/overlap on a multi-monitor
  // setup. One deterministic player, no cross-window election race. (Within a
  // window, the player is also single-flight so re-triggers never stack.)
  const isPrimarySoundWindow = getCurrentWindow().label === 'monitor-0';
  const playStartupSoundHere = (reducedMotion: boolean) =>
    playStartupSound(globalDesign.widgetStartupAnimation, globalDesign.widgetStartupAnimationMs, {
      enabled: globalDesign.widgetStartupSound && isPrimarySoundWindow,
      reducedMotion,
    });
  playStartupSoundHere(window.matchMedia('(prefers-reduced-motion: reduce)').matches);
  // Replay an intro in this widget window for `mode` ([S-16]/[S-24]): the
  // settings preview buttons (per section) and the real screen-unlock event all
  // funnel through here. Leave edit mode first so the animation plays on a clean
  // desktop (no toolbars / edit chrome). Only the STARTUP mode carries the
  // matching sound ([S-26]; the sound is a startup-only cue).
  const playIntroReplay = (mode: IntroAnimMode) => {
    setEditMode(false);
    const reducedMotion = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
    replayStartupAnimation(introAnimFor(mode), reducedMotion, mode);
    if (mode === 'startup') playStartupSoundHere(reducedMotion);
  };
  // Settings preview button / animation change (config window) → replay the
  // matching section's animation here ([S-16]/[S-23]/[S-24]).
  const offAnimReplay = onTransient('startup-anim-replay', (m) => playIntroReplay(m.mode ?? 'startup'));
  onCleanup(offAnimReplay);
  // Returning from a locked screen replays the UNLOCK animation ([S-16]): the
  // Rust session-notification watcher broadcasts 'screen-unlocked' on
  // WTS_SESSION_UNLOCK. Per window, so every monitor warps in. Disabled by
  // setting the Unlock animation to 'none' (suppressed downstream, same as
  // reduced-motion / mid-show).
  let offUnlock: (() => void) | undefined;
  listen('screen-unlocked', () => playIntroReplay('unlock'))
    .then(fn => { offUnlock = fn; })
    .catch(() => {});
  onCleanup(() => offUnlock?.());
  // [S-27] True only in the window that claimed the first-run consent prompt.
  const [consentPrompt, setConsentPrompt] = createSignal(false);

  const [barDragging, setBarDragging] = createSignal(false);
  const [barSnapTarget, setBarSnapTarget] = createSignal<ViewBarPosition | null>(null);

  const effectiveBarPos = createMemo(() => (barDragging() && barSnapTarget()) ? barSnapTarget()! : getViewBarPosition());

  // Per-display whitelist of which views appear in the bar. The whitelist is
  // honored in edit mode too; the bar shows only the views selected for this
  // display in settings, never the full list. (Hidden views remain reachable
  // for reorder/delete from the config window's view tree.)
  const visibleBarViews = createMemo(() => {
    const monId = getCurrentMonitorId();
    const map = getViewBarVisibleViewsMap();
    const whitelist = monId ? map[monId] : undefined;
    if (!whitelist || whitelist.length === 0) return viewStore.list;
    const set = new Set(whitelist);
    return viewStore.list.filter(v => set.has(v.id));
  });

  // True when this display has an explicit visible-views whitelist (≥1 entry).
  // A deliberate single-view whitelist still shows the bar; the >1 auto-hide
  // only applies to the default "show all" case where one view is pointless.
  const hasExplicitWhitelist = createMemo(() => {
    const monId = getCurrentMonitorId();
    const wl = monId ? getViewBarVisibleViewsMap()[monId] : undefined;
    return !!wl && wl.length > 0;
  });

  const viewBarStyle = createMemo(() => {
    const pos = effectiveBarPos();
    const s: Record<string, string> = {};
    if (pos === 'left-center' || pos === 'right-center') {
      s.top = '50%'; s.bottom = 'auto';
      s.transform = 'translateY(-50%)';
    } else if (pos.startsWith('top')) {
      s.top = '16px'; s.bottom = 'auto'; s.transform = 'none';
    } else {
      // In edit mode the action bar is docked bottom-center; lift every bottom
      // view-bar position above it so they don't overlap.
      s.bottom = editMode() ? '64px' : '16px'; s.top = 'auto'; s.transform = 'none';
    }
    if (pos === 'left-center') { s.left = '16px'; s.right = 'auto'; }
    else if (pos === 'right-center') { s.right = '16px'; s.left = 'auto'; }
    else if (pos.endsWith('left')) { s.left = '16px'; s.right = 'auto'; }
    else if (pos.endsWith('right')) { s.right = '16px'; s.left = 'auto'; }
    else { s.left = '50%'; s.right = 'auto'; s.transform = pos.startsWith('top') || pos.startsWith('bottom') ? 'translateX(-50%)' : s.transform; }
    return s;
  });

  const isVerticalBar = createMemo(() => {
    const pos = effectiveBarPos();
    return pos === 'left-center' || pos === 'right-center';
  });

  function viewIconSize(): number {
    const s = getViewBarSize();
    return s === 'large' ? 16 : s === 'xl' ? 20 : 13;
  }

  // ── Interactivity reasons ──
  // Each surface publishes its own reason to the shared interactivity store;
  // the aggregate drives the Rust `set_interactive` bridge, which strips
  // WS_EX_NOACTIVATE so the window can take keyboard focus.
  //
  // NOTE: the context menu is deliberately NOT a reason here. It only needs
  // the window REGION expanded so the menu (which spills past widget bounds)
  // is visible; it does NOT need the window to activate. Activating on
  // right-click is exactly what made the overlay jump in front of other
  // apps. The rect-push effect below tracks `ctxMenu.menu` and the backdrop
  // is tagged `data-themia-interactive`, so the region covers the whole
  // window while the menu is open, no activation required.
  useInteractive('editMode',     editMode);
  useInteractive('input',        inputFocused);
  useInteractive('hover-input',  hoverFocusable);
  useInteractive('placing',      () => !!placing());

  const winLabel = getCurrentWindow().label;

  // Active view, re-keyed when demo mode flips so the widget subtree remounts
  // and refetches with the new data source immediately (no window reload). The
  // memo only recomputes (and thus returns a new object reference) when the
  // active view, the view list, or demo mode actually change.
  const activeViewKeyed = createMemo(() => {
    void globalDesign.demoMode;
    const v = viewStore.list.find(s => s.id === viewStore.activeId);
    return v ? { ...v } : undefined;
  });

  // ── Per-monitor disable migration sync ──
  // Per-monitor disable is now enforced by destroying the overlay window for
  // disabled monitors entirely (Rust persists the list in a JSON file and
  // skips window creation at startup). Push the current localStorage state
  // once so the Rust mirror catches up. This covers (a) first launch after this
  // code ships, when the user already has disabled entries in localStorage
  // from the previous build, and (b) any future drift. Idempotent on the
  // backend. Base overlay only: the pin overlay is a mirror, not an authority.
  syncMonitorDisabledToBackend();

  // ── Rust bridges ──
  // Interactivity, text-input focus, per-monitor scale and the desktop
  // pass-through hit-mask ([S-17]) all live in overlayBridge.ts, shared with
  // the always-on-top pin overlay ([S-45]) so both windows mask identically.
  installOverlayBridge({
    extraDeps: () => {
      void editMode();
      void placing();
    },
  });

  // [S-45] Lazily build / tear down the monitor's always-on-top pin overlay.
  // Driven from the BASE overlay only (the pin window runs PinLayer, which
  // never calls this), and idempotent in Rust, so every monitor's base window
  // reporting the same global answer costs nothing. Keyed on "any view holds a
  // pinned top-level widget" rather than the ACTIVE view, so switching views
  // does not destroy and rebuild a WebView2 renderer.
  createEffect(() => {
    const enabled = viewStore.list.some(v => anyPinned(getViewLayoutStore(v.id).layout.widgets));
    invoke('set_pin_layer', { enabled })
      .catch(e => logger.error('tauri', `set_pin_layer failed: ${e}`));
  });

  // ── Escape stack registrations ──
  // Each effect pushes a handler only while its state is active.
  // LIFO order: the most-recently-opened surface handles Escape first.
  createEffect(() => {
    if (!placing()) return;
    const off = pushEscape(() => {
      const wasClip = placing()?.fromClipboard;
      setPlacing(null);
      postTransient({ type: wasClip ? 'clipboard-placing-end' : 'picker-placing-end' });
      return true;
    });
    onCleanup(off);
  });
  createEffect(() => {
    if (!ctxMenu.menu) return;
    const off = pushEscape(() => { closeCtxMenu(); return true; });
    onCleanup(off);
  });
  createEffect(() => {
    if (!inputFocused()) return;
    const off = pushEscape(() => {
      (document.activeElement as HTMLElement | null)?.blur();
      return true;
    });
    onCleanup(off);
  });
  createEffect(() => {
    if (!editMode()) return;
    const off = pushEscape(() => { setEditMode(false); return true; });
    onCleanup(off);
  });

  // Push the SSRF policy to the Rust backend. The flag is owned here (persisted
  // in the design store); the backend defaults to secure (block) until told.
  // Runs on startup and whenever the user toggles the setting.
  createEffect(() => {
    const allow = globalDesign.allowPrivateNetworkAccess;
    invoke('set_ssrf_bypass', { enabled: allow }).catch(() => { /* ignore: non-tauri/preview */ });
  });

  // [LOG-04] Push the logging master switch to the Rust gate and the frontend
  // console bridge. Off by default; the backend drops every record until
  // told otherwise. Debug builds always log (`import.meta.env.DEV` forces the
  // frontend bridge; the Rust gate is bypassed under debug_assertions).
  createEffect(() => {
    const on = globalDesign.loggingEnabled;
    setLogForwarding(on || import.meta.env.DEV);
    invoke('set_logging_enabled', { enabled: on }).catch(() => { /* ignore: non-tauri/preview */ });
  });

  // Listen for OS theme changes when using "system" theme
  onMount(() => {
    // We've mounted past the synchronous render/early-init crash point, so clear
    // THIS window's safe-mode boot-attempt counter so a normal boot never accrues
    // (1.19; keyed per window label to match main.tsx's beginBoot).
    try { markBootSuccess(getCurrentWindow().label); } catch { markBootSuccess('widget'); }
    // Start listening for native OLE drag/drop events from Rust
    initNativeDragListener();
    initGameMode();

    // Put the tray widgets' icons in the notification area (Tray.SPEC [S-02]).
    // Every overlay window does this: they all read the same collection, so they
    // all push the same set and `tray_sync` diffs it away — and the tray keeps
    // working no matter which window happens to close.
    initTraySync();

    // Anonymous usage analytics. Every monitor's window runs this; the backend
    // hands the work to whichever asks first, so a multi-monitor setup still
    // reports one heartbeat, not one per screen (analytics SPEC [S-14]).
    onCleanup(startAnalytics());

    // [S-27] The first-run consent question, asked in exactly one window for
    // the same reason: the backend hands the prompt to whichever monitor asks
    // first. Until it is answered nothing is sent and no install id exists.
    invoke('analytics_claim_consent_prompt')
      .then(setConsentPrompt)
      .catch(e => logger.error('analytics', `analytics_claim_consent_prompt failed: ${e}`));

    // Reopen devtools on startup if user had them open last session, but only
    // when Developer Mode is on, so a disabled toggle fully hides the feature.
    if (globalDesign.devMode && localStorage.getItem(`themia-devtools-open-${winLabel}`) === 'true') {
      invoke('show_devtools', { label: winLabel }).catch(e => logger.error('tauri', `show_devtools failed: ${e}`));
    }

    // Inject demo views before monitor init so they're available for assignment
    if (globalDesign.demoMode) injectDemoViews();

    // Dev-only: if development views are enabled, regenerate them from the
    // registry on boot so newly added widgets always appear.
    if (globalDesign.devMode) regenerateDevViewsOnBoot();

    // Initialize per-monitor view assignment. [MI-01] `get_monitor_id` resolves
    // to the monitor's STABLE hardware id; `initForMonitor` internally waits for
    // the #70 key migration before reading any per-monitor map, so this stays
    // fire-and-forget.
    invoke('get_monitor_id', { label: winLabel })
      .then(monitorId => { if (monitorId) return initForMonitor(monitorId); })
      .catch(e => logger.error('tauri', `get_monitor_id failed: ${e}`));

    const mq = window.matchMedia('(prefers-color-scheme: dark)');
    const handler = () => { if (globalDesign.theme === 'system') applyGlobalDesign(); };
    mq.addEventListener('change', handler);
    onCleanup(() => mq.removeEventListener('change', handler));

    // Re-init this window when the monitor it covers changes (display
    // connect/disconnect/resolution change). The Rust side dispatches this
    // CustomEvent instead of doing a hard reload.
    const onDisplayChanged = () => {
      invoke('get_monitor_id', { label: winLabel })
        .then(monitorId => { if (monitorId) return initForMonitor(monitorId); })
        .catch(e => logger.error('tauri', `get_monitor_id failed: ${e}`));
    };
    window.addEventListener('themia:display-changed', onDisplayChanged);
    onCleanup(() => window.removeEventListener('themia:display-changed', onDisplayChanged));
    initLicense();
  });

  function handlePlace(x: number, y: number) {
    const p = placing();
    if (!p) return;
    if (atFreeWidgetLimit()) {
      setPlacing(null);
      openConfigWindow({ kind: 'app', page: 'license' });
      return;
    }
    if (p.fromClipboard) {
      // Capture cut metadata before pasteWidget clears the clipboard.
      const clip = widgetClipboard();
      const cutSourceId  = p.mode === 'cut' ? (clip.sourceWidgetId ?? p.sourceId) : undefined;
      const cutSourceKey = p.mode === 'cut' ? clip.sourceLayoutKey : undefined;
      // For CUT: remove source FIRST so the global limit check inside
      // pasteWidget sees the post-cut count. An area group leaves WITH its
      // members ([S-41]) — the paste re-creates them, so the originals must go
      // rather than pop back to the desktop the way a delete leaves them.
      if (p.mode === 'cut' && cutSourceId) {
        if (cutSourceKey) removeWidgetFromKey(cutSourceKey, cutSourceId, true);
        else              removeWidgetTree(cutSourceId);
      }
      // A group brings its members, so the paste can need several slots; it
      // refuses as a whole rather than landing half a group ([S-41]).
      if (!pasteWidget(x, y)) {
        setPlacing(null);
        postTransient({ type: 'clipboard-placing-end' });
        if (hasClipboard()) openConfigWindow({ kind: 'app', page: 'license' });
        return;
      }
      setPlacing(null);
      postTransient({ type: 'clipboard-placing-end' });
      return;
    }
    const newId = addWidget(p.type, { x, y, w: p.size.w, h: p.size.h }, p.defaultConfig);
    setPlacing(null);
    // Other widget windows entered placing mode via `picker-place-start`;
    // tell them to exit now that the widget has landed here.
    postTransient({ type: 'picker-placing-end' });
    if (newId) {
      openConfigWindow({ kind: 'widget', widgetId: newId });
    }
  }

  function startClipboardPlacing(mode: 'copy' | 'cut', sourceId?: string) {
    const clip = widgetClipboard();
    if (!clip.widget) return;
    // Clipboard stores px size; convert to a fraction of THIS screen ([S-12]).
    setPlacing({
      fromClipboard: true,
      mode,
      sourceId,
      size: pxSizeToFrac({ width: clip.widget.wPx, height: clip.widget.hPx }),
    });
    // Tell other windows to enter placing mode too so the user can paste on
    // any display by clicking there.
    postTransient({
      type: 'clipboard-placing-start',
      mode,
      wPx: clip.widget.wPx,
      hPx: clip.widget.hPx,
    });
  }

  // Receive clipboard placing start/end from other windows.
  const cleanupPlacingStart = onTransient('clipboard-placing-start', (msg) => {
    const clip = widgetClipboard();
    if (!clip.widget) return;
    if (!editMode()) setEditMode(true);
    setPlacing({
      fromClipboard: true,
      mode: msg.mode,
      size: pxSizeToFrac({ width: msg.wPx, height: msg.hPx }),
    });
    // See picker-place-start: claim focus so Esc reaches the App.tsx handler.
    getCurrentWindow().setFocus().catch(() => {});
  });
  const cleanupPlacingEnd = onTransient('clipboard-placing-end', () => {
    if (placing()?.fromClipboard) setPlacing(null);
  });

  // The widget picker is the "Add Widget" page of the config window. When the
  // user picks a widget there it broadcasts `picker-place-start`; every widget
  // window enters placing mode so the user can drop the widget on any display.
  // The window that places (or any Escape) broadcasts `picker-placing-end` to
  // clear the rest.
  const cleanupPickerStart = onTransient('picker-place-start', (msg) => {
    if (!editMode()) setEditMode(true);
    setPlacing({
      type: msg.widgetType,
      size: pxSizeToFrac(msg.defaultSize),
      defaultConfig: msg.defaultConfig,
    });
    // The picker source (config window) minimizes itself on pick, so Esc has
    // no focused window unless a widget window claims focus. All widget
    // windows do this in parallel; whichever wins owns the keydown stack and
    // App.tsx's placing-Esc handler can cancel the flow.
    getCurrentWindow().setFocus().catch(() => {});
  });
  const cleanupPickerEnd = onTransient('picker-placing-end', () => {
    if (placing() && !placing()!.fromClipboard) setPlacing(null);
  });
  // The config window's edit-mode slider reads a localStorage snapshot that can
  // be stale after a restart. When it mounts it broadcasts `edit-mode-query`;
  // reply with this window's live signal so the slider shows the true state.
  const cleanupEditModeQuery = onTransient('edit-mode-query', () => {
    postTransient({ type: 'edit-mode-state', value: editMode() });
  });
  onCleanup(() => {
    cleanupPlacingStart(); cleanupPlacingEnd();
    cleanupPickerStart(); cleanupPickerEnd();
    cleanupEditModeQuery();
  });

  // Open the unified config window (or, if it's already open, focus it and
  // tell it to navigate to `target`). See Rust `open_config_window`.
  function openConfigWindow(target: ConfigTarget) {
    invoke('open_config_window', {
      parentLabel: winLabel,
      target: encodeTarget(target),
    }).catch(e => logger.error('tauri', `open_config_window failed: ${e}`));
  }
  function handleConfigure(widget: { id: string }) {
    openConfigWindow({ kind: 'widget', widgetId: widget.id });
  }
  // Open the config window on its "Add Widget" page.
  function openAddWidgetPage() {
    openConfigWindow({ kind: 'add' });
  }



  function openViewSettings(viewId: string) {
    switchView(viewId);
    openConfigWindow({ kind: 'view', viewId });
  }

  // Edit-bar / context-menu "Add view". The free tier caps at 2 views, where
  // `addView` returns null, so check the limit first and send the user to the
  // license page rather than have the button do nothing. On success we switch to
  // the new (empty) view and open its settings so it can be named right away.
  function handleAddNewView() {
    if (atFreeViewLimit()) {
      openConfigWindow({ kind: 'app', page: 'license' });
      return;
    }
    const id = addView(nextViewName());
    if (!id) return;
    openViewSettings(id);
  }

  const [dragSrcIdx, setDragSrcIdx] = createSignal<number | null>(null);
  const [dragOverIdx, setDragOverIdx] = createSignal<number | null>(null);

  function pickBarAnchor(x: number, y: number): ViewBarPosition {
    const w = window.innerWidth, h = window.innerHeight;
    const xz = x / w, yz = y / h;
    const horiz: 'left' | 'center' | 'right' =
      xz < 0.33 ? 'left' : xz > 0.67 ? 'right' : 'center';
    if (yz >= 0.33 && yz <= 0.67) {
      if (horiz === 'left') return 'left-center';
      if (horiz === 'right') return 'right-center';
      return yz < 0.5 ? 'top-center' : 'bottom-center';
    }
    const vert = yz < 0.5 ? 'top' : 'bottom';
    return `${vert}-${horiz}` as ViewBarPosition;
  }

  function startBarDrag(e: MouseEvent) {
    if (!editMode()) return;
    if (e.button !== 0) return;
    e.stopPropagation();
    beginPointerDrag(e, {
      threshold: 4,
      preventDefault: false,
      onStart: () => setBarDragging(true),
      onMove: (_d, ev) => setBarSnapTarget(pickBarAnchor(ev.clientX, ev.clientY)),
      onEnd: (ev, moved) => {
        if (moved) setViewBarPosition(pickBarAnchor(ev.clientX, ev.clientY));
        setBarDragging(false);
        setBarSnapTarget(null);
      },
    });
  }

  // `fromIdx` is an index into the RENDERED bar (visibleBarViews()), which can be
  // a per-display filtered subset of viewStore.list. All reorder math therefore
  // resolves by view id against the full list (see viewReorderIndices); indexing
  // viewStore.list with a bar index would grab/move the wrong view.
  function startViewDrag(e: MouseEvent, fromIdx: number) {
    if (!editMode()) return;
    if (e.button !== 0) return;
    const view = visibleBarViews()[fromIdx];
    if (!view || isDemoView(view.id)) return;

    let currentBarIdx = fromIdx;
    const viewId = view.id;

    const barEl = (e.currentTarget as HTMLElement).closest('.views-bar') as HTMLElement | null;

    beginPointerDrag(e, {
      threshold: 4,
      preventDefault: false,
      onStart: () => setDragSrcIdx(currentBarIdx),
      onMove: (_d, ev) => {
        if (!barEl) return;
        const chips = Array.from(barEl.querySelectorAll<HTMLElement>('.view-chip'));
        if (!chips.length) return;
        const vertical = isVerticalBar();
        // Pick chip whose center is closest to the pointer along the bar's axis.
        // This keeps reordering responsive when the pointer leaves the bar
        // (e.g. past the last chip or above/below the bar).
        let bestIdx = -1;
        let bestDist = Infinity;
        for (let i = 0; i < chips.length; i++) {
          const r = chips[i].getBoundingClientRect();
          const center = vertical ? r.top + r.height / 2 : r.left + r.width / 2;
          const p = vertical ? ev.clientY : ev.clientX;
          const d = Math.abs(p - center);
          if (d < bestDist) { bestDist = d; bestIdx = i; }
        }
        if (bestIdx < 0 || bestIdx === currentBarIdx) return;
        const target = visibleBarViews()[bestIdx];
        if (!target || isDemoView(target.id)) return;
        const move = viewReorderIndices(viewStore.list, viewId, target.id);
        if (!move) return;
        moveView(move.from, move.to);
        currentBarIdx = visibleBarViews().findIndex(s => s.id === viewId);
        setDragSrcIdx(currentBarIdx);
      },
      onEnd: () => { setDragSrcIdx(null); setDragOverIdx(null); },
    });
  }

  function handleViewChipCtx(e: MouseEvent, view: { id: string; name: string; icon?: string }, _idx: number) {
    e.preventDefault();
    e.stopPropagation();
    const demo = isDemoView(view.id);
    // No title header, and without it a demo / non-edit chip has no actions,
    // so show no menu at all rather than an empty panel.
    if (demo || !editMode()) return;
    showCtxMenu(e, [
      { icon: Copy, label: t('ctxMenu.duplicate'), action: () => { const id = duplicateView(view.id); if (id) switchView(id); } },
      { icon: Settings, label: t('app.viewSettingsBtn'), action: () => openViewSettings(view.id) },
    ]);
  }

  onMount(() => {
    initEscapeStack();
    // Tell the native WH_KEYBOARD_LL hook to stop consuming Escape while a DOM
    // escape handler is active (edit mode / placing / open context menu), so the
    // keystroke reaches the webview and the escape stack can run. Cleared when
    // the stack empties, restoring the hook's folder-clear-selection intercept.
    onEscapeStackActive((active) => {
      invoke('set_escape_wanted', { label: winLabel, wanted: active })
        .catch(e => logger.error('tauri', `set_escape_wanted failed: ${e}`));
    });
    initHotkeys();
    initFocusStore();
    const onWindowCtx = (e: MouseEvent) => {
      e.preventDefault();
      if (!editMode()) return;
      showCtxMenu(e, [
        { icon: Plus,    label: t('app.addWidget'),       action: () => openAddWidgetPage() },
        { icon: Layers,  label: t('app.viewSettings'),    action: () => openViewSettings(viewStore.activeId) },
        { icon: Monitor, label: t('app.displaySettings'), action: () => openConfigWindow({ kind: 'display', monitorId: getCurrentMonitorId() ?? '' }) },
        { sep: true },
        { icon: X, label: t('app.leaveEditMode'), action: () => setEditMode(false) },
      ]);
    };
    window.addEventListener('contextmenu', onWindowCtx);
    onCleanup(() => window.removeEventListener('contextmenu', onWindowCtx));

    // The first-run default Folder widget resolves its own Desktop path at
    // render now (FolderView), so there's no App-level seed timer here (1.26).

    // Convert legacy RSS `feedUrl` widgets to managed feed connections. Runs on
    // mount (no wall-clock delay); idempotent + self-retrying on a later launch
    // if the backend isn't ready yet, so it doesn't need to be sequenced.
    void migrateRssFeeds();

    // Widget windows are WS_EX_NOACTIVATE, so clicks don't promote us to
    // foreground, meaning Windows won't auto-dismiss an open Start Menu
    // the way it does for normal apps. On every mousedown, ask Rust to
    // check foreground and nudge Progman if Start/Search is foreground.
    // Also mirror the desktop-icon behavior: a click into any widget body
    // clears the Windows desktop icon selection (the foreground hook does
    // the reverse: desktop clicks clear themia folder selections).
    const onGlobalMouseDown = (e: MouseEvent) => {
      invoke('dismiss_start_menu_if_open').catch(() => {});
      // Same root cause as Start: clicking a WS_EX_NOACTIVATE widget doesn't
      // change foreground, so the desktop/Explorer right-click menu won't
      // auto-dismiss. Nudge it closed.
      invoke('dismiss_context_menu_if_open').catch(() => {});
      const target = e.target as HTMLElement | null;
      if (target && target.closest('.widget')) {
        invoke('clear_desktop_selection').catch(() => {});
      }
    };
    document.addEventListener('mousedown', onGlobalMouseDown, { capture: true });
    onCleanup(() => document.removeEventListener('mousedown', onGlobalMouseDown, { capture: true }));

    // App-level shortcuts. The hotkeys module runs both a window keydown
    // listener and a Tauri `app-key` listener (routed via Rust WH_KEYBOARD_LL
    // hook, which bypasses WS_EX_NOACTIVATE that blocks JS keydown on
    // passive windows).
    registerHotkey('ctrl+e', () => setEditMode(m => !m));
    registerHotkey('ctrl+w', () => openAddWidgetPage());
    registerHotkey('ctrl+v', () => {
      if (editMode() && hasClipboard()) startClipboardPlacing('copy');
    });
    registerHotkey('ctrl+z', async () => {
      const r = await undoLast();
      // Empty label → another window claimed this undo; don't flash a toast.
      if (!r.label) return;
      showToast(r.label, r.ok ? 'info' : 'error', r.ok ? 2500 : 6000);
    });
    for (let n = 1; n <= 9; n++) {
      registerHotkey(`ctrl+${n}`, () => {
        const view = viewStore.list[n - 1];
        if (view) switchView(view.id);
      });
    }
  });


  return (
    <Show when={!getMonitorDisabled()}>
    <>
      {/* Only the active view's widgets are mounted. Switching views unmounts
          the previous tree (effects stop) and mounts the new one (effects
          re-run). Per-view layout survives in a module-level Map in layout.ts,
          so unmount loses live component state only, not positions/config. */}
      <Show when={!gameModeSuspended()}>
        <Show when={activeViewKeyed()} keyed>
          {(view) => (
            <ViewRoot viewId={view.id} active={true}>
              <WidgetGrid
                editMode={editMode()}
                onConfigure={handleConfigure}
                onEnterEditMode={() => setEditMode(true)}
                placing={placing()}
                onPlace={handlePlace}
                onStartClipboardPlacing={startClipboardPlacing}
                onAddToContainer={(containerId) => openConfigWindow({ kind: 'add', into: containerId })}
              />
            </ViewRoot>
          )}
        </Show>
      </Show>
      <ContextMenu />
      <Toast />

      {/* Hyperspace boot overlay ([S-16]): full-screen warping star streaks
          while the widgets warp in; unmounts itself on its fade-out
          animationend. (hacker [S-23] is a per-widget tile, not an overlay.) */}
      <Show when={bootOverlayActive()}>
        <HyperspaceOverlay />
      </Show>

      {/* Bottom-right status corner. Lives outside the grid's <Show> so its
          badges stay visible when the grid is unmounted; the badges stack
          upwards from the corner so they never overlap each other. */}
      <div class="corner-badges">
        {/* [S-27] First run only: the analytics consent question. Never a
            modal — it sits with the other corner cards and can simply be
            ignored, which leaves the decision unmade and sends nothing. */}
        <Show when={consentPrompt()}>
          <AnalyticsConsent onAnswered={() => setConsentPrompt(false)} />
        </Show>

        {/* Shown while widgets are suspended (fullscreen / game / presentation). */}
        <Show when={gameModeSuspended()}>
          <div class="game-mode-badge" title={tm('app.stalePillTitle')()} data-themia-interactive="true">
            <Pause size={12} strokeWidth={1.5} />
            <span>{tm('app.stalePill')()}</span>
          </div>
        </Show>
      </div>

      {/* ── Edit bars ── */}
      <Show when={editMode()}>
        <div class="edit-bar-row">
        <div class="edit-bar-right">
          <button class="design-btn" onClick={openAddWidgetPage}><Plus size={13} strokeWidth={1.5} /> {tm('app.addWidget')()}</button>
          <button
            class="design-btn"
            onClick={handleAddNewView}
            title={atFreeViewLimit() ? t('app.addViewLimit') : undefined}
          ><Layers size={13} strokeWidth={1.5} /> {tm('app.addView')()}</button>
          <div class="grid-toggle" title={t('settings.style.gridSnap.desc')}>
            <Grid3x3 size={13} strokeWidth={1.5} />
            <span>{t('app.gridSnap')}</span>
            <ToggleSwitch
              checked={globalDesign.gridSnap}
              onLabel=""
              offLabel=""
              aria-label={t('settings.style.gridSnap.label')}
              onChange={(next) => setGlobalDesign('gridSnap', next)}
            />
          </div>
          <button class="design-btn" onClick={() => openConfigWindow({ kind: 'app', page: 'general' })}><Settings size={13} strokeWidth={1.5} /> {tm('app.pillSettings')()}</button>
          <div class="divider" />
          <button class="design-btn primary" onClick={() => setEditMode(false)}>
            <X size={13} strokeWidth={1.5} /> {tm('app.leaveEditMode')()}
          </button>
        </div>
        </div>
      </Show>

      {/* ── View switcher, hidden when only 1 view is shown by default, or
             when the user hid the bar. An explicit single-view whitelist still
             shows; always shown in edit mode so it stays reachable. ── */}
      <Show when={editMode() || ((visibleBarViews().length > 1 || hasExplicitWhitelist()) && !getViewBarHidden())}>
      <div
        class={`views-bar${editMode() ? '' : ' compact'}${isVerticalBar() ? ' vertical' : ''} size-${getViewBarSize()}${getViewBarIconOnly() ? ' icon-only' : ''}${barDragging() ? ' bar-dragging' : ''}${editMode() && getViewBarHidden() ? ' hidden-preview' : ''}`}
        style={viewBarStyle()}
        data-themia-interactive="true"
      >
        <Show when={editMode()}>
          <button
            class="view-bar-handle"
            onMouseDown={startBarDrag}
            title={tm('app.dragBar')()}
          ><GripVertical size={14} strokeWidth={1.5} /></button>
        </Show>
        <For each={visibleBarViews()}>
          {(view, idx) => (
            <div
              class={`view-chip${view.id === viewStore.activeId ? ' active' : ''}${defaultNewDisplayView() === view.id ? ' is-default' : ''}${dragOverIdx() === idx() && dragSrcIdx() !== null && dragSrcIdx() !== idx() ? ' drag-over' : ''}${dragSrcIdx() === idx() ? ' dragging' : ''}`}
              onContextMenu={(e) => handleViewChipCtx(e, view, idx())}
              onMouseDown={(e) => {
                if (editMode() && !isDemoView(view.id)) startViewDrag(e, idx());
              }}
            >
              <button
                class={`view-name-btn${defaultNewDisplayView() === view.id ? ' is-default' : ''}`}
                onClick={() => switchView(view.id)}
                title={`${defaultNewDisplayView() === view.id ? t('app.isDefaultLabel', { name: view.name }) : view.name}\n${editMode() && !isDemoView(view.id) ? t('app.chipTitleEditable') : t('app.chipTitle')}`}
              >
                <span class="view-icon">
                  <Show
                    when={view.icon && VIEW_ICON_MAP[view.icon]}
                    fallback={
                      <span
                        class="view-icon-letters"
                        style={{ 'font-size': `${viewIconSize()}px` }}
                      >{viewInitials(view.name)}</span>
                    }
                  >
                    {VIEW_ICON_MAP[view.icon!]?.(viewIconSize())}
                  </Show>
                </span>
                <span class="view-name-text">{view.name}</span>
              </button>
            </div>
          )}
        </For>
      </div>
      </Show>

      {/* The Add Widget picker, Settings, View Settings and Widget Config
          all live in the unified config window; see Rust
          `open_config_window`. */}

    </>
    </Show>
  );
}
