// ---------------------------------------------------------------------------
// Cross-window synchronization hub
//
// Two mechanisms:
// 1. Storage events, for persisted state (layout, design, views, clipboard).
//    Writing to localStorage automatically notifies all OTHER windows.
// 2. BroadcastChannel, for transient fire-and-forget messages (cross-drag,
//    clipboard-placing UI coordination) that have no localStorage backing.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 1. Storage-event listeners
// ---------------------------------------------------------------------------

type StorageKeyHandler = (newValue: string | null) => void;
type StorageMatchHandler = (key: string, newValue: string | null) => void;

const keyHandlers = new Map<string, StorageKeyHandler[]>();
const matchHandlers: { test: (key: string) => boolean; fn: StorageMatchHandler }[] = [];

/** Run every registered key + match handler for a (key, value) change. Invoked
 *  by BOTH the native `storage` event AND the reliable `storage-write` broadcast
 *  mirror (1.24): the native event doesn't fire consistently between WebView2
 *  overlay windows, so the broadcast guarantees delivery. Handlers reload from
 *  localStorage, so a double-fire (both paths arriving) is idempotent. */
function dispatchStorageChange(key: string, value: string | null): void {
  const fns = keyHandlers.get(key);
  if (fns) for (const fn of fns) fn(value);
  for (const h of matchHandlers) {
    if (h.test(key)) h.fn(key, value);
  }
}

window.addEventListener('storage', (e) => {
  if (!e.key) return;
  dispatchStorageChange(e.key, e.newValue);
});

/** Listen for changes to a specific localStorage key from other windows. */
export function onStorageKey(key: string, fn: StorageKeyHandler): () => void {
  if (!keyHandlers.has(key)) keyHandlers.set(key, []);
  keyHandlers.get(key)!.push(fn);
  return () => {
    const arr = keyHandlers.get(key)!;
    const idx = arr.indexOf(fn);
    if (idx !== -1) arr.splice(idx, 1);
  };
}

/** Listen for changes to localStorage keys matching a predicate. */
export function onStorageMatch(test: (key: string) => boolean, fn: StorageMatchHandler): () => void {
  const entry = { test, fn };
  matchHandlers.push(entry);
  return () => {
    const idx = matchHandlers.indexOf(entry);
    if (idx !== -1) matchHandlers.splice(idx, 1);
  };
}

// ---------------------------------------------------------------------------
// 2. BroadcastChannel for transient messages
// ---------------------------------------------------------------------------

const transientChannel = new BroadcastChannel('widget-transient');
const SENDER_ID = `w-${Math.random().toString(36).slice(2)}-${Date.now()}`;

/**
 * Discriminated union of every cross-window transient message. Replaces the
 * old `{ type: string; [k: string]: any }` bag so senders/handlers are
 * type-checked on the `type` discriminant and its payload. The genuinely
 * dynamic config payloads (a default-config blob, a layout assignment) stay
 * `Record<string, unknown>`; they're cross-module blobs sync.ts shouldn't
 * depend on the concrete types of.
 */
// Minimal serialized widget carried by a cross-window drop. Only the fields the
// receiving window reads to reconstruct the widget (type + config + label); the
// geometry travels separately in PIXELS on the message itself.
export interface CrossDropWidget {
  type: string;
  config?: Record<string, unknown>;
  label?: string;
}

export type TransientMsg =
  | { type: 'edit-mode-set'; value: boolean }
  | { type: 'edit-mode-query' }
  | { type: 'edit-mode-state'; value: boolean }
  // Reliable cross-window mirror of a localStorage write (1.24). The native
  // `storage` event is unreliable between WebView2 overlay windows, so
  // `writeShared`/`removeShared` broadcast this and the receiver re-dispatches to
  // the same `onStorageKey`/`onStorageMatch` handlers. `value: null` = removal.
  | { type: 'storage-write'; key: string; value: string | null }
  // Placement/transport sizes travel in PIXELS so each window can convert to a
  // fraction of its own screen and preserve physical size ([S-12]).
  | { type: 'clipboard-placing-start'; mode: 'copy' | 'cut'; wPx: number; hPx: number }
  | { type: 'clipboard-placing-end' }
  | { type: 'picker-place-start'; widgetType: string; defaultSize: { width: number; height: number }; defaultConfig: Record<string, unknown> }
  | { type: 'picker-placing-end' }
  | { type: 'accounts-changed' }
  // The license tier changed (activated / deactivated). Only the config window
  // runs activation, and every window has its own JS heap and its own license
  // store read once at startup -- so without this the desktop windows keep the
  // tier they booted with: Settings says Pro while the widget/view limits still
  // refuse ([S-46]).
  | { type: 'license-changed' }
  // The local notes store changed (created / renamed / deleted / body written).
  // Every window holding a Notes widget or its config panel reloads. `id` names
  // the note whose BODY changed, so a widget showing a different note can skip
  // the re-read; it is absent for list-shape changes.
  | { type: 'notes-changed'; id?: string }
  // Settings preview button: replay an intro (incl. the hyperspace overlay,
  // [S-16]) in every widget window. `mode` selects which section's style +
  // duration to play ([S-24]/[S-16]); absent = 'startup' (back-compat).
  | { type: 'startup-anim-replay'; mode?: 'startup' | 'unlock' | 'transition' }
  | { type: 'layout-assign'; leafId: string; assignment: Record<string, unknown> }
  | { type: 'widget-cross-drag-move'; screenX: number; screenY: number; wPx: number; hPx: number; offsetX: number; offsetY: number; dragId: string }
  | { type: 'widget-cross-drag-end'; dragId: string }
  | { type: 'widget-cross-drop'; screenX: number; screenY: number; offsetX: number; offsetY: number; wPx: number; hPx: number; widget: CrossDropWidget; sourceId: string; sourceKey: string };

export type TransientType = TransientMsg['type'];
type MsgOf<K extends TransientType> = Extract<TransientMsg, { type: K }> & { sender?: string };

type AnyHandler = (msg: unknown) => void;

// Minimal channel surface the bus needs; a real BroadcastChannel satisfies it,
// tests inject a fake to simulate two windows sharing one channel.
export interface TransientChannel {
  postMessage(msg: unknown): void;
  addEventListener(type: 'message', listener: (e: MessageEvent) => void): void;
}

export interface TransientBus {
  post(msg: TransientMsg): void;
  on<K extends TransientType>(type: K, fn: (msg: MsgOf<K>) => void): () => void;
}

/**
 * The transient-message bus, factored out of module scope so the routing (own-
 * sender suppression + type dispatch) is unit-testable. `selfId` stamps every
 * outgoing message; an incoming message stamped with the same id is ignored, so
 * a window never handles its own broadcast.
 */
export function createTransientBus(channel: TransientChannel, selfId: string): TransientBus {
  const handlers = new Map<string, AnyHandler[]>();
  channel.addEventListener('message', (e: MessageEvent) => {
    if (e.data?.sender === selfId) return;
    const type = e.data?.type;
    if (!type) return;
    const fns = handlers.get(type);
    if (fns) for (const fn of fns) fn(e.data);
  });
  return {
    post(msg) {
      channel.postMessage({ ...msg, sender: selfId });
    },
    on(type, fn) {
      if (!handlers.has(type)) handlers.set(type, []);
      handlers.get(type)!.push(fn as AnyHandler);
      return () => {
        const arr = handlers.get(type)!;
        const idx = arr.indexOf(fn as AnyHandler);
        if (idx !== -1) arr.splice(idx, 1);
      };
    },
  };
}

const transientBus = createTransientBus(transientChannel, SENDER_ID);

/** Send a fire-and-forget message to all other windows. */
export function postTransient(msg: TransientMsg): void {
  transientBus.post(msg);
}

/** Listen for a specific transient message type from other windows. The
 *  handler's `msg` is typed to that message's payload. */
export function onTransient<K extends TransientType>(type: K, fn: (msg: MsgOf<K>) => void): () => void {
  return transientBus.on(type, fn);
}

// ---------------------------------------------------------------------------
// 3. Reliable shared-write propagation (1.24, option B)
//
// The native `storage` event is the only thing notifying other windows of a
// persisted change today, and it doesn't fire reliably across separate WebView2
// windows. Route cross-window-shared writes through `writeShared`, which mirrors
// the change over the reliable BroadcastChannel; the receiver re-dispatches it to
// the same `onStorageKey` handlers. The native `storage` event stays as a
// redundant (harmless, idempotent) path, and handlers reload from localStorage.
// ---------------------------------------------------------------------------

transientBus.on('storage-write', (m) => dispatchStorageChange(m.key, m.value));

/** Write a cross-window-shared localStorage key AND reliably notify other
 *  windows. Use instead of `localStorage.setItem` for any state that drives
 *  `onStorageKey`/`onStorageMatch` handlers elsewhere (layout, design, views,
 *  per-monitor maps). The writing window updates its own state directly, so it
 *  doesn't receive its own broadcast (own-sender suppression). */
export function writeShared(key: string, value: string): void {
  localStorage.setItem(key, value);
  postTransient({ type: 'storage-write', key, value });
}

/** Remove a cross-window-shared key + notify other windows (mirror of writeShared). */
export function removeShared(key: string): void {
  localStorage.removeItem(key);
  postTransient({ type: 'storage-write', key, value: null });
}

/** A `localStorage`-backed store whose writes propagate cross-window via
 *  `writeShared` (1.24). Pass this to `savePersisted`/`savePersistedBare` instead
 *  of `localStorage` so a store's changes reliably reach other windows. Backup
 *  writes (`__bak__*`, which no window listens for) stay local to avoid redundant
 *  broadcasts. Shape matches `persistedStore`'s `BackedStorage` (getItem+setItem). */
export const sharedStorage = {
  getItem: (key: string): string | null => localStorage.getItem(key),
  setItem: (key: string, value: string): void => {
    if (key.startsWith('__bak__')) localStorage.setItem(key, value);
    else writeShared(key, value);
  },
};
