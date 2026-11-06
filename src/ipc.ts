// ---------------------------------------------------------------------------
// Typed IPC layer (1.22)
//
// A hand-maintained, drift-guarded type map over Tauri's `invoke`. One entry per
// `#[tauri::command]` in src-tauri: the args object the frontend passes + the
// success type the command resolves to. (A Rust `Result<T, E>` resolves to `T`
// and REJECTS on `Err`, so `result` is the Ok type; `()` → `void`, `Option<T>` →
// `T | null`.) This is the TS-side source of truth for IPC shapes; `ipc.test.ts`
// guards the command-NAME set against the Rust `generate_handler!` list so a
// renamed/removed command fails a test instead of blowing up at runtime with
// "command not found".
//
// Migration: switch a call site from `@tauri-apps/api/core`'s `invoke` to this
// one. Unknown command names + wrong arg/return types then become compile errors.
// Commands are added to `Commands` + `TYPED_COMMANDS` together (a compile-time
// assertion keeps the two in sync); call sites migrate once their command is typed.
// ---------------------------------------------------------------------------
import { invoke as rawInvoke } from '@tauri-apps/api/core';
// Type-only, and it must stay that way: this module is a leaf that everything
// (including i18n) imports, so a VALUE import from a widget would close an
// import cycle. `import type` is erased at build, and it buys the power widget's
// action union a single definition instead of a copy here that could drift from
// the Rust enum independently.
import type { PowerActionName } from '../widgets/system/power/actions.ts';

// ── Custom types referenced by Commands (deduplicated by name) ──

// Returned by tray_env: what the shell expects a notification icon to look
// like right now (Tray.SPEC [S-03]).
export interface TrayEnv {
  /** Icon edge in PHYSICAL px (`SM_CXSMICON`), so we rasterize 1:1. */
  iconSize: number;
  /** Taskbar painted light → icons must be drawn dark to stay visible. */
  lightTheme: boolean;
}

// One tray icon, as sent to tray_sync. `icon` is `iconSize² * 4` RGBA8 bytes.
export interface TraySpec {
  id: string;
  tooltip: string;
  icon: number[];
  iconSize: number;
  /** Flyout size in LOGICAL px (Tray.SPEC [S-07]). */
  width: number;
  height: number;
}

// Tray context-menu strings, translated frontend-side (Tray.SPEC [S-08]).
export interface TrayMenuLabels {
  open: string;
  settings: string;
  appSettings: string;
  quit: string;
}

// Used by resolve_shortcut
interface ShortcutTarget {
  target_path: string;
  is_directory: boolean;
}

// Returned by recycle_bin_status: how much sits in the Recycle Bin across all
// drives. Zeroed on non-Windows.
export interface RecycleBinStatus {
  items: number;
  bytes: number;
}

// Returned by fs_delete. Rust always serializes every field (the bin-id fields
// are empty strings / 0 when the post-delete enumeration couldn't match), so on
// the RESULT direction they are all present.
interface TrashedFile {
  original_path: string;
  name: string;
  deleted_at_ms: number;
  id: string;
  original_parent: string;
  time_deleted: number;
}

// Accepted by fs_restore_from_trash. The Rust struct marks id/original_parent/
// time_deleted `#[serde(default)]`, so on the ARGS (deserialize) direction these
// are optional on the wire; tokens persisted in localStorage before those
// fields existed legitimately omit them, and the backend falls back to the
// legacy enumerate-and-match path. (Matches widget_folder::TrashedFile.)
interface TrashedFileToken {
  original_path: string;
  name: string;
  deleted_at_ms: number;
  id?: string;
  original_parent?: string;
  time_deleted?: number;
}

// Used by clipboard_get_files
interface ClipboardFiles {
  paths: string[];
  cut: boolean;
}

// Used by get_cursor_state
interface CursorState {
  x: number;
  y: number;
  left_down: boolean;
  ctrl_down: boolean;
}

// Used by get_cloud_sync_folders
interface CloudFolder {
  provider: string;
  path: string;
}

// Used by get_shell_new_entries
interface ShellNewEntry {
  extension: string;
  label: string;
  icon_base64: string | null;
  template_data: number[] | null;
}

// Used by get_shell_menu_entries
interface ShellMenuEntry {
  id: string;
  label: string;
  icon_path: string | null;
  icon_base64: string | null;
  command: string;
  extended: boolean;
}

// Used by folder_read_dir
interface FolderDirEntry {
  name: string;
  isDirectory: boolean;
  isFile: boolean;
  isSymlink: boolean;
  isHidden: boolean;
}

// Used by get_git_status
interface GitStatus {
  is_git: boolean;
  branch: string;
  files: Record<string, string>;
  staged: number;
  modified: number;
  untracked: number;
  deleted: number;
  conflicted: number;
}

// Used by get_git_repo_info_batch
interface GitRepoInfo {
  branch: string;
  dirty: boolean;
}

// Used by email_add_caldav_account, email_add_carddav_account, email_add_github_account,
//   email_add_imap_account, email_add_rss_feed, email_complete_google_auth,
//   email_list_accounts, email_poll_auth
interface AccountInfo {
  id: string;
  email: string;
  display_name: string;
  nickname: string | null;
  provider: string;
}

// Used by email_probe_endpoint. Mirrors the Rust `ProbeTarget` / `ProbeOutcome`
// enums (widgets/office/email/src/lib.rs) — closed sets on both sides, so a new
// probe kind can't be invented at a call site. The frontend counterpart of the
// target lives in connect/presets.ts as `PresetProbe`.
export type ProbeTarget =
  | { kind: 'imap'; host: string; port: number }
  | { kind: 'dav'; url: string };

export type ProbeOutcome = 'reachable' | 'unreachable' | 'not_dav' | 'server_error';

export interface ProbeResult {
  outcome: ProbeOutcome;
  detail: string;
  elapsed_ms: number;
}

// Used by email_get_claude_usage. One rate-limit window + the rendered subset of
// the Claude OAuth usage endpoint (unknown buckets dropped by the backend).
interface UsageWindow {
  utilization: number;
  resets_at: string | null;
}
interface ClaudeUsage {
  five_hour: UsageWindow;
  seven_day: UsageWindow;
  seven_day_opus: UsageWindow | null;
  seven_day_sonnet: UsageWindow | null;
}

// Used by email_start_device_auth
interface DeviceAuthStart {
  user_code: string;
  verification_url: string;
  device_code: string;
  interval: number;
}

// Used by email_start_google_auth
interface GoogleAuthStart {
  auth_url: string;
  state: string;
}

// Used by email_start_claude_auth
interface ClaudeAuthStart {
  auth_url: string;
  state: string;
}

// Used by email_get_messages
interface EmailMessage {
  id: string;
  subject: string;
  from_name: string;
  from_email: string;
  received_at: string;
  is_read: boolean;
  preview: string;
  web_link: string;
  account_id: string;
  account_email: string;
  account_nickname: string;
  account_provider: string;
}

// Used by email_get_folder_status
interface FolderStatus {
  unread: number;
  total: number;
}

// Used by get_system_stats
interface ProcCpu {
  name: string;
  pid: number;
  cpu_pct: number;
  /** Version-info FileDescription ("Google Chrome"); null when unresolvable ([cpu S-25]). */
  desc?: string | null;
}

// Used by get_system_stats
interface ProcMem {
  name: string;
  pid: number;
  mem_bytes: number;
}

// Used by get_system_stats
interface DiskStats {
  mount: string;
  used: number;
  total: number;
  read_per_sec: number;
  write_per_sec: number;
}

// Used by get_system_stats
interface NetIface {
  name: string;
  rx_per_sec: number;
  tx_per_sec: number;
  is_up: boolean;
}

// Used by get_system_stats
interface SystemStats {
  cpu_usage: number;
  cpu_cores: number[];
  cpu_freq_mhz: number;
  cpu_temp_c: number | null;
  cpu_fan_rpm: number | null;
  cpu_processes_total: number;
  cpu_threads_total: number;
  cpu_top: ProcCpu[];
  cpu_uptime_s: number;
  ram_used: number;
  ram_total: number;
  ram_available: number;
  swap_used: number;
  swap_total: number;
  ram_top: ProcMem[];
  disks: DiskStats[];
  net_rx_per_sec: number;
  net_tx_per_sec: number;
  net_interfaces: NetIface[];
  net_session_rx: number;
  net_session_tx: number;
  public_ip: string | null;
  disk_read_per_sec: number;
  disk_write_per_sec: number;
  disk_read_ops_per_sec: number;
  disk_write_ops_per_sec: number;
}

// Used by list_gpus
interface GpuInfo {
  index: number;
  name: string;
}

// Used by get_gpu_stats
interface GpuStats {
  name: string;
  utilization: number;
  vram_used_mb: number;
  vram_total_mb: number;
  shared_used_mb: number;
  shared_total_mb: number;
  temp_c: number | null;
  power_w: number | null;
  fan_pct: number | null;
  encoder_pct: number | null;
  decoder_pct: number | null;
  core_clock_mhz: number | null;
  mem_clock_mhz: number | null;
}

// Used by ping_hosts
interface PingHost {
  label: string;
  address: string;
}

// Used by ping_hosts
interface PingResult {
  label: string;
  address: string;
  ok: boolean;
  latency_ms: number | null;
}

// Used by volume_get
interface VolumeState {
  level: number;
  muted: boolean;
}

// One row of mixer_get_sessions: an application (grouped) or a single audio
// session (ungrouped) on the default output device (`widget_mixer::MixerApp`).
interface MixerApp {
  /** Exe path, session instance id, or the `system-sounds` sentinel — whichever
   *  it is, it is what mixer_set_volume / mixer_set_mute take back. */
  key: string;
  /** Empty when Windows offers no usable name; the widget localizes that case. */
  name: string;
  path: string | null;
  level: number;
  muted: boolean;
  active: boolean;
  is_system: boolean;
  sessions: number;
}

// Used by music_get_now_playing
interface NowPlayingInfo {
  title: string;
  artist: string;
  album: string;
  thumbnail_base64: string | null;
  position_ms: number;
  duration_ms: number;
  is_playing: boolean;
  has_next: boolean;
  has_previous: boolean;
}

// Used by calendar_get_events
interface CalendarEvent {
  id: string;
  subject: string;
  start_dt: string;
  end_dt: string;
  is_all_day: boolean;
  location: string | null;
  organizer: string | null;
  web_link: string;
}

// Used by calendar_list_calendars [S-48]
interface CalendarInfo {
  id: string;
  name: string;
  is_default: boolean;
  color: string | null;
}

// Used by calendar_get_birthdays
interface ContactBirthday {
  display_name: string;
  month: number;
  day: number;
  year: number | null;
}

// Used by todo_get_lists
interface TodoTaskList {
  id: string;
  display_name: string;
  is_default: boolean;
}

// Used by todo_create_task, todo_get_tasks
interface TodoTask {
  id: string;
  title: string;
  status: string;
  importance: string;
  due_date: string | null;
  completed_at: string | null;
  list_id: string;
}

// Used by contacts_list
interface Contact {
  id: string;
  display_name: string;
  email: string | null;
  phone: string | null;
  job_title: string | null;
  company: string | null;
}

// Used by onedrive_list_folder, onedrive_recent_files
interface OneDriveItem {
  id: string;
  name: string;
  size: number;
  last_modified: string;
  web_url: string;
  is_folder: boolean;
  mime_type: string | null;
}

// Used by notes_list, notes_create
export interface NoteMeta {
  id: string;
  title: string;
  /** Unix seconds of the last content write. */
  updated_at: number;
}

// Used by get_battery_stats
interface BatteryStats {
  percent: number;
  is_charging: boolean;
  is_on_ac: boolean;
  seconds_remaining: number | null;
  has_battery: boolean;
  design_capacity_mwh: number | null;
  full_charge_capacity_mwh: number | null;
  health_pct: number | null;
  cycle_count: number | null;
  voltage_mv: number | null;
  temp_c: number | null;
}

// Used by screentime_get_day
interface ScreentimeAppStat {
  exe: string;
  name: string;
  secs: number;
  // [S-38] full exe path (when known) for icon extraction via get_file_icon.
  path?: string | null;
}

// Used by screentime_get_day
interface ScreentimeDayStats {
  date: string;
  total_secs: number;
  apps: ScreentimeAppStat[];
  /** [S-55] foreground app switches recorded for the date. */
  switch_count: number;
}

// Used by get_bluetooth_devices
interface BluetoothDevice {
  name: string;
  id: string;
  is_connected: boolean;
  device_type: string;
  battery_pct: number | null;
  rssi: number | null;
}

// Used by list_start_menu_apps
interface AppEntry {
  name: string;
  path: string;
  category: string;
}

// Used by launch_app_layout
interface LayoutEntry {
  kind: string;
  target: string;
  args?: string | null;
  xPct: number;
  yPct: number;
  wPct: number;
  hPct: number;
  reuseIfRunning?: boolean;
  matchHint?: string | null;
}

// Returned by get_active_monitors (one entry per live monitor).
//
// [MI-01] `id` is the monitor's device INTERFACE path
// (`\\?\DISPLAY#DEL4098#5&1a2b3c&0&UID4353#{e6f07b5f-…}`): it carries the panel's
// hardware ids and survives reboots, so it is the ONLY valid persistence key.
// `deviceName` is the GDI ordinal `\\.\DISPLAYn`, which Windows re-derives every
// boot from the order the panels come up in — it names a DIFFERENT physical panel
// from one boot to the next (that was #70). It is a LABEL SOURCE ONLY and must
// never be written to storage as a key again.
//
// Geometry is the work area (taskbar excluded) in physical pixels; x/y are
// virtual-desktop coordinates and CAN be negative (a monitor left of the primary).
export interface MonitorDescriptor {
  id: string;
  /** [MI-08] False when `id` is the [MI-02] ordinal fallback - a placeholder
   *  for an identity Rust could not resolve on this pass (a panel still
   *  DisplayPort-link-training answers that way). ABSENT means "resolved", so
   *  descriptors from any other source (demo fixtures, e2e stubs) keep their
   *  old meaning and only a backend actively reporting degradation changes
   *  behaviour. */
  idStable?: boolean;
  deviceName: string;
  primary: boolean;
  x: number;
  y: number;
  width: number;
  height: number;
}

/**
 * Closed set of analytics event names, mirroring the Rust `AnalyticsEvent`
 * enum. A union rather than `string`: the backend rejects anything else at the
 * IPC boundary, so the frontend cannot invent an event. These strings ARE the
 * TelemetryDeck signal types, passed straight to `td.signal()`.
 * See docs/ANALYTICS.md.
 */
export type AnalyticsEvent =
  | 'TelemetryDeck.Acquisition.newInstallDetected'
  | 'TelemetryDeck.Session.started'
  | 'app_heartbeat'
  | 'app_opt_out';

/**
 * One fully-built signal. The frontend adds nothing to it: it posts `payload`
 * under `type` and, when `marker` is set, confirms delivery back so the backend
 * can persist it ([S-15]).
 */
export interface AnalyticsSignal {
  type: string;
  /**
   * TelemetryDeck's default parameters, built by the backend. The SDK knows
   * nothing about the app it is embedded in, so version, platform, OS version,
   * locale and retention counts come from here or are lost ([S-16]).
   */
  payload: Record<string, string>;
  marker: AnalyticsEvent | null;
}

/**
 * What the backend permits the frontend to send right now (analytics SPEC
 * [S-04]). An empty `signals` means send nothing — opted out, unconfigured,
 * already sent today, or another window claimed the work. The frontend can
 * only ever send what appears here, which is what keeps consent enforcement in
 * the backend even though TelemetryDeck's SDK runs in the webview.
 */
export interface AnalyticsPlan {
  appId: string;
  /** SHA-256 of the install UUID; the raw UUID never reaches the webview. */
  clientUser: string;
  target: string;
  testMode: boolean;
  signals: AnalyticsSignal[];
}

/**
 * The user's analytics decision ([S-03]). Three states, not a boolean:
 * `unset` means the first-run prompt has not been answered yet, which shares
 * exactly as much as `denied` and differs only in that we may still ask.
 */
export type AnalyticsConsent = 'unset' | 'granted' | 'denied';

/** TelemetryDeck's error categories, spelled as its Errors preset expects. */
export type AnalyticsErrorCategory = 'thrown-exception' | 'user-input' | 'app-state';

/**
 * Things the app reports as they happen. A closed, typed set: the backend
 * rejects an unknown event or a smuggled extra parameter at the boundary, so
 * arbitrary data can never be attached to a signal ([S-21]).
 */
export type AnalyticsTrackedEvent =
  | {
      event: 'error';
      /** Stable grouping slug, never an interpolated message. */
      id: string;
      category: AnalyticsErrorCategory;
      /**
       * Optional STATIC description. The backend drops anything shaped like
       * user data (paths, URLs, addresses) before sending ([S-22]).
       */
      message?: string;
    }
  | { event: 'navigation'; from: string; to: string }
  | { event: 'purchase'; productId: string; kind: 'subscription' | 'one-time-purchase' };

/**
 * The facts only a webview can see, handed to the backend so it can put them
 * on every signal. Every field is optional: a missing one drops its parameter
 * rather than inventing a value ([S-18]).
 */
export interface AnalyticsEnv {
  locale?: string;
  preferredLanguage?: string;
  region?: string;
  appLanguage?: string;
  timeZone?: string;
  colorScheme?: string;
  layoutDirection?: string;
  screenWidth?: number;
  screenHeight?: number;
  screenScale?: number;
}

// Used by license_activate, license_validate, license_status
interface LicenseStatus {
  tier: string;
  activated: boolean;
  key: string | null;
  instance_id: string | null;
  valid_until: string | null;
  error: string | null;
}

// Used by start_native_drag
interface StartDragRequest {
  data_type: string;
  file_paths: string[] | null;
  widget_json: string | null;
  effect_allowed: string;
}

// Used by start_native_drag
interface StartDragResult {
  effect: string;
}

// Used by diag_window_state
interface WindowDiagnostics {
  hwnd: number;
  is_visible: boolean;
  is_iconic: boolean;
  is_cloaked: boolean;
  cloak_reason: number;
  rect: [number, number, number, number];
  has_subclass: boolean;
  topmost_class_at_center: string;
  above_progman: boolean;
  widgets_pinned_topmost: boolean;
  ex_style: number;
}

// Used by diag_z_counters
interface ZOrderCounters {
  topmost_transitions: number;
  currently_topmost: boolean;
}

// Used by diag_compare_zorder
interface ZComparison {
  widget_hwnd: number;
  other_hwnd: number;
  other_found: boolean;
  other_above_widget: boolean;
}

// Used by diag_demote_widget
interface DemoteResult {
  succeeded: boolean;
  above_progman_immediately_after: boolean;
}

// Used by diag_list_widgets
interface WidgetSummary {
  label: string;
  hwnd: number;
  has_subclass: boolean;
  above_progman: boolean;
  is_topmost: boolean;
  is_visible: boolean;
  is_iconic: boolean;
  is_cloaked: boolean;
  rect: [number, number, number, number];
  owner_hwnd: number;
  owner_is_topmost: boolean;
  ex_style: number;
  style: number;
  class_name: string;
  parent_hwnd: number;
  process_id: number;
}

// Used by diag_force_topmost
interface ForceTopmostResult {
  succeeded_pass1: boolean;
  ex_style_after_pass1: number;
  succeeded_pass2: boolean;
  ex_style_after_pass2: number;
  last_error: number;
}

export interface Commands {
  // ── Files / shell ──
  open_url:          { args: { url: string };       result: void };
  quit_app:          { args: void;                  result: void };
  open_file:         { args: { path: string };      result: void };
  open_in_explorer:  { args: { path: string };      result: void };
  fs_exists:         { args: { path: string };      result: boolean };
  recycle_bin_status: { args: void;                 result: RecycleBinStatus };
  resolve_shortcut: { args: { path: string }; result: ShortcutTarget };
  get_file_icon: { args: { path: string; size: number }; result: string };
  get_file_icons_batch: { args: { paths: string[]; size: number }; result: Record<string, string> };
  fs_paste: { args: { paths: string[]; destDir: string; cut: boolean }; result: void };
  fs_delete: { args: { paths: string[] }; result: TrashedFile[] };
  fs_delete_permanent: { args: { paths: string[] }; result: void };
  fs_restore_from_trash: { args: { items: TrashedFileToken[] }; result: void };
  fs_rename: { args: { path: string; newName: string }; result: void };
  show_file_properties: { args: { path: string }; result: void };
  open_with: { args: { path: string }; result: void };
  fs_write_file: { args: { destDir: string; fileName: string; data: number[] }; result: void };
  get_folder_item_counts: { args: { paths: string[] }; result: Record<string, number> };
  get_image_thumbnail: { args: { path: string; size: number }; result: string };
  get_image_thumbnails_batch: { args: { paths: string[]; size: number }; result: Record<string, string> };
  fs_download_url: { args: { url: string; destDir: string }; result: string };
  get_clipboard_url: { args: void; result: string | null };
  get_cloud_sync_folders: { args: void; result: CloudFolder[] };
  get_shell_new_entries: { args: void; result: ShellNewEntry[] };
  fs_create_folder: { args: { destDir: string; name: string }; result: string };
  get_shell_menu_entries: { args: { isDirectory: boolean; isBackground: boolean }; result: ShellMenuEntry[] };
  execute_shell_command: { args: { verbId: string; targetPath: string; isDirectory: boolean; isBackground: boolean }; result: void };
  get_file_attributes: { args: { path: string }; result: number };
  get_file_attributes_batch: { args: { paths: string[] }; result: Record<string, number> };
  folder_read_dir: { args: { path: string }; result: FolderDirEntry[] };
  get_file_stats_batch: { args: { paths: string[] }; result: Record<string, [number, number]> };
  set_cloud_pin: { args: { paths: string[]; mode: string }; result: void };
  get_git_status: { args: { path: string }; result: GitStatus };
  get_git_repo_info_batch: { args: { paths: string[] }; result: Record<string, GitRepoInfo> };
  scan_images: { args: { path: string; recursive: boolean }; result: string[] };
  read_image_base64: { args: { path: string }; result: string };
  // ── System ──
  get_system_locale: { args: void;                  result: string };
  // Installed font family names for the Settings font picker (issue #64). Empty
  // on non-Windows or on any enumeration failure.
  list_system_fonts: { args: void;                   result: string[] };
  // [MI-01] Resolves to the window's monitor's STABLE id, never the ordinal.
  get_monitor_id:    { args: { label: string };     result: string | null };
  show_devtools:     { args: { label: string };     result: void };
  set_ssrf_bypass:   { args: { enabled: boolean };  result: void };
  // ── Drag & drop ──
  start_native_drag: { args: { request: StartDragRequest }; result: StartDragResult };
  save_virtual_files: { args: { destDir: string }; result: string[] };
  set_drag_over_zone: { args: { label: string; overZone: boolean }; result: void };
  // ── Window / config / system ──
  push_windows_to_desktop: { args: void; result: void };
  // [DI-01] Posts the shell's own toggle command; the change is Explorer's
  // persisted setting, so it outlives the app. Settings toggle only.
  set_desktop_icons: { args: { visible: boolean }; result: void };
  // [DI-04] Live state from the shell. `null` = unreadable (non-Windows, or the
  // desktop view could not be resolved); the caller keeps what it has.
  get_desktop_icons: { args: void; result: boolean | null };
  // [DI-06] One-shot upgrade repair: undo a pre-0.15 `SW_HIDE` and re-hide
  // through the shell. Called at most once ever, from the design-store migration.
  repair_legacy_desktop_icon_hide: { args: void; result: void };
  clear_desktop_selection: { args: void; result: void };
  // [LOG-02] Settings > Logs page open/close, gates INFO/DEBUG webview forwarding.
  set_log_stream: { args: { open: boolean }; result: void };
  get_cursor_state: { args: void; result: CursorState };
  activate_widget_keyboard: { args: { label: string }; result: void };
  deactivate_widget_keyboard: { args: void; result: void };
  set_text_input_focused: { args: { label: string; focused: boolean }; result: void };
  set_escape_wanted: { args: { label: string; wanted: boolean }; result: void };
  dismiss_start_menu_if_open: { args: void; result: boolean };
  dismiss_context_menu_if_open: { args: void; result: boolean };
  close_devtools: { args: { label: string }; result: void };
  set_logging_enabled: { args: { enabled: boolean }; result: void };
  // [LOG-08] Reveals %LOCALAPPDATA%\com.themia.desktop\logs, creating it first
  // so a user who just enabled logging never lands in Documents.
  open_log_folder: { args: void; result: void };
  open_config_window: { args: { parentLabel: string; target?: string | null }; result: void };
  open_connect_window: { args: { parentLabel: string; provider?: string | null }; result: void };
  open_layout_window: { args: { parentLabel: string; widgetId: string; viewId: string; itemId: string }; result: void };
  open_shortcut_add_window: { args: { parentLabel: string; widgetId: string; viewId: string }; result: void };
  open_layout_app_picker_window: { args: { parentLabel: string; leafId: string }; result: void };
  open_widget_window: { args: { parentLabel: string; widgetType: string; widgetId: string; viewId: string; title: string }; result: void };
  open_web_element_picker_window: { args: { parentLabel: string; targetUrl: string; widgetId: string }; result: void };
  web_element_render_html: { args: { url: string }; result: string };
  // ── System tray (src/Tray.SPEC.md) ──
  // The shell metrics tray icons have to be rasterized against ([S-03]).
  tray_env: { args: void; result: TrayEnv };
  // The WHOLE desired icon set, diffed backend-side ([S-02]). `icon` is an RGBA8
  // bitmap flattened row-major; `labels` carries the translated menu strings, so
  // a language switch re-labels the live menus.
  tray_sync: { args: { specs: TraySpec[]; labels: TrayMenuLabels }; result: void };
  tray_flyout_hide: { args: void; result: void };
  // [S-12] The flyout's document announcing which widget it mounted. Rust holds
  // the window hidden until this arrives, so a switch between icons never
  // flashes the previous widget.
  tray_flyout_ready: { args: { widget: string }; result: void };
  // [S-13] Pin a tray widget's flyout open as a live settings preview; `null`
  // releases the pin and closes it.
  tray_flyout_preview: { args: { widget: string | null }; result: void };
  set_interactive: { args: { label: string; interactive: boolean }; result: void };
  // [S-45] Whether any widget is pinned, so Rust builds / tears down the
  // per-monitor always-on-top overlay. Reported by every base overlay;
  // idempotent on the backend.
  set_pin_layer: { args: { enabled: boolean }; result: void };
  // [MI-04] `disabled` holds MonitorDescriptor.id values, never ordinals. Rust
  // re-keys the incoming list with the same [MI-03] rules at the boundary, but
  // that is a backstop for an older frontend — this one must send stable ids.
  sync_disabled_monitors: { args: { disabled: string[] }; result: void };
  update_widget_rects: { args: { rects: number[][]; interactive: boolean }; result: void };
  launch_app_layout: { args: { entries: LayoutEntry[] }; result: void };
  // `accept` is per-call: RSS negotiates for XML, Stocks/Gallery want JSON,
  // web-element wants HTML. Omitted means "no Accept header", the prior
  // behavior. Rejects non-2xx with the real status rather than handing back a
  // challenge/error page as if it were the document (#81).
  http_get: { args: { url: string; accept?: string }; result: string };
  // The live topology: identity + geometry in one call. (`get_monitor_work_areas`
  // was a strict subset of this and is gone — one path, [MI-05].)
  get_active_monitors: { args: void; result: MonitorDescriptor[] };
  set_game_mode_watcher: { args: { enabled: boolean }; result: void };
  // ── System monitors ──
  // includeProcs gates the process-table refresh + top lists (cpu [S-21],
  // ram [S-16]); includeTemp starts the WMI temperature sampler (cpu [S-20]).
  get_system_stats: { args: { includeProcs?: boolean; includeTemp?: boolean }; result: SystemStats };
  list_gpus: { args: void; result: GpuInfo[] };
  get_gpu_stats: { args: { gpuIndex: number }; result: GpuStats };
  ping_hosts: { args: { hosts: PingHost[] }; result: PingResult[] };
  get_battery_stats: { args: void; result: BatteryStats };
  // Screen-time tracking (screentime SPEC [S-30]); date defaults to today.
  screentime_get_day: { args: { date?: string }; result: ScreentimeDayStats };
  // Rolling window ending today, oldest first (screentime SPEC [S-45]).
  screentime_get_range: { args: { days: number }; result: ScreentimeDayStats[] };
  get_bluetooth_devices: { args: void; result: BluetoothDevice[] };
  list_start_menu_apps: { args: void; result: AppEntry[] };
  // ── Music ──
  music_get_now_playing: { args: void; result: NowPlayingInfo | null };
  music_play_pause: { args: void; result: void };
  music_next: { args: void; result: void };
  music_previous: { args: void; result: void };
  music_seek: { args: { positionMs: number }; result: void };
  music_start_watcher: { args: void; result: void };
  music_start_spectrum: { args: void; result: void };
  music_stop_spectrum: { args: void; result: void };
  // ── Volume ──
  volume_get: { args: void; result: VolumeState };
  volume_set: { args: { level: number }; result: void };
  volume_set_mute: { args: { muted: boolean }; result: void };
  // ── Mixer (per-application volume) ──
  mixer_get_sessions: { args: { grouped: boolean }; result: MixerApp[] };
  mixer_set_volume: { args: { key: string; level: number }; result: void };
  mixer_set_mute: { args: { key: string; muted: boolean }; result: void };
  // ── Power ──
  // `action` is the union, not `string`: the Rust side deserializes into an
  // enum, so a name this map does not list is a compile error here rather than
  // an IPC rejection at click time (power SPEC [S-02]).
  power_run: { args: { action: PowerActionName }; result: void };
  // ── Clipboard ──
  clipboard_set_files: { args: { paths: string[]; cut: boolean }; result: void };
  clipboard_get_files: { args: void; result: ClipboardFiles };
  clipboard_start: { args: void; result: void };
  clipboard_set: { args: { text: string }; result: void };
  clipboard_extract_virtual_files: { args: void; result: string[] };
  // ── Email ──
  email_list_accounts: { args: void; result: AccountInfo[] };
  email_start_device_auth: { args: void; result: DeviceAuthStart };
  email_poll_auth: { args: { deviceCode: string }; result: AccountInfo | null };
  email_start_github_device_auth: { args: void; result: DeviceAuthStart };
  email_poll_github_device_auth: { args: { deviceCode: string }; result: AccountInfo | null };
  email_start_google_auth: { args: void; result: GoogleAuthStart };
  email_complete_google_auth: { args: { state: string }; result: AccountInfo };
  email_get_messages: { args: { accountId: string; folder: string; count: number }; result: EmailMessage[] };
  email_sign_out_account: { args: { accountId: string }; result: void };
  email_rename_account: { args: { accountId: string; nickname: string }; result: void };
  email_get_folder_status: { args: { accountId: string; folder: string }; result: FolderStatus };
  email_mark_message_read: { args: { messageId: string; isRead: boolean }; result: void };
  email_delete_message: { args: { messageId: string }; result: void };
  email_move_message: { args: { messageId: string; destinationFolder: string }; result: void };
  email_open_url: { args: { url: string }; result: void };
  email_open_message: { args: { webLink: string; messageId: string }; result: void };
  email_reply_to_message: { args: { messageId: string; fromEmail: string; subject: string }; result: void };
  email_probe_endpoint: { args: { target: ProbeTarget }; result: ProbeResult };
  email_add_imap_account: { args: { host: string; port: number; username: string; password: string; displayName: string; nickname?: string | null }; result: AccountInfo };
  email_add_caldav_account: { args: { url: string; username: string; password: string; displayName: string; nickname?: string | null }; result: AccountInfo };
  email_add_carddav_account: { args: { url: string; username: string; password: string; displayName: string; nickname?: string | null }; result: AccountInfo };
  email_add_github_account: { args: { token: string; apiBaseUrl?: string | null; nickname?: string | null }; result: AccountInfo };
  email_add_rss_feed: { args: { url: string; nickname?: string | null }; result: AccountInfo };
  email_get_github_token: { args: { accountId: string }; result: string | null };
  email_get_github_api_base_url: { args: { accountId: string }; result: string | null };
  email_start_claude_auth: { args: void; result: ClaudeAuthStart };
  email_complete_claude_auth: { args: { code: string; state: string; nickname?: string | null }; result: AccountInfo };
  email_get_claude_usage: { args: { accountId: string }; result: ClaudeUsage };
  // ── Calendar ──
  calendar_get_events: { args: { accountId: string; startDt: string; endDt: string; calendarId?: string }; result: CalendarEvent[] };
  calendar_list_calendars: { args: { accountId: string }; result: CalendarInfo[] };
  calendar_get_birthdays: { args: { accountId: string }; result: ContactBirthday[] };
  calendar_open_event: { args: { webLink: string; eventId: string }; result: void };
  // ── To-do ──
  todo_get_lists: { args: { accountId: string }; result: TodoTaskList[] };
  todo_get_tasks: { args: { accountId: string; listId: string; showCompleted: boolean }; result: TodoTask[] };
  todo_update_task: { args: { accountId: string; listId: string; taskId: string; status: string }; result: void };
  todo_rename_task: { args: { accountId: string; listId: string; taskId: string; title: string }; result: void };
  // `dueDate` is a bare `YYYY-MM-DD`, sent only from the Today view [S-52].
  todo_create_task: { args: { accountId: string; listId: string; title: string; dueDate?: string }; result: TodoTask };
  todo_delete_task: { args: { accountId: string; listId: string; taskId: string }; result: void };
  // ── Contacts ──
  contacts_list: { args: { accountId: string; search: string; count: number }; result: Contact[] };
  // ── OneDrive ──
  onedrive_recent_files: { args: { accountId: string; count: number }; result: OneDriveItem[] };
  onedrive_list_folder: { args: { accountId: string; folderId: string; count: number }; result: OneDriveItem[] };
  // ── Notes (local store) ──
  notes_list: { args: void; result: NoteMeta[] };
  notes_read: { args: { id: string }; result: string };
  // Resolves to the new `updated_at` stamp, which the caller echo-suppresses.
  notes_write: { args: { id: string; content: string }; result: number };
  notes_create: { args: { title: string; content: string }; result: NoteMeta };
  notes_rename: { args: { id: string; title: string }; result: void };
  notes_delete: { args: { id: string }; result: void };
  // ── License ──
  license_activate: { args: { key: string }; result: LicenseStatus };
  license_validate: { args: void; result: LicenseStatus };
  license_deactivate: { args: void; result: void };
  license_status: { args: void; result: LicenseStatus };
  license_get_key: { args: void; result: string | null };
  newsletter_subscribe: { args: { email: string }; result: void };
  // ── Autostart ──
  autostart_is_enabled: { args: void; result: boolean };
  /** [AS-06] Resolves if autostart is configurable; rejects with an `AutostartError`. */
  autostart_can_configure: { args: void; result: void };
  autostart_enable: { args: void; result: void };
  autostart_disable: { args: void; result: void };
  // ── Analytics ──
  /** The user's analytics decision. `unset` until the first-run prompt is answered ([S-03]). */
  analytics_consent: { args: void; result: AnalyticsConsent };
  /**
   * Records the answer, from the prompt or from Settings. Revoking a consent
   * that was actually given returns a plan holding the single `app_opt_out`
   * signal the caller should send; every other answer returns an empty one
   * ([S-07]).
   */
  analytics_set_consent: { args: { granted: boolean }; result: AnalyticsPlan };
  /** True for the one window that should raise the first-run prompt ([S-27]). */
  analytics_claim_consent_prompt: { args: void; result: boolean };
  /** Reports this webview's environment and asks what is due ([S-18]). */
  analytics_plan: { args: { env?: AnalyticsEnv }; result: AnalyticsPlan };
  /** Builds the signal for something that just happened ([S-21]). */
  analytics_track: { args: { event: AnalyticsTrackedEvent }; result: AnalyticsPlan };
  /** Records how long the finished session ran, for the retention average. */
  analytics_session_end: { args: { seconds: number }; result: void };
  analytics_mark_sent: { args: { event: AnalyticsEvent }; result: void };
  // ── Diagnostics ──
  diag_window_state: { args: { label: string }; result: WindowDiagnostics };
  diag_send_show_desktop: { args: void; result: void };
  diag_click_and_measure_start_close: { args: { x: number; y: number; timeoutMs: number }; result: number | null };
  diag_toggle_desktop: { args: void; result: number };
  diag_z_counters: { args: void; result: ZOrderCounters };
  diag_reset_counters: { args: void; result: void };
  diag_compare_zorder: { args: { widgetLabel: string; otherClass: string }; result: ZComparison };
  diag_window_above: { args: { topLabel: string; bottomLabel: string }; result: boolean };
  diag_close_windows_by_prefix: { args: { prefix: string }; result: string[] };
  diag_demote_widget: { args: { label: string }; result: DemoteResult };
  diag_set_polling_paused: { args: { paused: boolean }; result: void };
  diag_list_widgets: { args: void; result: WidgetSummary[] };
  diag_force_topmost: { args: { label: string }; result: ForceTopmostResult };
  diag_analytics_test: { args: void; result: string };
}

export type CommandName = keyof Commands;

/**
 * Runtime mirror of the keys in `Commands`. Append here + to `Commands` together
 * when typing a command; the `_AssertInSync` check below is a compile error if
 * the two diverge, and `ipc.test.ts` checks every name here exists in the Rust
 * `generate_handler!` list.
 */
export const TYPED_COMMANDS = [
  'open_url', 'quit_app', 'open_file', 'open_in_explorer', 'fs_exists', 'recycle_bin_status',
  'get_system_locale', 'list_system_fonts', 'get_monitor_id', 'show_devtools', 'set_ssrf_bypass',
  'resolve_shortcut', 'get_file_icon', 'get_file_icons_batch', 'fs_paste',
  'fs_delete', 'fs_delete_permanent', 'fs_restore_from_trash', 'fs_rename',
  'show_file_properties', 'open_with', 'fs_write_file', 'get_folder_item_counts',
  'get_image_thumbnail', 'get_image_thumbnails_batch', 'fs_download_url', 'get_clipboard_url',
  'get_cloud_sync_folders', 'get_shell_new_entries', 'fs_create_folder', 'get_shell_menu_entries',
  'execute_shell_command', 'get_file_attributes', 'get_file_attributes_batch', 'folder_read_dir',
  'get_file_stats_batch', 'set_cloud_pin', 'get_git_status', 'get_git_repo_info_batch',
  'scan_images', 'read_image_base64',
  'start_native_drag', 'save_virtual_files', 'set_drag_over_zone',
  'push_windows_to_desktop', 'set_desktop_icons', 'get_desktop_icons',
  'repair_legacy_desktop_icon_hide',
  'clear_desktop_selection', 'set_log_stream', 'get_cursor_state',
  'activate_widget_keyboard', 'deactivate_widget_keyboard', 'set_text_input_focused', 'set_escape_wanted',
  'dismiss_start_menu_if_open', 'dismiss_context_menu_if_open',
  'close_devtools', 'set_logging_enabled', 'open_log_folder',
  'open_config_window', 'open_connect_window', 'open_layout_window',
  'open_layout_app_picker_window', 'open_shortcut_add_window', 'open_widget_window', 'open_web_element_picker_window', 'web_element_render_html',
  'tray_env', 'tray_sync', 'tray_flyout_hide', 'tray_flyout_ready', 'tray_flyout_preview',
  'set_interactive', 'set_pin_layer', 'sync_disabled_monitors', 'update_widget_rects',
  'launch_app_layout', 'http_get', 'get_active_monitors',
  'set_game_mode_watcher',
  'get_system_stats', 'list_gpus', 'get_gpu_stats', 'ping_hosts',
  'get_battery_stats', 'screentime_get_day', 'screentime_get_range', 'get_bluetooth_devices', 'list_start_menu_apps',
  'music_get_now_playing', 'music_play_pause', 'music_next', 'music_previous',
  'music_seek', 'music_start_watcher', 'music_start_spectrum', 'music_stop_spectrum',
  'volume_get', 'volume_set', 'volume_set_mute',
  'mixer_get_sessions', 'mixer_set_volume', 'mixer_set_mute',
  'power_run',
  'clipboard_set_files', 'clipboard_get_files', 'clipboard_start', 'clipboard_set',
  'clipboard_extract_virtual_files',
  'email_list_accounts', 'email_start_device_auth', 'email_poll_auth',
  'email_start_github_device_auth', 'email_poll_github_device_auth', 'email_start_google_auth',
  'email_complete_google_auth', 'email_get_messages', 'email_sign_out_account', 'email_rename_account',
  'email_get_folder_status', 'email_mark_message_read', 'email_delete_message', 'email_move_message',
  'email_open_url', 'email_open_message', 'email_reply_to_message', 'email_probe_endpoint', 'email_add_imap_account',
  'email_add_caldav_account', 'email_add_carddav_account', 'email_add_github_account', 'email_add_rss_feed',
  'email_get_github_token', 'email_get_github_api_base_url',
  'email_start_claude_auth', 'email_complete_claude_auth', 'email_get_claude_usage',
  'calendar_get_events', 'calendar_list_calendars', 'calendar_get_birthdays', 'calendar_open_event',
  'todo_get_lists', 'todo_get_tasks', 'todo_update_task', 'todo_rename_task',
  'todo_create_task', 'todo_delete_task',
  'contacts_list',
  'onedrive_recent_files', 'onedrive_list_folder',
  'notes_list', 'notes_read', 'notes_write', 'notes_create', 'notes_rename', 'notes_delete',
  'license_activate', 'license_validate', 'license_deactivate', 'license_status',
  'license_get_key', 'newsletter_subscribe',
  'autostart_is_enabled', 'autostart_can_configure', 'autostart_enable', 'autostart_disable',
  'analytics_consent', 'analytics_set_consent', 'analytics_claim_consent_prompt',
  'analytics_plan', 'analytics_track',
  'analytics_session_end', 'analytics_mark_sent',
  'diag_window_state', 'diag_send_show_desktop', 'diag_click_and_measure_start_close', 'diag_toggle_desktop',
  'diag_z_counters', 'diag_reset_counters', 'diag_compare_zorder', 'diag_window_above',
  'diag_close_windows_by_prefix', 'diag_demote_widget', 'diag_set_polling_paused', 'diag_list_widgets',
  'diag_force_topmost', 'diag_analytics_test',
] as const;

// Compile-time guard: `TYPED_COMMANDS` and the `Commands` keys are the same set.
type AssertSameSet<A extends string, B extends string> =
  [A] extends [B] ? ([B] extends [A] ? true : never) : never;
const _AssertInSync: AssertSameSet<CommandName, typeof TYPED_COMMANDS[number]> = true;
void _AssertInSync;

export type ArgsOf<K extends CommandName> = Commands[K]['args'];
export type InvokeRest<K extends CommandName> = ArgsOf<K> extends void ? [] : [args: ArgsOf<K>];

// Test-only seam. Tauri 2 defines `__TAURI_INTERNALS__.invoke` as non-writable,
// so e2e specs can't stub the IPC at the window level; and every core surface
// (Settings, ConfigWindow, operationLog, …) calls through the typed `invoke`
// below. Route it through a swappable impl so webdriver specs can record/stub
// any command in one place (mirrors operationLog's `__opLog` and the demo seam).
// Inert in release: `window.__ipcInvoke` is only created when `navigator.webdriver`
// is true, which it never is in a normal WebView2 session.
type RawInvoke = (cmd: string, args?: Record<string, unknown>) => Promise<unknown>;
const defaultInvoke: RawInvoke = (cmd, args) => rawInvoke(cmd, args);
let invokeImpl: RawInvoke = defaultInvoke;

/**
 * Typed `invoke`. No-arg commands are called `invoke('name')`; commands with args
 * are called `invoke('name', { ... })`, both shapes enforced by the types. The
 * return type is the command's Ok value (the promise rejects on a Rust `Err`).
 */
export function invoke<K extends CommandName>(
  cmd: K,
  ...rest: InvokeRest<K>
): Promise<Commands[K]['result']> {
  return invokeImpl(cmd, rest[0] as Record<string, unknown> | undefined) as Promise<Commands[K]['result']>;
}

declare global {
  interface Window {
    // Test-only hook (webdriver): swap the invoke transport every typed `invoke` uses.
    __ipcInvoke?: { setInvoke: (fn: RawInvoke | null) => void };
  }
}

if (typeof window !== 'undefined' && navigator.webdriver) {
  window.__ipcInvoke = {
    setInvoke: (fn: RawInvoke | null) => { invokeImpl = fn ?? defaultInvoke; },
  };
}
