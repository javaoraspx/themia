use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

/// CLI flag passed *only* by the app's own autostart scheduled task
/// (`mod autostart`, emitted in `build_task_xml`). Its presence distinguishes a
/// silent login launch (widgets only) from a manual launch (Start menu,
/// Explorer, double-click), which opens the settings window instead — see the
/// `show_config_window` call sites in `run()`.
///
/// Consequence for #69: a *manually* added Windows startup entry (Startup
/// folder shortcut or HKCU\...\Run) runs the BARE exe with no flag, so it is
/// correctly treated as a manual launch and opens Settings. That is by design,
/// not the bug. Users who want widgets-only at login should use the in-app
/// "Launch on startup" toggle (which installs this task), or, if adding an
/// entry by hand, append the `--autostart` argument to the command.
const AUTOSTART_FLAG: &str = "--autostart";

/// [WA-02] CLI flag passed *only* by the e2e harness (`tests/e2e/wdio.conf.ts`
/// capabilities). Windows that a user-initiated action would normally bring to
/// the front — the settings window, the connect wizard, widget popouts — are
/// instead shown without activation, so a test run never yanks the foreground
/// out of whatever the developer is typing in. It is a presentation switch
/// only: the same windows are created, at the same time, with the same content.
const NO_ACTIVATE_FLAG: &str = "--no-activate";

/// Pure argv predicate for [WA-02], split from the process lookup so the flag
/// contract is unit-testable.
fn args_request_no_activate<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter().any(|a| a.as_ref() == NO_ACTIVATE_FLAG)
}

/// [WA-02] Cached: argv cannot change after launch.
fn no_activate_mode() -> bool {
    static MODE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| args_request_no_activate(std::env::args()))
}

/// [WA-02] Show a freshly built, user-invoked window and give it the keyboard.
/// Under `--no-activate` it becomes visible without taking the foreground.
fn present_window(win: &tauri::WebviewWindow) {
    if !no_activate_mode() {
        let _ = win.show();
        let _ = win.set_focus();
        return;
    }
    #[cfg(target_os = "windows")]
    match win.hwnd() {
        Ok(h) => show_without_activating(h.0 as usize as winapi::shared::windef::HWND),
        Err(e) => log::warn!(target: "window", "no-activate show: no hwnd: {e}"),
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = win.show();
    }
}

/// [WA-02] Make `hwnd` visible — restoring it if minimized — without taking the
/// foreground. Used instead of `show()` and of `unminimize()`, whose `SW_RESTORE`
/// activates. NOT `WS_EX_NOACTIVATE`: that was tried and it stops WebView2 from
/// coming up at all for these windows (`config window kind="add" did not appear`),
/// so the guarantee stays per-call-site.
#[cfg(target_os = "windows")]
fn show_without_activating(hwnd: winapi::shared::windef::HWND) {
    use winapi::um::winuser::{ShowWindow, SW_SHOWNOACTIVATE};
    if hwnd.is_null() {
        return;
    }
    // SAFETY: live HWND owned by a Tauri window, freed by Tauri on close.
    unsafe {
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
}

/// [WA-02] Re-surface a window that is already open (the idempotent branch of
/// every "open X" command). Under `--no-activate` a minimized window still
/// comes back, it just does so without taking the foreground.
fn refocus_window(win: &tauri::WebviewWindow) {
    if !no_activate_mode() {
        let _ = win.set_focus();
        let _ = win.unminimize();
        return;
    }
    // `unminimize()` is `SW_RESTORE`, which activates. The non-activating
    // restore is the same `SW_SHOWNOACTIVATE` that shows a hidden window.
    #[cfg(target_os = "windows")]
    match win.hwnd() {
        Ok(h) => show_without_activating(h.0 as usize as winapi::shared::windef::HWND),
        Err(e) => log::warn!(target: "window", "no-activate restore: no hwnd: {e}"),
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = win.unminimize();
    }
}

#[cfg(target_os = "windows")]
mod drag_drop;

mod game_mode;

mod analytics;

/// [LOG-09] Windows Application event log sink for ERROR records.
mod eventlog;

mod tray;

#[cfg(target_os = "windows")]
mod autostart {
    // Autostart via Windows Scheduled Task (LogonTrigger).
    // Avoids the ~10s StartupDelayInMSec throttle that applies to
    // HKCU\...\Run entries and Startup-folder shortcuts.
    //
    // Uses `schtasks /Create /XML` (not `/SC ONLOGON`) because the latter
    // requires elevation even when scoped to the current user, which would
    // silently fail on non-elevated production installs.
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    use std::ptr;
    use winapi::um::winreg::{RegOpenKeyExW, RegDeleteValueW, RegCloseKey, HKEY_CURRENT_USER};
    use winapi::um::winnt::KEY_WRITE;

    pub const TASK_NAME: &str = "Themia";
    /// Throwaway task for the [AS-06] permission probe. MUST differ from
    /// `TASK_NAME` — the probe deletes it, and deleting the real autostart task
    /// would silently turn the feature off.
    const PROBE_TASK_NAME: &str = "Themia autostart permission probe";
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const LEGACY_RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run\0";
    const LEGACY_VAL_NAME: &str = "Themia\0";

    /// Does OUR task exist at all, regardless of whether it would run?
    ///
    /// Deliberately distinct from `is_enabled()`. The first-run/migration path
    /// asks THIS question: a user who switched the task off in Task Scheduler
    /// has made a choice, and answering "not enabled" there would have us
    /// recreate it — silently overriding them on every launch.
    pub fn task_exists() -> bool {
        Command::new("schtasks")
            .args(["/Query", "/TN", TASK_NAME])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Is autostart really on? ([AS-07])
    ///
    /// "A task named Themia exists" is NOT the question — that was the old
    /// check, and it reports Enabled for a task that cannot launch anything:
    /// one the user switched off in Task Scheduler, or whose LogonTrigger is
    /// disabled. That is #69's actual report ("boot computer / no start up"
    /// while the setting looked on). So the task's XML is read and the two
    /// switches that decide whether Windows will run it are checked.
    ///
    /// `Err` means we could not ask at all (schtasks missing / not spawnable),
    /// which is NOT the same as "off" and must not be silently rendered as a
    /// confident Disabled. A non-zero exit IS reported as `Ok(false)`: the
    /// overwhelmingly common cause is "task does not exist", and schtasks does
    /// not give a machine-readable way to tell that from an unreadable task.
    pub fn is_enabled() -> Result<bool, String> {
        let out = Command::new("schtasks")
            .args(["/Query", "/TN", TASK_NAME, "/XML"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|e| format!("failed to run schtasks /Query: {e}"))?;
        if !out.status.success() {
            return Ok(false);
        }
        Ok(task_xml_is_enabled(&schtasks_xml_to_string(&out.stdout)))
    }

    /// `schtasks /XML` emits UTF-16 LE; decode lossily for substring checks.
    fn schtasks_xml_to_string(stdout: &[u8]) -> String {
        String::from_utf16_lossy(
            &stdout
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect::<Vec<u16>>(),
        )
    }

    /// Contents between `<name>` and `</name>`, or None if either is missing.
    fn section<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
        let open = format!("<{name}>");
        let close = format!("</{name}>");
        let start = xml.find(&open)? + open.len();
        let end = xml[start..].find(&close)? + start;
        Some(&xml[start..end])
    }

    /// Pure: will Windows actually run this task at logon? Split out so every
    /// "looks on but never runs" shape is unit-testable without a scheduler.
    fn task_xml_is_enabled(xml: &str) -> bool {
        // Task-level switch. A missing <Settings> block means the scheduler
        // default (enabled), so absence is not treated as off.
        let task_on = section(xml, "Settings")
            .is_none_or(|s| !s.contains("<Enabled>false</Enabled>"));
        // A task with no logon trigger never fires at login, whatever its
        // task-level switch says.
        let triggers = match section(xml, "Triggers") {
            Some(t) if t.contains("<LogonTrigger>") => t,
            _ => return false,
        };
        let trigger_on = !triggers.contains("<Enabled>false</Enabled>");
        task_on && trigger_on
    }

    /// True if an installed task exists but lacks the `--autostart` argument
    /// (i.e. it was created by an older build, before the manual-vs-autostart
    /// distinction existed). Triggers a one-time refresh via `enable()` so the
    /// task starts passing the flag. Returns false if the query fails, since
    /// it's better to leave a working task alone than churn it on a transient error.
    pub fn needs_arg_migration() -> bool {
        let out = Command::new("schtasks")
            .args(["/Query", "/TN", TASK_NAME, "/XML"])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        match out {
            Ok(o) if o.status.success() => {
                !schtasks_xml_to_string(&o.stdout).contains(crate::AUTOSTART_FLAG)
            }
            _ => false,
        }
    }

    /// Pure mapping from a finished `schtasks` invocation to a `Result`, split
    /// out so the failure path is unit-testable without spawning a process
    /// (#69). A non-zero exit carries the exit code AND the captured stderr —
    /// schtasks writes its diagnostics ("ERROR: Access is denied.", policy
    /// blocks, etc.) to stderr — so the caller can surface WHY autostart could
    /// not be configured instead of swallowing it into a silent `false`.
    ///
    /// `stderr` is best-effort decoded as UTF-8 (schtasks emits its error text
    /// in the console codepage; the ASCII "ERROR: …" prefix survives lossily).
    fn map_schtasks_result(
        op: &str,
        success: bool,
        exit_code: Option<i32>,
        stderr: &[u8],
    ) -> Result<(), String> {
        if success {
            return Ok(());
        }
        let detail = String::from_utf8_lossy(stderr);
        let detail = detail.trim();
        let code = exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        if detail.is_empty() {
            Err(format!("schtasks {op} failed (exit code {code})"))
        } else {
            Err(format!("schtasks {op} failed (exit code {code}): {detail}"))
        }
    }

    pub fn enable() -> Result<(), String> {
        remove_legacy_run_entry();
        let exe_path = std::env::current_exe()
            .map_err(|e| format!("cannot resolve current executable path: {e}"))?;
        let exe_str = exe_path.to_string_lossy().to_string();
        let user = current_user_id();
        let xml = build_task_xml(&user, &exe_str);

        // Write XML to a temp file (UTF-16 LE with BOM, since schtasks requires it).
        let mut xml_path = std::env::temp_dir();
        xml_path.push("themia_autostart_task.xml");
        write_utf16_le_bom(&xml_path, &xml)
            .map_err(|e| format!("cannot write autostart task XML to {}: {e}", xml_path.display()))?;

        // `.output()` (not `.status()`) so schtasks' stderr is captured and can
        // be reported; CREATE_NO_WINDOW keeps the console hidden.
        let output = Command::new("schtasks")
            .args([
                "/Create",
                "/TN", TASK_NAME,
                "/XML", &xml_path.to_string_lossy(),
                "/F",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output();

        let _ = std::fs::remove_file(&xml_path); // ignore: temp-file cleanup, best-effort

        match output {
            Ok(o) => map_schtasks_result("/Create", o.status.success(), o.status.code(), &o.stderr),
            Err(e) => Err(format!("failed to run schtasks /Create: {e}")),
        }
    }

    pub fn disable() -> Result<(), String> {
        remove_legacy_run_entry();
        let output = Command::new("schtasks")
            .args(["/Delete", "/TN", TASK_NAME, "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        match output {
            Ok(o) => map_schtasks_result("/Delete", o.status.success(), o.status.code(), &o.stderr),
            Err(e) => Err(format!("failed to run schtasks /Delete: {e}")),
        }
    }

    /// Dry-run of the ONE operation autostart needs and that #69/#79 were
    /// blocked on: creating a task in the Task Scheduler root folder ([AS-06]).
    /// Lets the UI warn BEFORE the user clicks, instead of only after.
    ///
    /// It is a real create+delete, not an elevation check: a standard user CAN
    /// normally create a per-user task (that is why [AS-01] uses `/Create /XML`
    /// rather than `/SC ONLOGON`), so `is_elevated()` would report a problem for
    /// the large majority of users who have none.
    ///
    /// The probe task is deliberately INERT — no `<Triggers>`,
    /// `<Enabled>false</Enabled>`, and `cmd /c exit` as its action — so if the
    /// cleanup delete ever fails, what leaks is a disabled do-nothing entry, not
    /// a second thing launching Themia at every logon.
    pub fn can_configure() -> Result<(), String> {
        let xml = build_probe_task_xml(&current_user_id());

        let mut xml_path = std::env::temp_dir();
        xml_path.push("themia_autostart_probe.xml");
        write_utf16_le_bom(&xml_path, &xml)
            .map_err(|e| format!("cannot write autostart task XML to {}: {e}", xml_path.display()))?;

        let output = Command::new("schtasks")
            .args([
                "/Create",
                "/TN", PROBE_TASK_NAME,
                "/XML", &xml_path.to_string_lossy(),
                "/F",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output();

        let _ = std::fs::remove_file(&xml_path); // ignore: temp-file cleanup, best-effort

        let created = match output {
            Ok(o) => map_schtasks_result("/Create (probe)", o.status.success(), o.status.code(), &o.stderr),
            Err(e) => Err(format!("failed to run schtasks /Create: {e}")),
        };
        if created.is_ok() {
            delete_probe_task();
        }
        created
    }

    /// Remove the [AS-06] probe task. Best-effort on purpose: a failure here
    /// does NOT make the probe report a permission problem — creating worked,
    /// which is the thing being measured. A leak is loud in the log and inert
    /// on the machine.
    fn delete_probe_task() {
        let out = Command::new("schtasks")
            .args(["/Delete", "/TN", PROBE_TASK_NAME, "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        let failed = match out {
            Ok(o) if o.status.success() => None,
            Ok(o) => Some(String::from_utf8_lossy(&o.stderr).trim().to_string()),
            Err(e) => Some(e.to_string()),
        };
        if let Some(why) = failed {
            log::warn!("autostart: could not remove probe task {PROBE_TASK_NAME}: {why}");
        }
    }

    /// Inert task used only to test whether we may write to the Task Scheduler
    /// root folder. No triggers, disabled, and its action exits immediately.
    fn build_probe_task_xml(user: &str) -> String {
        let user_esc = xml_escape(user);
        format!(
            r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Principals>
    <Principal id="Author">
      <UserId>{user_esc}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <Enabled>false</Enabled>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <ExecutionTimeLimit>PT1M</ExecutionTimeLimit>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>cmd.exe</Command>
      <Arguments>/c exit</Arguments>
    </Exec>
  </Actions>
</Task>"#
        )
    }

    /// RAII wrapper for a process token handle: closed on every path, including
    /// an early return or an unwind out of `is_elevated`.
    struct TokenHandle(winapi::um::winnt::HANDLE);

    impl Drop for TokenHandle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { winapi::um::handleapi::CloseHandle(self.0) };
            }
        }
    }

    /// True when this process runs elevated ("Run as administrator").
    ///
    /// Used ONLY to pick which remedy to offer when a `schtasks` call fails
    /// (#69/#79): "run Themia as administrator" is the fix when the user isn't
    /// elevated, and misleading when they already are (then it's group policy
    /// or a disabled Task Scheduler service, and elevating changes nothing).
    /// Read from the process token rather than by matching schtasks' stderr,
    /// which is localized and would misclassify on a non-English Windows.
    ///
    /// On a token query failure this reports `false`, i.e. it still suggests
    /// elevation: an unnecessary suggestion is harmless, silence is what #79
    /// was about.
    pub fn is_elevated() -> bool {
        use winapi::um::processthreadsapi::{GetCurrentProcess, OpenProcessToken};
        use winapi::um::securitybaseapi::GetTokenInformation;
        use winapi::um::winnt::{TokenElevation, HANDLE, TOKEN_ELEVATION, TOKEN_QUERY};

        unsafe {
            let mut raw: HANDLE = ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) == 0 {
                return false;
            }
            let token = TokenHandle(raw);
            let mut info = TOKEN_ELEVATION { TokenIsElevated: 0 };
            let mut ret_len: u32 = 0;
            let ok = GetTokenInformation(
                token.0,
                TokenElevation,
                &mut info as *mut _ as *mut _,
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut ret_len,
            );
            ok != 0 && info.TokenIsElevated != 0
        }
    }

    fn current_user_id() -> String {
        let domain = std::env::var("USERDOMAIN").unwrap_or_default();
        let user = std::env::var("USERNAME").unwrap_or_default();
        if domain.is_empty() { user } else { format!("{domain}\\{user}") }
    }

    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    }

    fn build_task_xml(user: &str, exe_path: &str) -> String {
        let user_esc = xml_escape(user);
        let exe_esc = xml_escape(exe_path);
        let flag = xml_escape(crate::AUTOSTART_FLAG);
        format!(
            r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user_esc}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user_esc}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe_esc}</Command>
      <Arguments>{flag}</Arguments>
    </Exec>
  </Actions>
</Task>"#
        )
    }

    fn write_utf16_le_bom(path: &std::path::Path, s: &str) -> std::io::Result<()> {
        use std::io::Write;
        let mut bytes: Vec<u8> = vec![0xFF, 0xFE]; // UTF-16 LE BOM
        for u in s.encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        let mut f = std::fs::File::create(path)?;
        f.write_all(&bytes)
    }

    fn remove_legacy_run_entry() {
        unsafe {
            let mut hkey = ptr::null_mut();
            let run_key: Vec<u16> = LEGACY_RUN_KEY.encode_utf16().collect();
            let val_name: Vec<u16> = LEGACY_VAL_NAME.encode_utf16().collect();
            if RegOpenKeyExW(HKEY_CURRENT_USER, run_key.as_ptr(), 0, KEY_WRITE, &mut hkey) == 0 {
                RegDeleteValueW(hkey, val_name.as_ptr());
                RegCloseKey(hkey);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------
    #[cfg(test)]
    mod tests {
        use super::*;

        // --- Pure unit tests ------------------------------------------------

        #[test]
        fn xml_escape_handles_all_special_chars() {
            assert_eq!(xml_escape("a&b<c>d\"e'f"), "a&amp;b&lt;c&gt;d&quot;e&apos;f");
            assert_eq!(xml_escape("plain"), "plain");
            assert_eq!(xml_escape(""), "");
        }

        #[test]
        fn build_task_xml_contains_user_and_exe() {
            let xml = build_task_xml("MYPC\\alice", r"C:\Program Files\Themia\themia.exe");
            // Both placeholders substituted.
            assert!(xml.contains("<UserId>MYPC\\alice</UserId>"));
            assert!(xml.contains(r"<Command>C:\Program Files\Themia\themia.exe</Command>"));
            // The autostart marker arg lets a login launch be told apart from
            // a manual one (which should open the settings window instead).
            assert!(xml.contains("<Arguments>--autostart</Arguments>"));
            // Core structural elements present.
            assert!(xml.contains("<LogonTrigger>"));
            assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
            assert!(xml.contains("<RunLevel>LeastPrivilege</RunLevel>"));
            // UTF-16 declaration: schtasks /XML requires matching BOM encoding.
            assert!(xml.starts_with(r#"<?xml version="1.0" encoding="UTF-16"?>"#));
        }

        #[test]
        fn build_task_xml_escapes_ampersand_in_path() {
            let xml = build_task_xml("DOM\\u", r"C:\A&B\app.exe");
            assert!(xml.contains(r"<Command>C:\A&amp;B\app.exe</Command>"));
            assert!(!xml.contains(r"C:\A&B\app.exe"));
        }

        #[test]
        fn write_utf16_le_bom_emits_bom_then_le_code_units() {
            let mut p = std::env::temp_dir();
            p.push(format!("themia_test_bom_{}.bin", std::process::id()));
            write_utf16_le_bom(&p, "Hi").expect("write");
            let bytes = std::fs::read(&p).expect("read");
            let _ = std::fs::remove_file(&p);
            assert_eq!(&bytes[..2], &[0xFF, 0xFE], "missing UTF-16 LE BOM");
            // 'H' = 0x48, 'i' = 0x69 in LE → 0x48 0x00 0x69 0x00
            assert_eq!(&bytes[2..], &[0x48, 0x00, 0x69, 0x00]);
        }

        // --- #69: schtasks failures must surface, not be swallowed ----------
        //
        // Regression for issue #69 "Launch on startup not working": a failed
        // `schtasks /Create` (e.g. Task Scheduler restricted by policy) used to
        // be swallowed into a silent `false`, so the toggle flipped ON while
        // nothing launched at login. `map_schtasks_result` is the pure mapping
        // from a finished invocation → Result; the failure branch must carry
        // the schtasks exit code and stderr so a toast can explain WHY.

        #[test]
        fn s69_schtasks_failure_surfaces_error() {
            let err = map_schtasks_result(
                "/Create",
                false,
                Some(1),
                b"ERROR: Access is denied.",
            )
            .expect_err("non-zero exit must map to Err, not a silent success");
            assert!(
                err.contains("Access is denied"),
                "error must carry schtasks stderr, got: {err}"
            );
            assert!(err.contains("/Create"), "error should name the failed operation: {err}");
            assert!(err.contains('1'), "error should include the exit code: {err}");
        }

        #[test]
        fn s69_schtasks_success_maps_to_ok() {
            assert!(map_schtasks_result("/Create", true, Some(0), b"").is_ok());
            // Success ignores whatever noise is on stderr.
            assert!(map_schtasks_result("/Delete", true, Some(0), b"warn: something").is_ok());
        }

        #[test]
        fn s69_schtasks_failure_without_stderr_still_errs() {
            // Some failures produce no stderr; must still be a descriptive Err,
            // never a silent success.
            let err = map_schtasks_result("/Delete", false, None, b"")
                .expect_err("failure must map to Err even without stderr");
            assert!(err.contains("/Delete"), "error should name the operation: {err}");
        }

        // --- [AS-07]: "Enabled" must mean it will actually run --------------
        //
        // #69 was reported as "Launch on startup isn't working ... boot
        // computer, no start up" — the setting LOOKED on. The old check only
        // asked whether a task named Themia existed, which is true for several
        // tasks that never launch anything.

        /// Our real task XML, with the two switches parameterised.
        fn task_xml(task_enabled: bool, trigger_enabled: bool) -> String {
            format!(
                r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers>
    <LogonTrigger>
      <Enabled>{trigger_enabled}</Enabled>
      <UserId>PC\u</UserId>
    </LogonTrigger>
  </Triggers>
  <Settings>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <Enabled>{task_enabled}</Enabled>
    <Hidden>false</Hidden>
  </Settings>
</Task>"#
            )
        }

        #[test]
        fn as07_enabled_task_with_logon_trigger_reads_as_on() {
            assert!(task_xml_is_enabled(&task_xml(true, true)));
        }

        #[test]
        fn as07_task_switched_off_in_task_scheduler_reads_as_off() {
            // The #69 shape: the task is right there, and does nothing.
            assert!(!task_xml_is_enabled(&task_xml(false, true)));
        }

        #[test]
        fn as07_disabled_logon_trigger_reads_as_off() {
            assert!(!task_xml_is_enabled(&task_xml(true, false)));
        }

        #[test]
        fn as07_task_without_a_logon_trigger_reads_as_off() {
            // A trigger-less task never fires at login however "enabled" it is
            // — this is also exactly the shape of the [AS-06] probe task, so a
            // leaked probe must never be mistaken for working autostart.
            let xml = build_probe_task_xml("PC\\u");
            assert!(!task_xml_is_enabled(&xml));
        }

        #[test]
        fn as07_our_own_generated_task_reads_as_on() {
            // Round-trip against the XML `enable()` actually writes, so the two
            // can never drift into "we wrote it, we can't read it".
            let xml = build_task_xml("PC\\u", r"C:\Themia\themia.exe");
            assert!(task_xml_is_enabled(&xml), "enable()'s own XML must read back as enabled");
        }

        // Characterization: Task Scheduler does NOT echo back what we wrote. It
        // normalizes, and in particular OMITS any `<Enabled>` that equals the
        // default (true) — so the two off-states look nothing like each other,
        // and neither looks like `build_task_xml`'s output. Both fixtures below
        // are literal `schtasks /Query /XML` output captured from Windows 11.
        // A reader written against our own XML alone would pass its tests and
        // still misreport on a real machine.

        /// Task switched off in Task Scheduler: `<Settings>` carries the false,
        /// and the trigger's `<Enabled>` is dropped entirely.
        const EMITTED_TASK_DISABLED: &str = r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Settings>
    <Enabled>false</Enabled>
  </Settings>
  <Triggers>
    <LogonTrigger>
    </LogonTrigger>
  </Triggers>
</Task>"#;

        /// Trigger switched off: `<Settings>` drops its `<Enabled>` (default
        /// true) and the false moves into the trigger.
        const EMITTED_TRIGGER_DISABLED: &str = r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Settings>
  </Settings>
  <Triggers>
    <LogonTrigger>
      <Enabled>false</Enabled>
    </LogonTrigger>
  </Triggers>
</Task>"#;

        #[test]
        fn as07_reads_real_schtasks_output_for_a_disabled_task() {
            assert!(!task_xml_is_enabled(EMITTED_TASK_DISABLED));
        }

        #[test]
        fn as07_reads_real_schtasks_output_for_a_disabled_trigger() {
            // The subtle one: nothing says "false" at task level here, and the
            // omitted default must not be read as off either.
            assert!(!task_xml_is_enabled(EMITTED_TRIGGER_DISABLED));
        }

        #[test]
        fn as07_omitted_enabled_defaults_to_on() {
            // Both defaults omitted = a perfectly healthy task.
            let xml = "<Task><Settings>\n</Settings><Triggers><LogonTrigger>\n</LogonTrigger></Triggers></Task>";
            assert!(task_xml_is_enabled(xml));
        }

        #[test]
        fn as07_section_extracts_and_tolerates_missing_blocks() {
            assert_eq!(section("<a><b>x</b></a>", "b"), Some("x"));
            assert_eq!(section("<a></a>", "b"), None);
            assert_eq!(section("<b>unclosed", "b"), None);
        }

        // --- [AS-06]: the up-front permission probe -------------------------

        #[test]
        fn probe_task_is_inert() {
            let xml = build_probe_task_xml("MYPC\\alice");
            // A leaked probe must never launch anything at logon: no trigger,
            // disabled, and an action that exits immediately.
            assert!(!xml.contains("<Triggers>"), "probe task must have no trigger");
            assert!(!xml.contains("LogonTrigger"), "probe task must have no logon trigger");
            assert!(xml.contains("<Enabled>false</Enabled>"), "probe task must be disabled");
            assert!(xml.contains("<Command>cmd.exe</Command>"));
            assert!(xml.contains("<Arguments>/c exit</Arguments>"));
            // It must NOT carry the autostart marker, or a leak would look like
            // a real login launch to anything inspecting the task store.
            assert!(!xml.contains(crate::AUTOSTART_FLAG));
            assert!(xml.contains("<UserId>MYPC\\alice</UserId>"));
        }

        #[test]
        fn probe_task_name_differs_from_the_real_one() {
            // The probe deletes its task. Sharing the name would silently turn
            // the user's autostart off every time Settings opened.
            assert_ne!(PROBE_TASK_NAME, TASK_NAME);
            assert!(!PROBE_TASK_NAME.is_empty());
        }

        #[test]
        fn can_configure_succeeds_and_leaves_nothing_behind() {
            // Live round-trip: proves schtasks ACCEPTS the trigger-less probe
            // XML (if it didn't, every user would see a false warning), and
            // that the probe task is gone afterwards.
            delete_task(PROBE_TASK_NAME); // safety net for a previous run
            let result = can_configure();
            let leaked = task_exists(PROBE_TASK_NAME);
            delete_task(PROBE_TASK_NAME);

            assert!(result.is_ok(), "probe failed on a machine that can create tasks: {result:?}");
            assert!(!leaked, "probe task survived can_configure()");
        }

        // --- #79: the elevation probe must actually probe -------------------

        /// `is_elevated()` decides whether the UI offers "run as administrator"
        /// ([AS-05]), and its failure mode is a silent `false` — which looks
        /// exactly like a correct "not elevated" answer. Cross-check it against
        /// an independent signal: the token's integrity level from `whoami
        /// /groups`, where S-1-16-12288 (High Mandatory Level) means elevated.
        /// SID strings, so this holds on a non-English Windows.
        #[test]
        fn s79_is_elevated_matches_token_integrity_level() {
            let out = Command::new("whoami")
                .args(["/groups"])
                .creation_flags(CREATE_NO_WINDOW)
                .output()
                .expect("spawn whoami");
            assert!(out.status.success(), "whoami /groups failed");
            let text = String::from_utf8_lossy(&out.stdout);
            let high_integrity = text.contains("S-1-16-12288");
            assert_eq!(
                is_elevated(),
                high_integrity,
                "is_elevated() disagrees with the token integrity level; \
                 a broken probe would silently report not-elevated"
            );
        }

        // --- Live schtasks round-trip --------------------------------------
        //
        // Creates a task with a unique name, queries it, deletes it. This
        // hits the real Windows Task Scheduler but is hermetic: the task
        // name is per-process and cleaned up in the test body. It validates
        // that our XML is actually accepted by schtasks (catches the bug
        // that motivated this whole rewrite: `/Create /SC ONLOGON` failing
        // with Access Denied non-elevated).

        fn unique_task_name() -> String {
            format!("ThemiaAutostartTest_{}", std::process::id())
        }

        fn task_exists(name: &str) -> bool {
            Command::new("schtasks")
                .args(["/Query", "/TN", name])
                .creation_flags(CREATE_NO_WINDOW)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        }

        fn delete_task(name: &str) {
            let _ = Command::new("schtasks")
                .args(["/Delete", "/TN", name, "/F"])
                .creation_flags(CREATE_NO_WINDOW)
                .status();
        }

        #[test]
        fn schtasks_accepts_our_generated_xml() {
            let name = unique_task_name();
            // Safety net in case a previous run left it behind.
            delete_task(&name);
            assert!(!task_exists(&name), "pre-condition: task must not exist");

            // Generate XML for a fake exe path and write it the same way
            // `enable()` does.
            let user = current_user_id();
            let fake_exe = r"C:\Windows\System32\notepad.exe"; // always exists
            let xml = build_task_xml(&user, fake_exe);

            let mut xml_path = std::env::temp_dir();
            xml_path.push(format!("themia_autostart_test_{}.xml", std::process::id()));
            write_utf16_le_bom(&xml_path, &xml).expect("write xml");

            let created = Command::new("schtasks")
                .args([
                    "/Create",
                    "/TN", &name,
                    "/XML", &xml_path.to_string_lossy(),
                    "/F",
                ])
                .creation_flags(CREATE_NO_WINDOW)
                .status()
                .expect("spawn schtasks")
                .success();
            let _ = std::fs::remove_file(&xml_path);

            // Always clean up, even if assertions below fail.
            let existed = task_exists(&name);
            delete_task(&name);

            assert!(created, "schtasks /Create /XML rejected our XML (non-elevated access-denied?)");
            assert!(existed, "task not found after successful /Create");
            assert!(!task_exists(&name), "task still present after /Delete");
        }
    }
}

// ---------------------------------------------------------------------------
// License commands
// ---------------------------------------------------------------------------

#[tauri::command]
async fn license_activate(key: String) -> Result<widget_license::LicenseStatus, String> {
    let machine_id = widget_license::machine_fingerprint()?;
    widget_license::activate_key(&key, &machine_id).await
}

#[tauri::command]
async fn license_validate() -> Result<widget_license::LicenseStatus, String> {
    Ok(widget_license::validate_on_launch().await)
}

#[tauri::command]
async fn license_deactivate() -> Result<(), String> {
    widget_license::deactivate_current().await
}

#[tauri::command]
fn license_status() -> widget_license::LicenseStatus {
    widget_license::get_status()
}

#[tauri::command]
fn license_get_key() -> Option<String> {
    widget_license::get_raw_key()
}

#[tauri::command]
async fn newsletter_subscribe(email: String) -> Result<(), String> {
    widget_license::subscribe_newsletter(&email).await
}

/// Shared client for `http_get`, honoring the Windows proxy (netproxy SPEC
/// [PX-01]).
///
/// Pooled rather than rebuilt per invoke: RSS and Stocks poll on a timer, so
/// a client per call threw away the connection pool on every tick - and now
/// would also discard reqwest's proxy wiring each time.
fn http_get_client() -> reqwest::Client {
    use std::sync::OnceLock;
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            widget_netproxy::client_builder(std::time::Duration::from_secs(30))
                // One shared identity for every outbound request (netproxy
                // [PX-10]). This used to be a TRUNCATED browser string with no
                // `Chrome/`/`Safari/` token, which reads as a bot to any filter
                // that looks - while the add-a-feed path sent `themia-app`, so
                // the same URL presented as two clients (#81).
                .user_agent(widget_netproxy::user_agent())
                .build()
                .unwrap_or_else(|e| {
                    log::error!("http_get: client build failed ({e}); no proxy support");
                    reqwest::Client::new()
                })
        })
        .clone()
}

/// GET a user-supplied URL as text, through the SSRF guard and the Windows proxy.
///
/// Every failure returns the REAL cause and is logged. The previous version
/// returned only reqwest's transport error and, worse, never looked at the
/// status: a 403 challenge page or a 404 came back as `Ok(html)`, so the caller
/// saw "success" and then failed to parse it, reporting something generic and
/// wrong. That is the #81 diagnosis blocker - a user's feed stopped updating
/// for two days and neither the UI nor the log file contained the status that
/// would have explained it.
///
/// `accept` is optional and per-call rather than baked into the shared client:
/// the RSS widget wants XML content negotiation, while Stocks/Gallery want JSON
/// and web-element wants HTML. One command, caller-chosen negotiation.
#[tauri::command]
async fn http_get(url: String, accept: Option<String>) -> Result<String, String> {
    let u = url.clone();
    tauri::async_runtime::spawn_blocking(move || widget_netguard::check_url(&u))
        .await
        .map_err(|e| {
            log::error!("http_get: guard task failed for {url}: {e}");
            e.to_string()
        })?
        .map_err(|e| {
            log::warn!("http_get: blocked {url}: {e}");
            e
        })?;

    let mut req = http_get_client().get(&url);
    if let Some(a) = accept.as_deref() {
        req = req.header(reqwest::header::ACCEPT, a);
    }

    let resp = req.send().await.map_err(|e| {
        log::warn!("http_get: request to {url} failed: {e}");
        format!("Request failed: {e}")
    })?;

    let status = resp.status();
    if !status.is_success() {
        // The status alone is the whole diagnosis for the common failures
        // (403 bot challenge, 404 moved feed, 429 rate limit, 5xx outage), so
        // it must reach both the log and the caller.
        log::warn!("http_get: {url} returned HTTP {}", status.as_u16());
        return Err(match status.canonical_reason() {
            Some(reason) => format!("HTTP {} {}", status.as_u16(), reason),
            None => format!("HTTP {}", status.as_u16()),
        });
    }

    resp.text().await.map_err(|e| {
        log::warn!("http_get: reading body of {url} failed: {e}");
        format!("Couldn't read response: {e}")
    })
}

#[tauri::command]
fn autostart_is_enabled() -> Result<bool, String> {
    #[cfg(target_os = "windows")]
    { autostart::is_enabled() }
    #[cfg(not(target_os = "windows"))]
    { Ok(false) }
}

/// Why an autostart enable/disable failed, as a CLOSED set the frontend can
/// branch on ([AS-05], #69/#79). The kind is decided here, in the process that
/// knows its own token — never by string-matching a localized `schtasks`
/// message across the IPC boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
enum AutostartErrorKind {
    /// Themia is not elevated: "run as administrator" is the CONFIDENT remedy,
    /// stated plainly. Never reported when already elevated.
    ElevationRequired,
    /// Already elevated, so the cause is something else: group policy, Task
    /// Scheduler service disabled, I/O. The UI still suggests running as
    /// administrator here (a user may have hit this from a differently-elevated
    /// launch), but hedges it and names policy as the likelier cause.
    Other,
}

#[derive(Debug, Clone, serde::Serialize)]
struct AutostartError {
    kind: AutostartErrorKind,
    /// Raw diagnostic (schtasks operation + exit code + stderr). Always
    /// populated, so it can be logged and quoted in a bug report even when the
    /// UI shows the friendlier `kind`-specific text.
    message: String,
}

/// Pure classifier, split from the Win32 probe so the decision is unit-testable
/// (`s79_*` tests) without a token or a Task Scheduler.
fn classify_autostart_failure(message: String, elevated: bool) -> AutostartError {
    let kind = if elevated {
        AutostartErrorKind::Other
    } else {
        AutostartErrorKind::ElevationRequired
    };
    AutostartError { kind, message }
}

#[cfg(target_os = "windows")]
fn classify_current(message: String) -> AutostartError {
    classify_autostart_failure(message, autostart::is_elevated())
}

// #69/#79: these return `Result<(), AutostartError>` (not `bool`, not a bare
// `String`) so a `schtasks` failure propagates BOTH its reason and a machine-
// readable kind to the frontend, which renders the matching remedy inline next
// to the toggle. On Ok the toggle reflects the real state; on Err the caller
// must NOT treat the setting as applied.
/// [AS-06] Can this process configure autostart at all? `Ok(())` = yes; `Err`
/// carries the same classified failure the real toggle would produce, so the UI
/// can render the identical warning before the user ever clicks.
#[tauri::command]
fn autostart_can_configure() -> Result<(), AutostartError> {
    #[cfg(target_os = "windows")]
    { autostart::can_configure().map_err(classify_current) }
    #[cfg(not(target_os = "windows"))]
    {
        Err(AutostartError {
            kind: AutostartErrorKind::Other,
            message: "autostart is only supported on Windows".to_string(),
        })
    }
}

#[tauri::command]
fn autostart_enable() -> Result<(), AutostartError> {
    #[cfg(target_os = "windows")]
    { autostart::enable().map_err(classify_current) }
    #[cfg(not(target_os = "windows"))]
    {
        Err(AutostartError {
            kind: AutostartErrorKind::Other,
            message: "autostart is only supported on Windows".to_string(),
        })
    }
}

#[tauri::command]
fn autostart_disable() -> Result<(), AutostartError> {
    #[cfg(target_os = "windows")]
    { autostart::disable().map_err(classify_current) }
    // Nothing to remove off-Windows; treat as already-disabled.
    #[cfg(not(target_os = "windows"))]
    { Ok(()) }
}

#[tauri::command]
fn push_windows_to_desktop(app: tauri::AppHandle) {
    widget_folder::push_windows_to_desktop(&app);
}

#[tauri::command]
fn open_in_explorer(path: String) -> Result<(), String> {
    widget_folder::open_in_explorer(&path)
}

#[tauri::command]
fn open_file(path: String) -> Result<(), String> {
    widget_folder::open_file(&path)
}

#[tauri::command]
fn resolve_shortcut(path: String) -> Result<widget_folder::ShortcutTarget, String> {
    widget_folder::resolve_shortcut(&path)
}

#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    widget_folder::open_url(&url)
}

/// [EX-01] Set only by `quit_app`, the one deliberate shutdown route. The
/// `RunEvent::ExitRequested` guard reads it to tell a real quit apart from the
/// runtime asking to exit merely because the window count reached zero.
static APP_QUITTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// [EX-01] Should the runtime's exit request be honoured?
///
/// Themia keeps no permanently-open window and has no tray icon, so the
/// overlay set legitimately passes through zero windows during a monitor
/// reconcile ([DW-06] rebuild, disable-all-monitors, hot-unplug, dock, wake).
/// `tauri-runtime-wry` treats an empty window map as "app is done" and exits
/// the process with no panic, no log line and no Event Viewer entry — the
/// user-visible symptom in themia-releases #87 ("exits without a trace"). Only
/// a deliberate `quit_app` may exit.
fn exit_request_should_be_honoured(quitting: bool) -> bool {
    quitting
}

/// Quit the whole application. Surfaced by the Settings "Quit Themia" button so
/// users can shut Themia down without Task Manager, since the widget windows are
/// borderless, skip the taskbar, and have no close button of their own.
/// `exit(0)` runs the normal Tauri teardown, firing `RunEvent::Exit` (which
/// restores desktop-icon visibility) before the process ends.
#[tauri::command]
fn quit_app(app: tauri::AppHandle) {
    request_quit(&app);
}

/// The one deliberate shutdown route ([EX-01]), shared by the Settings button
/// and the tray menu's Quit item. Both are the user asking to exit, so both must
/// arm the guard rather than reimplement it.
fn request_quit(app: &tauri::AppHandle) {
    log::info!(target: "app", "quit requested by the user: shutting down");
    // [EX-01] Arm the guard BEFORE `exit`, so the `ExitRequested` this raises
    // is recognised as deliberate and passes through.
    APP_QUITTING.store(true, std::sync::atomic::Ordering::SeqCst);
    app.exit(0);
}

#[tauri::command]
fn get_system_locale() -> String {
    sys_locale::get_locale().unwrap_or_else(|| "en".into())
}

/// Installed font FAMILY names for the Settings font picker (issue #64). The
/// picker lists these below the four curated presets so a user can pick any
/// typeface installed on their machine. Names only — font files are never read
/// and nothing is fetched (the app CSP blocks remote fonts). Returns a
/// case-insensitively sorted, de-duplicated list; an empty vec on any failure,
/// in which case the picker simply shows the presets.
#[cfg(target_os = "windows")]
#[tauri::command]
fn list_system_fonts() -> Vec<String> {
    list_installed_font_families()
}

#[cfg(not(target_os = "windows"))]
#[tauri::command]
fn list_system_fonts() -> Vec<String> {
    Vec::new()
}

// RAII for the screen DC borrowed for font enumeration: ReleaseDC on every path,
// including an early return or a panic (per the Win32-handle non-negotiable).
#[cfg(target_os = "windows")]
struct FontEnumDc(winapi::shared::windef::HDC);
#[cfg(target_os = "windows")]
impl Drop for FontEnumDc {
    fn drop(&mut self) {
        unsafe { winapi::um::winuser::ReleaseDC(std::ptr::null_mut(), self.0); }
    }
}

// GDI EnumFontFamiliesExW callback. Collects each family name into the BTreeSet
// passed via lParam. It is an FFI boundary, so a panic unwinding into GDI is UB:
// catch it and keep enumerating (mirrors kb_hook_proc).
#[cfg(target_os = "windows")]
unsafe extern "system" fn font_enum_proc(
    lpelf: *const winapi::um::wingdi::LOGFONTW,
    _lpntm: *const winapi::um::wingdi::TEXTMETRICW,
    _font_type: winapi::shared::minwindef::DWORD,
    lparam: winapi::shared::minwindef::LPARAM,
) -> i32 {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if lpelf.is_null() || lparam == 0 {
            return;
        }
        let lf = &*lpelf;
        let face = &lf.lfFaceName;
        let end = face.iter().position(|&c| c == 0).unwrap_or(face.len());
        let name = String::from_utf16_lossy(&face[..end]);
        // Skip GDI's '@' vertical-writing faces (e.g. "@MS Gothic"): rotated
        // variants, not distinct families a user would pick.
        if name.is_empty() || name.starts_with('@') {
            return;
        }
        let set = &mut *(lparam as *mut std::collections::BTreeSet<String>);
        set.insert(name);
    }));
    1 // non-zero → continue enumeration
}

#[cfg(target_os = "windows")]
fn list_installed_font_families() -> Vec<String> {
    use std::collections::BTreeSet;
    use winapi::um::wingdi::{EnumFontFamiliesExW, DEFAULT_CHARSET, LOGFONTW};
    use winapi::um::winuser::GetDC;

    let mut set: BTreeSet<String> = BTreeSet::new();
    unsafe {
        let hdc = GetDC(std::ptr::null_mut());
        if hdc.is_null() {
            log::warn!(target: "app", "list_system_fonts: GetDC(null) returned null");
            return Vec::new();
        }
        let _dc = FontEnumDc(hdc); // released on every path below
        let mut lf: LOGFONTW = std::mem::zeroed();
        lf.lfCharSet = DEFAULT_CHARSET as u8;
        // lfFaceName left zeroed → enumerate one representative entry per family.
        EnumFontFamiliesExW(
            hdc,
            &mut lf,
            Some(font_enum_proc),
            &mut set as *mut BTreeSet<String> as winapi::shared::minwindef::LPARAM,
            0,
        );
    }
    let mut out: Vec<String> = set.into_iter().collect();
    out.sort_by_key(|a| a.to_lowercase());
    out
}

#[tauri::command]
async fn get_file_icon(path: String, size: u32) -> Result<String, String> {
    tokio::task::spawn_blocking(move || widget_folder::get_file_icon(&path, size))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn get_file_icons_batch(
    paths: Vec<String>,
    size: u32,
) -> std::collections::HashMap<String, String> {
    tokio::task::spawn_blocking(move || widget_folder::get_file_icons_batch(paths, size))
        .await
        .unwrap_or_default()
}

#[tauri::command]
fn email_list_accounts(app: tauri::AppHandle) -> Vec<widget_email::AccountInfo> {
    widget_email::list_accounts(&app)
}

#[tauri::command]
async fn email_start_device_auth() -> Result<widget_email::DeviceAuthStart, String> {
    widget_email::start_device_auth().await
}

#[tauri::command]
async fn email_poll_auth(
    app: tauri::AppHandle,
    device_code: String,
) -> Result<Option<widget_email::AccountInfo>, String> {
    widget_email::poll_device_auth(&app, &device_code).await
}

/// Begin GitHub sign-in via OAuth device flow. [S-51]
#[tauri::command]
async fn email_start_github_device_auth() -> Result<widget_email::DeviceAuthStart, String> {
    widget_email::start_github_device_auth().await
}

/// One poll of the GitHub device flow. `None` = the user hasn't approved yet.
#[tauri::command]
async fn email_poll_github_device_auth(
    app: tauri::AppHandle,
    device_code: String,
) -> Result<Option<widget_email::AccountInfo>, String> {
    widget_email::poll_github_device_auth(&app, &device_code).await
}

#[tauri::command]
async fn email_start_google_auth() -> Result<widget_email::GoogleAuthStart, String> {
    widget_email::start_google_auth().await
}

#[tauri::command]
async fn email_complete_google_auth(
    app: tauri::AppHandle,
    state: String,
) -> Result<widget_email::AccountInfo, String> {
    widget_email::complete_google_auth(&app, &state).await
}

#[tauri::command]
async fn email_get_messages(
    app: tauri::AppHandle,
    account_id: String,
    folder: String,
    count: u32,
) -> Result<Vec<widget_email::EmailMessage>, String> {
    widget_email::get_messages(&app, &account_id, &folder, count).await
}

#[tauri::command]
fn email_sign_out_account(app: tauri::AppHandle, account_id: String) {
    widget_email::sign_out_account(&app, &account_id);
}

#[tauri::command]
fn email_rename_account(app: tauri::AppHandle, account_id: String, nickname: String) {
    widget_email::rename_account(&app, &account_id, nickname);
}

#[tauri::command]
async fn email_get_folder_status(
    app: tauri::AppHandle,
    account_id: String,
    folder: String,
) -> Result<widget_email::FolderStatus, String> {
    widget_email::get_folder_status(&app, &account_id, &folder).await
}

#[tauri::command]
async fn email_mark_message_read(
    app: tauri::AppHandle,
    message_id: String,
    is_read: bool,
) -> Result<(), String> {
    widget_email::mark_message_read(&app, &message_id, is_read).await
}

#[tauri::command]
async fn email_delete_message(
    app: tauri::AppHandle,
    message_id: String,
) -> Result<(), String> {
    widget_email::delete_message(&app, &message_id).await
}

#[tauri::command]
async fn email_move_message(
    app: tauri::AppHandle,
    message_id: String,
    destination_folder: String,
) -> Result<(), String> {
    widget_email::move_message(&app, &message_id, &destination_folder).await
}

#[tauri::command]
fn email_open_url(url: String) -> Result<(), String> {
    widget_email::open_url(&url)
}

#[tauri::command]
fn email_open_message(web_link: String, message_id: String) -> Result<(), String> {
    widget_email::open_message(&web_link, &message_id)
}

#[tauri::command]
fn calendar_open_event(web_link: String, event_id: String) -> Result<(), String> {
    widget_email::open_calendar_event(&web_link, &event_id)
}

#[tauri::command]
fn email_reply_to_message(message_id: String, from_email: String, subject: String) -> Result<(), String> {
    widget_email::reply_to_message(&message_id, &from_email, &subject)
}

/// Reachability check for a connect preset's endpoint, before any credentials
/// are typed. Anonymous (IMAP greeting / unauthenticated PROPFIND) — see
/// `widget_email::probe_endpoint`. [S-46]
#[tauri::command]
async fn email_probe_endpoint(
    target: widget_email::ProbeTarget,
) -> Result<widget_email::ProbeResult, String> {
    // A probe dials a host the frontend supplied, exactly like the add commands
    // below, so it goes through the same SSRF guard: a preset must never become
    // a way to reach loopback or private address space.
    match &target {
        widget_email::ProbeTarget::Imap { host, port } => {
            let (h, p) = (host.clone(), *port);
            tauri::async_runtime::spawn_blocking(move || widget_netguard::check_host_port(&h, p))
                .await
                .map_err(|e| e.to_string())??;
        }
        widget_email::ProbeTarget::Dav { url } => {
            let u = url.clone();
            tauri::async_runtime::spawn_blocking(move || widget_netguard::check_url(&u))
                .await
                .map_err(|e| e.to_string())??;
        }
    }
    Ok(widget_email::probe_endpoint(target).await)
}

#[tauri::command]
async fn email_add_imap_account(
    app: tauri::AppHandle,
    host: String,
    port: u16,
    username: String,
    password: String,
    display_name: String,
    nickname: Option<String>,
) -> Result<widget_email::AccountInfo, String> {
    let (h, p) = (host.clone(), port);
    tauri::async_runtime::spawn_blocking(move || widget_netguard::check_host_port(&h, p))
        .await
        .map_err(|e| e.to_string())??;
    widget_email::add_imap_account(&app, host, port, username, password, display_name, nickname).await
}

#[tauri::command]
async fn email_add_caldav_account(
    app: tauri::AppHandle,
    url: String,
    username: String,
    password: String,
    display_name: String,
    nickname: Option<String>,
) -> Result<widget_email::AccountInfo, String> {
    let u = url.clone();
    tauri::async_runtime::spawn_blocking(move || widget_netguard::check_url(&u))
        .await
        .map_err(|e| e.to_string())??;
    widget_email::add_caldav_account(&app, url, username, password, display_name, nickname).await
}

#[tauri::command]
async fn email_add_carddav_account(
    app: tauri::AppHandle,
    url: String,
    username: String,
    password: String,
    display_name: String,
    nickname: Option<String>,
) -> Result<widget_email::AccountInfo, String> {
    let u = url.clone();
    tauri::async_runtime::spawn_blocking(move || widget_netguard::check_url(&u))
        .await
        .map_err(|e| e.to_string())??;
    widget_email::add_carddav_account(&app, url, username, password, display_name, nickname).await
}

#[tauri::command]
async fn email_add_github_account(
    app: tauri::AppHandle,
    token: String,
    api_base_url: Option<String>,
    nickname: Option<String>,
) -> Result<widget_email::AccountInfo, String> {
    if let Some(base) = api_base_url.clone() {
        tauri::async_runtime::spawn_blocking(move || widget_netguard::check_url(&base))
            .await
            .map_err(|e| e.to_string())??;
    }
    widget_email::add_github_account(&app, token, api_base_url, nickname).await
}

#[tauri::command]
async fn email_add_rss_feed(
    app: tauri::AppHandle,
    url: String,
    nickname: Option<String>,
) -> Result<widget_email::AccountInfo, String> {
    let u = url.clone();
    tauri::async_runtime::spawn_blocking(move || widget_netguard::check_url(&u))
        .await
        .map_err(|e| e.to_string())??;
    widget_email::add_rss_feed(&app, url, nickname).await
}

#[tauri::command]
fn email_get_github_token(app: tauri::AppHandle, account_id: String) -> Option<String> {
    widget_email::get_github_token(&app, &account_id)
}

#[tauri::command]
fn email_get_github_api_base_url(app: tauri::AppHandle, account_id: String) -> Option<String> {
    widget_email::get_github_api_base_url(&app, &account_id)
}

#[tauri::command]
async fn email_start_claude_auth() -> Result<widget_email::ClaudeAuthStart, String> {
    widget_email::start_claude_auth()
}

#[tauri::command]
async fn email_complete_claude_auth(
    app: tauri::AppHandle,
    code: String,
    state: String,
    nickname: Option<String>,
) -> Result<widget_email::AccountInfo, String> {
    widget_email::complete_claude_auth(&app, code, state, nickname).await
}

#[tauri::command]
async fn email_get_claude_usage(
    app: tauri::AppHandle,
    account_id: String,
) -> Result<widget_email::ClaudeUsage, String> {
    widget_email::fetch_claude_usage(&app, &account_id).await
}

#[tauri::command]
fn fs_paste(paths: Vec<String>, dest_dir: String, cut: bool) -> Result<(), String> {
    widget_folder::fs_paste(&paths, &dest_dir, cut)
}

#[tauri::command]
fn get_clipboard_url() -> Option<String> {
    widget_folder::get_clipboard_url()
}

#[tauri::command]
fn clipboard_set_files(window: tauri::Window, paths: Vec<String>, cut: bool) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        let hwnd = window.hwnd().map_err(|e| format!("hwnd: {e}"))?.0 as usize;
        widget_folder::clipboard_set_files(hwnd, paths, cut)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = window;
        widget_folder::clipboard_set_files(0, paths, cut)
    }
}

#[tauri::command]
fn clipboard_get_files() -> widget_folder::ClipboardFiles {
    widget_folder::clipboard_get_files()
}

/// Track which widget window should receive keyboard shortcut events.
/// The keyboard hook (installed in start_display_watcher) reads this.
#[cfg(target_os = "windows")]
static WIDGET_KB_ACTIVE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(target_os = "windows")]
static WIDGET_KB_HWND: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(target_os = "windows")]
static WIDGET_KB_LABEL: std::sync::OnceLock<std::sync::Mutex<String>> =
    std::sync::OnceLock::new();

#[cfg(target_os = "windows")]
fn widget_kb_label() -> &'static std::sync::Mutex<String> {
    WIDGET_KB_LABEL.get_or_init(|| std::sync::Mutex::new(String::new()))
}

/// Called from JS on mousedown inside any folder widget.
/// Marks this window as the target for intercepted keyboard shortcuts.
#[tauri::command]
fn activate_widget_keyboard(app: tauri::AppHandle, label: String) {
    #[cfg(target_os = "windows")]
    {
        use std::sync::atomic::Ordering;

        let hwnd_val = app
            .get_webview_window(&label)
            .and_then(|w| w.hwnd().ok())
            .map(|h| h.0 as usize)
            .unwrap_or(0);
        WIDGET_KB_HWND.store(hwnd_val, Ordering::Relaxed);

        *widget_kb_label().lock().unwrap_or_else(|e| e.into_inner()) = label;
        WIDGET_KB_ACTIVE.store(true, Ordering::Relaxed);
    }
}

/// Called from JS on mouseleave from a folder widget.
/// Clears keyboard interception so keystrokes reach other apps.
#[tauri::command]
fn deactivate_widget_keyboard() {
    #[cfg(target_os = "windows")]
    {
        WIDGET_KB_ACTIVE.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

/// True while any overlay window has a focused text input (<input>/<textarea>),
/// set from JS via `set_text_input_focused`. While typing, the overlay holds
/// real Win32 focus and native key delivery works, so the WH_KEYBOARD_LL hook
/// must stand down or it eats Backspace/Enter/arrows before they reach the
/// input, including the Backspaces IME tools (Unikey/EVKey Vietnamese Telex)
/// inject via SendInput to replace composed characters (issue #35).
#[cfg(target_os = "windows")]
static TEXT_INPUT_FOCUSED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Per-window-label focus claims backing TEXT_INPUT_FOCUSED, so two overlay
/// windows (multi-monitor) can't clobber each other's state.
#[cfg(target_os = "windows")]
static TEXT_INPUT_LABELS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::OnceLock::new();

#[cfg(target_os = "windows")]
fn text_input_labels() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    TEXT_INPUT_LABELS.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// Pure aggregate update for the text-input focus claims: insert/remove the
/// window's label, return whether ANY window still has a focused text input.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn update_text_input_focus(
    set: &mut std::collections::HashSet<String>,
    label: String,
    focused: bool,
) -> bool {
    if focused {
        set.insert(label);
    } else {
        set.remove(&label);
    }
    !set.is_empty()
}

/// True while any overlay window has an active handler on its DOM Escape stack
/// (edit mode, widget placing, or an open context menu), set from JS via
/// `set_escape_wanted`. When set, the WH_KEYBOARD_LL Escape intercept must stand
/// down so the keystroke reaches the focused webview and the escape stack can
/// close the menu / exit the mode; otherwise the hook consumes Escape (only
/// broadcasting folder-clear-selection) and those DOM handlers never fire.
#[cfg(target_os = "windows")]
static ESCAPE_WANTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Per-window-label claims backing ESCAPE_WANTED, so two overlay windows
/// (multi-monitor) can't clobber each other's state. Mirrors TEXT_INPUT_LABELS.
#[cfg(target_os = "windows")]
static ESCAPE_WANTED_LABELS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::OnceLock::new();

#[cfg(target_os = "windows")]
fn escape_wanted_labels() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    ESCAPE_WANTED_LABELS.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// Pure aggregate update for the escape-wanted claims: insert/remove the
/// window's label, return whether ANY window still wants Escape.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn update_escape_wanted(
    set: &mut std::collections::HashSet<String>,
    label: String,
    wanted: bool,
) -> bool {
    if wanted {
        set.insert(label);
    } else {
        set.remove(&label);
    }
    !set.is_empty()
}

/// Pure gate for the hook's desktop Escape intercept: consume Escape (to
/// broadcast folder-clear-selection) ONLY when no text input is focused AND no
/// window's DOM escape stack wants the key. Either claim stands the intercept
/// down so the focused webview handles Escape natively (escape stack / inline
/// input), which is what closes a context menu or exits edit/placing mode.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn should_consume_desktop_escape(text_input_focused: bool, escape_wanted: bool) -> bool {
    !text_input_focused && !escape_wanted
}

/// Pure gate for the hook's folder-level intercept (issue #35): requires an
/// active widget-keyboard route AND no focused text input. While a text input
/// is focused the keystroke must fall through to native delivery, since the
/// focused webview handles it (and IME-injected backspaces survive).
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn should_intercept_widget_keys(kb_active: bool, text_input_focused: bool) -> bool {
    kb_active && !text_input_focused
}

/// Pure classifier for the folder-level intercept whitelist (issue #35): maps
/// a `(ctrl, virtual-key)` pair to the `folder-key` action the hook emits, or
/// `None` for keys the hook must never claim. Extracted from the inline match
/// in `kb_hook_proc_impl` so the whitelist has a single, unit-testable source
/// of truth (no parallel copy to drift). CRITICAL: Backspace (0x08) and Space
/// (0x20), both no-Ctrl, are in this set, so the hook *would* eat them: the
/// Backspaces a Vietnamese Telex IME (Unikey/EVKey) injects to replace composed
/// characters, and the Space a user types into a notes/todo input (themia
/// #53), unless `should_intercept_widget_keys` stands the hook down because a
/// text input is focused. Ctrl+letters are clipboard/select-all; bare keys are
/// nav/edit; everything else (printable typing, Ctrl+Backspace) returns None.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn widget_kb_action(ctrl: bool, vk_code: u32) -> Option<&'static str> {
    match (ctrl, vk_code) {
        (true, 0x43) => Some("c"),         // Ctrl+C
        (true, 0x56) => Some("v"),         // Ctrl+V
        (true, 0x58) => Some("x"),         // Ctrl+X
        (true, 0x41) => Some("a"),         // Ctrl+A
        (false, 0x2E) => Some("delete"),   // Delete
        (false, 0x71) => Some("f2"),       // F2
        // Tab steps the keyboard focus through a widget's rows (To Do [S-56],
        // habit tracker [S-17]). Safe to claim: the hook only reaches here when
        // a widget owns keyboard AND the cursor is over our desktop surface, and
        // it stands down entirely while a text input is focused -- so Tab inside
        // an inline editor is a native DOM keydown, never this.
        (false, 0x09) => Some("tab"),      // Tab
        (false, 0x26) => Some("up"),       // ArrowUp
        (false, 0x28) => Some("down"),     // ArrowDown
        (false, 0x25) => Some("left"),     // ArrowLeft
        (false, 0x27) => Some("right"),    // ArrowRight
        (false, 0x0D) => Some("enter"),    // Enter
        (false, 0x08) => Some("backspace"),// Backspace
        (false, 0x20) => Some("space"),    // Space (slideshow pause, gallery [S-29])
        (false, 0x24) => Some("home"),     // Home
        (false, 0x23) => Some("end"),      // End
        (false, 0x21) => Some("pageup"),   // PageUp
        (false, 0x22) => Some("pagedown"), // PageDown
        _ => None,
    }
}

/// Pure gate for the hook's app-level shortcut block (Ctrl+E/W/Z/1-9), todo
/// SPEC [S-38]: while a text input is focused, Ctrl+Z must fall through to
/// the input's native text undo (not the file-op undo broadcast) and
/// Ctrl+1-9 must not switch views, since a view switch unmounts the widget tree
/// and discards the in-progress edit. Ctrl+E/W are the exception and stay
/// consumed: falling through would hand them to WebView2's browser
/// accelerators (Ctrl+W = close window). Consuming them is safe: the JS
/// hotkey dispatcher (hotkeys.ts `inInput` guard) already ignores them in
/// the typing window.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn should_intercept_app_key(action: &str, text_input_focused: bool) -> bool {
    if !text_input_focused {
        return true;
    }
    matches!(action, "e" | "w")
}

/// Called from JS whenever a text input gains/loses focus in an overlay
/// window (deduped boolean per window, mirrors `set_interactive`).
#[tauri::command]
fn set_text_input_focused(label: String, focused: bool) {
    #[cfg(target_os = "windows")]
    {
        use std::sync::atomic::Ordering;
        let mut set = text_input_labels().lock().unwrap_or_else(|e| e.into_inner());
        let any = update_text_input_focus(&mut set, label, focused);
        TEXT_INPUT_FOCUSED.store(any, Ordering::Relaxed);
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (label, focused);
    }
}

/// Mirror of `set_text_input_focused` for the DOM Escape stack: JS calls this
/// when its escape-stack depth crosses 0 (edit mode / placing / a context menu
/// opening or closing) so the hook knows whether to let Escape reach the webview
/// instead of consuming it for folder-clear-selection.
#[tauri::command]
fn set_escape_wanted(label: String, wanted: bool) {
    #[cfg(target_os = "windows")]
    {
        use std::sync::atomic::Ordering;
        let mut set = escape_wanted_labels().lock().unwrap_or_else(|e| e.into_inner());
        let any = update_escape_wanted(&mut set, label, wanted);
        ESCAPE_WANTED.store(any, Ordering::Relaxed);
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (label, wanted);
    }
}

/// True if the Windows Start Menu (or Search flyout) is currently foreground.
/// Both live in Windows.UI.Core.CoreWindow hosts, so we narrow by process name
/// so unrelated UWP apps (Settings, Calculator) don't trip it.
#[cfg(target_os = "windows")]
unsafe fn start_is_foreground() -> bool {
    use winapi::shared::windef::HWND;
    use winapi::um::winuser::{
        GetClassNameW, GetForegroundWindow, GetWindowThreadProcessId,
    };

    let fg: HWND = GetForegroundWindow();
    if fg.is_null() {
        return false;
    }

    let mut class_buf = [0u16; 64];
    let n = GetClassNameW(fg, class_buf.as_mut_ptr(), class_buf.len() as i32);
    if n <= 0 {
        return false;
    }
    let class = String::from_utf16_lossy(&class_buf[..n as usize]);
    if class != "Windows.UI.Core.CoreWindow" {
        return false;
    }

    let mut pid: u32 = 0;
    GetWindowThreadProcessId(fg, &mut pid);
    if pid == 0 {
        return false;
    }

    matches!(
        process_basename(pid).as_deref(),
        Some("startmenuexperiencehost.exe" | "searchhost.exe" | "searchapp.exe"),
    )
}

/// Synthesise a single VK_ESCAPE key press via SendInput. Routes to the
/// current foreground window, used to dismiss Start/Search since
/// SetForegroundWindow(Progman) silently fails from our WS_EX_NOACTIVATE
/// process (no foreground privileges).
#[cfg(target_os = "windows")]
unsafe fn inject_escape() {
    use winapi::um::winuser::{
        SendInput, INPUT, INPUT_KEYBOARD, KEYEVENTF_KEYUP, VK_ESCAPE,
    };
    fn key(vk: u16, up: bool) -> INPUT {
        let mut input: INPUT = unsafe { std::mem::zeroed() };
        input.type_ = INPUT_KEYBOARD;
        unsafe {
            let ki = input.u.ki_mut();
            ki.wVk = vk;
            ki.dwFlags = if up { KEYEVENTF_KEYUP } else { 0 };
        }
        input
    }
    let mut inputs = [key(VK_ESCAPE as u16, false), key(VK_ESCAPE as u16, true)];
    SendInput(
        inputs.len() as u32,
        inputs.as_mut_ptr(),
        std::mem::size_of::<INPUT>() as i32,
    );
}

// ---------------------------------------------------------------------------
// [HK-01] Lock-free overlay-HWND snapshot for the low-level input hooks
// ---------------------------------------------------------------------------
//
// A WH_MOUSE_LL / WH_KEYBOARD_LL callback runs on the hook thread for EVERY
// input event in the whole session, and Windows silently and PERMANENTLY
// removes any hook whose callback overruns `LowLevelHooksTimeout` (300 ms by
// default). So the hot path must never take a lock another thread can hold for
// an unbounded time — which is exactly what reading Tauri's window map did: it
// clones the manager's window collection under the manager lock, a lock the
// main thread holds across window creation and teardown, i.e. across precisely
// the display-change storms where input keeps flowing. Losing the hook there is
// invisible and unrecoverable: clicks stop dismissing Start, `themia-blur`
// never fires again and widget selection sticks until the app is restarted
// (themia-releases #58).
//
// Instead the overlay HWNDs are mirrored into a fixed array of atomics that the
// hook reads without any lock at all. Overlay windows are one per monitor, so
// the cap is generous.

#[cfg(target_os = "windows")]
const OVERLAY_SNAPSHOT_CAP: usize = 32;

#[cfg(target_os = "windows")]
static OVERLAY_HWNDS: [std::sync::atomic::AtomicUsize; OVERLAY_SNAPSHOT_CAP] =
    [const { std::sync::atomic::AtomicUsize::new(0) }; OVERLAY_SNAPSHOT_CAP];

/// Number of live entries at the front of [`OVERLAY_HWNDS`]. Published last
/// (Release) so a reader that sees a length also sees the handles behind it.
#[cfg(target_os = "windows")]
static OVERLAY_HWNDS_LEN: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// [HK-01] Mirror the current `wndproc_map` keys into the lock-free snapshot.
///
/// Called from the normal (non-hook) threads that mutate the map: the subclass
/// install, the `Destroyed` window event and the WM_NCDESTROY teardown. Taking
/// the map lock HERE is fine; it is only the hook side that must stay lock-free.
#[cfg(target_os = "windows")]
fn publish_overlay_snapshot() {
    use std::sync::atomic::Ordering;

    let hwnds: Vec<usize> = match wndproc_map().lock() {
        Ok(map) => map.keys().copied().collect(),
        Err(poisoned) => poisoned.into_inner().keys().copied().collect(),
    };

    let n = hwnds.len().min(OVERLAY_SNAPSHOT_CAP);
    if hwnds.len() > OVERLAY_SNAPSHOT_CAP {
        log::warn!(
            target: "displays",
            "[HK-01] {} overlay windows exceeds the {OVERLAY_SNAPSHOT_CAP}-slot hook snapshot; \
             clicks on the surplus windows will read as outside-the-app",
            hwnds.len()
        );
    }

    // Shrink first, so a reader can never see a stale tail beyond the new
    // length; then fill, then publish the length.
    OVERLAY_HWNDS_LEN.store(0, Ordering::Release);
    for (slot, hwnd) in OVERLAY_HWNDS.iter().zip(hwnds.iter()).take(n) {
        slot.store(*hwnd, Ordering::Relaxed);
    }
    OVERLAY_HWNDS_LEN.store(n, Ordering::Release);
}

/// [HK-01] Snapshot membership test. No locks, no allocation, no Tauri.
#[cfg(target_os = "windows")]
fn overlay_snapshot_contains(hwnd: usize) -> bool {
    use std::sync::atomic::Ordering;

    if hwnd == 0 {
        return false;
    }
    let n = OVERLAY_HWNDS_LEN.load(Ordering::Acquire).min(OVERLAY_SNAPSHOT_CAP);
    OVERLAY_HWNDS
        .iter()
        .take(n)
        .any(|slot| slot.load(Ordering::Relaxed) == hwnd)
}

/// True if `pt` (screen coords) lands on one of our widget windows.
///
/// [HK-01] Runs inside WH_MOUSE_LL: three cheap Win32 calls plus a lock-free
/// array scan, and nothing else.
#[cfg(target_os = "windows")]
unsafe fn point_over_widget_window(pt: winapi::shared::windef::POINT) -> bool {
    use winapi::um::winuser::{GetAncestor, WindowFromPoint, GA_ROOT};

    let hwnd_at = WindowFromPoint(pt);
    if hwnd_at.is_null() {
        return false;
    }
    let root = GetAncestor(hwnd_at, GA_ROOT);
    let root = if root.is_null() { hwnd_at } else { root };

    overlay_snapshot_contains(root as usize)
}

/// [HK-01] Pending `themia-blur` broadcasts requested from the input hook.
/// A counter rather than a queue: the event carries no payload, so N clicks
/// away from the app coalesce into one broadcast with the same meaning.
#[cfg(target_os = "windows")]
static BLUR_REQUESTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// [HK-01] Unparked whenever a hook callback has work for the emitter.
/// Parking rather than polling keeps this off the "no new wall-clock timing"
/// list.
#[cfg(target_os = "windows")]
static HOOK_EMITTER: std::sync::OnceLock<std::thread::Thread> = std::sync::OnceLock::new();

/// [HK-01] A keyboard event the hook wants delivered to the webviews. The
/// keystroke itself is decided (and consumed or passed on) inside the callback;
/// only the delivery — which needs Tauri's manager lock — is handed over.
#[cfg(target_os = "windows")]
enum HookEmit {
    /// `app-key` to every window. The actions behind it write to synced
    /// localStorage and are idempotent, so N windows acting is one action.
    AppKeyBroadcast(String),
    /// `app-key` to exactly one window. Ctrl+Z is not idempotent, so it goes to
    /// the window under the cursor (the toast belongs on the monitor the user
    /// is looking at), falling back to the lexicographically-first label.
    AppKeyOneWindow(String),
    /// `folder-clear-selection` to every window.
    ClearSelection,
    /// `folder-key` to the widget that currently owns keyboard routing.
    FolderKey { label: String, payload: String },
}

/// [HK-01] Bound on the hand-off queue. A wedged main thread cannot be fixed by
/// growing this; the cap just stops one from turning into unbounded memory.
#[cfg(target_os = "windows")]
const HOOK_EMIT_CAP: usize = 64;

#[cfg(target_os = "windows")]
static HOOK_EMITS: std::sync::OnceLock<std::sync::Mutex<Vec<HookEmit>>> =
    std::sync::OnceLock::new();

#[cfg(target_os = "windows")]
fn hook_emits() -> &'static std::sync::Mutex<Vec<HookEmit>> {
    HOOK_EMITS.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// [HK-01] Hook-side half: bump a counter and unpark. Both are wait-free, so
/// the callback's time is bounded no matter what the rest of the app is doing.
#[cfg(target_os = "windows")]
fn request_blur_broadcast() {
    use std::sync::atomic::Ordering;
    BLUR_REQUESTS.fetch_add(1, Ordering::Release);
    if let Some(thread) = HOOK_EMITTER.get() {
        thread.unpark();
    }
}

/// [HK-01] Hook-side half for keyboard events. The only lock taken is the queue
/// itself, which is never held across an emit by either side, so the callback's
/// time stays bounded — unlike `Emitter::emit` / `Manager::get_webview_window`,
/// which take the manager lock the main thread holds across window creation and
/// teardown.
#[cfg(target_os = "windows")]
fn request_hook_emit(emit: HookEmit) {
    {
        let mut queue = hook_emits().lock().unwrap_or_else(|e| e.into_inner());
        if queue.len() >= HOOK_EMIT_CAP {
            log::warn!(
                target: "displays",
                "[HK-01] hook emit queue full ({HOOK_EMIT_CAP}); dropping a keyboard event"
            );
            return;
        }
        queue.push(emit);
    }
    if let Some(thread) = HOOK_EMITTER.get() {
        thread.unpark();
    }
}

/// [HK-01] Worker half: does the actual `Emitter::emit`, off the hook thread.
/// One worker for both hooks — the mouse hook's blur broadcast and the keyboard
/// hook's key events are the same job (take the manager lock somewhere that is
/// allowed to block) and must not become two parallel implementations of it.
#[cfg(target_os = "windows")]
fn start_hook_emitter() {
    use std::sync::atomic::Ordering;

    let handle = match std::thread::Builder::new()
        .name("themia-hook-emitter".into())
        .spawn(|| loop {
            std::thread::park();

            // Coalesce: drain everything queued while we were emitting.
            let blurs = BLUR_REQUESTS.swap(0, Ordering::Acquire);
            let keys = std::mem::take(
                &mut *hook_emits().lock().unwrap_or_else(|e| e.into_inner()),
            );
            if blurs == 0 && keys.is_empty() {
                continue; // spurious wakeup
            }

            let Some(app) = WATCHER_APP.get() else { continue };
            use tauri::{Emitter, Manager};

            if blurs > 0 {
                if let Err(e) = app.emit("themia-blur", ()) {
                    log::warn!(target: "displays", "[HK-01] themia-blur emit failed: {e}");
                }
            }

            for key in keys {
                match key {
                    HookEmit::AppKeyBroadcast(action) => {
                        if let Err(e) = app.emit("app-key", &action) {
                            log::error!(target: "app-key", "emit failed: {e:?}");
                        }
                    }
                    HookEmit::AppKeyOneWindow(action) => {
                        // SAFETY: GetCursorPos + window rect reads; no callback
                        // re-entrancy, and we are not on the hook thread.
                        let target = unsafe { widget_window_label_at_cursor() }.or_else(|| {
                            let mut labels: Vec<String> =
                                app.webview_windows().keys().cloned().collect();
                            labels.sort();
                            labels.into_iter().next()
                        });
                        if let Some(label) = target {
                            // IMPORTANT: `win.emit(...)` on a WebviewWindow is
                            // the same trait method as `app.emit(...)` and
                            // broadcasts to every webview. Reaching a single
                            // window needs `emit_to(label, ...)`.
                            if let Err(e) = app.emit_to(label.as_str(), "app-key", &action) {
                                log::error!(target: "app-key", "emit_to failed: {e:?}");
                            }
                        }
                    }
                    HookEmit::ClearSelection => {
                        if let Err(e) = app.emit("folder-clear-selection", ()) {
                            log::error!(target: "folder-clear-selection", "emit failed: {e:?}");
                        }
                    }
                    HookEmit::FolderKey { label, payload } => {
                        match app.get_webview_window(&label) {
                            Some(win) => {
                                if let Err(e) = win.emit("folder-key", &payload) {
                                    log::error!(target: "folder-key", "emit failed: {e:?}");
                                }
                            }
                            // The window went away between the keystroke and
                            // this emit. The hook already decided to consume on
                            // the strength of the snapshot; nothing to do but
                            // say so.
                            None => log::warn!(
                                target: "folder-key",
                                "route target {label} vanished before delivery"
                            ),
                        }
                    }
                }
            }
        }) {
        Ok(h) => h,
        Err(e) => {
            log::error!(target: "displays", "[HK-01] hook emitter thread failed to start: {e}");
            return;
        }
    };
    // ignore: OnceLock::set only fails if the emitter is already running, which
    // is the correct end state either way.
    let _ = HOOK_EMITTER.set(handle.thread().clone());
}

// ---------------------------------------------------------------------------
// [HK-02] Input-hook liveness
// ---------------------------------------------------------------------------
//
// Windows removes a low-level hook whose callback overruns
// `LowLevelHooksTimeout` without telling the owner: `SetWindowsHookEx` returned
// a handle, no API reports that it is now dead, and the callback simply stops
// being called. The [HK-01] work above removes the known way to overrun, but a
// machine under heavy load, a slow WMI/COM call on the same thread or a driver
// stall can still get us evicted, and the app has no way back short of a
// restart. So keep the handles and re-arm on evidence of death.

/// Monotonic counter bumped by every hook callback. Compared across health
/// checks; if it hasn't moved while the OS says input HAS happened, our hooks
/// are no longer being called.
#[cfg(target_os = "windows")]
static HOOK_HEARTBEAT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(target_os = "windows")]
static KB_HOOK_HANDLE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(target_os = "windows")]
static MOUSE_HOOK_HANDLE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(target_os = "windows")]
fn note_hook_alive() {
    HOOK_HEARTBEAT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Milliseconds since the last system-wide input event, straight from the OS.
/// Independent of our hooks, which is what makes it usable as the control in
/// the liveness comparison.
#[cfg(target_os = "windows")]
fn ms_since_last_input() -> Option<u64> {
    use winapi::um::sysinfoapi::GetTickCount;
    use winapi::um::winuser::{GetLastInputInfo, LASTINPUTINFO};

    unsafe {
        let mut lii = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        if GetLastInputInfo(&mut lii) == 0 {
            return None;
        }
        Some(GetTickCount().wrapping_sub(lii.dwTime) as u64)
    }
}

/// [HK-02] Pure decision: are the hooks dead?
///
/// Dead means the OS saw input during the interval we just watched, yet not one
/// of our callbacks ran. `installed` guards the startup window where the hooks
/// were never armed in the first place.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn hooks_look_dead(
    installed: bool,
    heartbeat_moved: bool,
    ms_since_input: Option<u64>,
    check_interval_ms: u64,
) -> bool {
    if !installed || heartbeat_moved {
        return false;
    }
    // No reading = no evidence either way; never re-arm on a guess.
    let Some(idle) = ms_since_input else { return false };
    // The input must have landed inside the interval we were watching,
    // otherwise the user was simply away from the machine.
    idle < check_interval_ms
}

/// [HK-03] Thread id of the dedicated input-hook thread, published once its
/// message queue exists. Zero until then.
#[cfg(target_os = "windows")]
static HOOK_THREAD_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// [HK-03] Private message asking the hook thread to reinstall both hooks.
#[cfg(target_os = "windows")]
const WM_REARM_INPUT_HOOKS: winapi::shared::minwindef::UINT = winapi::um::winuser::WM_APP + 1;

/// [HK-02] Install both low-level input hooks, recording the handles.
/// Idempotent per handle slot: an existing handle is unhooked before it is
/// replaced, so a re-arm can never leak a hook.
///
/// [HK-03] Runs on the hook thread and nowhere else. Windows delivers a
/// WH_KEYBOARD_LL / WH_MOUSE_LL callback through the message queue of the
/// thread that called `SetWindowsHookEx`, so a hook installed from a thread
/// that never pumps is worse than no hook at all: the registration succeeds,
/// the callback is never dispatched, and every input event in the whole session
/// waits out `LowLevelHooksTimeout` before the OS gives up on us — which the
/// user feels as system-wide keyboard and mouse stutter. Hence the guard rather
/// than a comment.
#[cfg(target_os = "windows")]
fn install_input_hooks() {
    use std::sync::atomic::Ordering;
    use winapi::shared::windef::HHOOK;
    use winapi::um::libloaderapi::GetModuleHandleW;
    use winapi::um::processthreadsapi::GetCurrentThreadId;
    use winapi::um::winuser::{
        SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WH_MOUSE_LL,
    };

    unsafe {
        if HOOK_THREAD_ID.load(Ordering::SeqCst) != GetCurrentThreadId() {
            log::error!(
                target: "displays",
                "[HK-03] refused to install input hooks off the hook thread: they would \
                 never fire and would stall every input event in the session"
            );
            return;
        }

        let hinstance = GetModuleHandleW(std::ptr::null());

        // RAII-by-hand: whatever is in the slot is released before we overwrite
        // it, on every path.
        let old_kb = KB_HOOK_HANDLE.swap(0, Ordering::SeqCst);
        if old_kb != 0 {
            UnhookWindowsHookEx(old_kb as HHOOK);
        }
        let old_mouse = MOUSE_HOOK_HANDLE.swap(0, Ordering::SeqCst);
        if old_mouse != 0 {
            UnhookWindowsHookEx(old_mouse as HHOOK);
        }

        let kb = SetWindowsHookExW(WH_KEYBOARD_LL, Some(kb_hook_proc), hinstance, 0);
        if kb.is_null() {
            log::error!(target: "displays", "[HK-02] WH_KEYBOARD_LL install failed");
        } else {
            KB_HOOK_HANDLE.store(kb as usize, Ordering::SeqCst);
        }

        let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), hinstance, 0);
        if mouse.is_null() {
            log::error!(target: "displays", "[HK-02] WH_MOUSE_LL install failed");
        } else {
            MOUSE_HOOK_HANDLE.store(mouse as usize, Ordering::SeqCst);
        }
    }
}

/// [HK-03] Start the thread that owns the input hooks. It installs them and
/// then does nothing but pump, so no work anywhere else in this process can
/// delay a callback past `LowLevelHooksTimeout` — the watcher thread's pump,
/// which used to own them, also runs `wnd_proc` and its `Emitter::emit` on
/// session unlock.
///
/// Re-arms arrive as `WM_REARM_INPUT_HOOKS`; the requesting thread never
/// installs anything itself.
#[cfg(target_os = "windows")]
fn start_input_hook_thread() {
    use std::sync::atomic::Ordering;

    if let Err(e) = std::thread::Builder::new()
        .name("themia-input-hooks".into())
        .spawn(|| unsafe {
            use winapi::um::processthreadsapi::GetCurrentThreadId;
            use winapi::um::winuser::{
                DispatchMessageW, GetMessageW, PeekMessageW, TranslateMessage, MSG, PM_NOREMOVE,
                WM_USER,
            };

            let mut msg: MSG = std::mem::zeroed();
            // Force the queue into existence before publishing the id, so a
            // re-arm posted by the watchdog can never land on a thread that
            // has no queue yet and be dropped.
            PeekMessageW(&mut msg, std::ptr::null_mut(), WM_USER, WM_USER, PM_NOREMOVE);
            HOOK_THREAD_ID.store(GetCurrentThreadId(), Ordering::SeqCst);

            install_input_hooks();

            loop {
                let r = GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0);
                if r <= 0 {
                    break;
                }
                if msg.message == WM_REARM_INPUT_HOOKS {
                    install_input_hooks();
                    continue;
                }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            HOOK_THREAD_ID.store(0, Ordering::SeqCst);
            log::error!(
                target: "displays",
                "[HK-03] input hook thread exited; keyboard and click handling are gone \
                 until restart"
            );
        })
    {
        log::error!(target: "displays", "[HK-03] input hook thread failed to start: {e}");
    }
}

/// [HK-02] Health checks to sit out before the next re-arm attempt, given how
/// many consecutive attempts have failed to bring the callbacks back.
///
/// Doubles to a cap instead of retrying at the health-check cadence forever: a
/// fresh registration that cannot be serviced makes the OS wait out its timeout
/// on every input event again, so an unbounded retry loop turns one dead hook
/// into permanent system-wide input lag.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn rearm_backoff_checks(consecutive_failures: u32) -> u32 {
    1u32 << consecutive_failures.min(4)
}

/// [HK-02] Health check, driven from the existing watchdog thread. Asks the
/// hook thread to reinstall when the OS reports input that none of our
/// callbacks saw.
#[cfg(target_os = "windows")]
fn rearm_input_hooks_if_dead(interval_ms: u64) {
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    use winapi::um::winuser::PostThreadMessageW;

    static LAST_HEARTBEAT: AtomicU64 = AtomicU64::new(0);
    static CONSECUTIVE_FAILURES: AtomicU32 = AtomicU32::new(0);
    static CHECKS_TO_SKIP: AtomicU32 = AtomicU32::new(0);

    let beat = HOOK_HEARTBEAT.load(Ordering::Relaxed);
    let moved = LAST_HEARTBEAT.swap(beat, Ordering::Relaxed) != beat;
    let installed = KB_HOOK_HANDLE.load(Ordering::SeqCst) != 0
        || MOUSE_HOOK_HANDLE.load(Ordering::SeqCst) != 0;

    if moved {
        // Callbacks are running again; the next failure starts from scratch.
        CONSECUTIVE_FAILURES.store(0, Ordering::Relaxed);
        CHECKS_TO_SKIP.store(0, Ordering::Relaxed);
    }

    if !hooks_look_dead(installed, moved, ms_since_last_input(), interval_ms) {
        return;
    }

    let skip = CHECKS_TO_SKIP.load(Ordering::Relaxed);
    if skip > 0 {
        CHECKS_TO_SKIP.store(skip - 1, Ordering::Relaxed);
        return;
    }

    let attempt = CONSECUTIVE_FAILURES.fetch_add(1, Ordering::Relaxed) + 1;
    CHECKS_TO_SKIP.store(
        rearm_backoff_checks(attempt).saturating_sub(1),
        Ordering::Relaxed,
    );

    log::error!(
        target: "displays",
        "[HK-02] input hooks stopped firing while the OS reported input; \
         re-arming (attempt {attempt})"
    );

    let tid = HOOK_THREAD_ID.load(Ordering::SeqCst);
    if tid == 0 {
        log::error!(target: "displays", "[HK-02] no input hook thread to re-arm on");
        return;
    }
    // SAFETY: posting a private message to our own thread's queue.
    if unsafe { PostThreadMessageW(tid, WM_REARM_INPUT_HOOKS, 0, 0) } == 0 {
        log::error!(
            target: "displays",
            "[HK-02] re-arm request could not be posted to the hook thread"
        );
    }
}

/// Fallback dismiss path, called from JS mousedown handlers. Redundant
/// once the WH_MOUSE_LL hook is installed (the hook fires first on any real
/// OS click), but kept for synthetic DOM mousedowns that don't traverse the
/// OS input queue (e.g. webdriver MouseEvent dispatch).
#[tauri::command]
fn dismiss_start_menu_if_open() -> bool {
    #[cfg(target_os = "windows")]
    unsafe {
        if !start_is_foreground() {
            return false;
        }
        inject_escape();
        true
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

/// Lower-case executable basename (e.g. `explorer.exe`) owning `pid`, or
/// None when the process can't be opened/queried.
#[cfg(target_os = "windows")]
unsafe fn process_basename(pid: u32) -> Option<String> {
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::processthreadsapi::OpenProcess;
    use winapi::um::winbase::QueryFullProcessImageNameW;
    use winapi::um::winnt::PROCESS_QUERY_LIMITED_INFORMATION;

    let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
    if handle.is_null() {
        return None;
    }
    let mut name_buf = [0u16; 260];
    let mut size: u32 = name_buf.len() as u32;
    let ok = QueryFullProcessImageNameW(handle, 0, name_buf.as_mut_ptr(), &mut size);
    CloseHandle(handle);
    if ok == 0 {
        return None;
    }
    let full = String::from_utf16_lossy(&name_buf[..size as usize]);
    Some(full.rsplit(['\\', '/']).next().unwrap_or("").to_ascii_lowercase())
}

/// Scan state threaded through `find_context_menu_proc` via the EnumWindows
/// lparam: whether a classic popup was seen, and the owner PIDs of any visible
/// WinUI popup candidates (resolved to "is the shell?" after the sweep).
#[cfg(target_os = "windows")]
struct CtxMenuScan {
    found_classic: bool,
    candidate_pids: Vec<u32>,
}

/// EnumWindows callback for `shell_context_menu_is_open`. Flags the classic
/// tracked popup (`#32768`) immediately, and collects the owner PID of every
/// visible WinUI content-island popup (the Win11 modern menu's class) for the
/// caller to match against the shell's processes.
#[cfg(target_os = "windows")]
unsafe extern "system" fn find_context_menu_proc(
    hwnd: winapi::shared::windef::HWND,
    lparam: winapi::shared::minwindef::LPARAM,
) -> i32 {
    use winapi::um::winuser::{GetClassNameW, GetWindowThreadProcessId, IsWindowVisible};

    let scan = &mut *(lparam as *mut CtxMenuScan);
    if scan.found_classic {
        return 0; // classic menu already found, stop enumeration
    }
    if IsWindowVisible(hwnd) == 0 {
        return 1;
    }

    let mut buf = [0u16; 64];
    let n = GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
    if n <= 0 {
        return 1;
    }
    let class = String::from_utf16_lossy(&buf[..n as usize]);

    // Classic tracked popup menu, always a transient menu surface.
    if class == "#32768" {
        scan.found_classic = true;
        return 0;
    }

    // Windows 11 modern context menu: a WinUI content-island popup
    // (`Microsoft.UI.Content.PopupWindowSiteBridge`; older builds used
    // `Xaml_WindowedPopupClass`). These classes are also used by app
    // flyouts/tooltips, so we don't trust them alone; record the owner PID and
    // let the caller confirm it belongs to a shell (explorer.exe) process.
    if class == "Microsoft.UI.Content.PopupWindowSiteBridge"
        || class == "Xaml_WindowedPopupClass"
    {
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid != 0 {
            scan.candidate_pids.push(pid);
        }
    }
    1
}

/// PIDs of every running explorer.exe. The Win11 shell context menu is owned by
/// an explorer process, but NOT necessarily the same instance that owns the
/// desktop (`GetShellWindow`): with "launch folder windows in a separate
/// process" or extra shell hosts there are several. One Toolhelp snapshot is
/// cheaper and more robust than `OpenProcess`-ing each candidate window.
#[cfg(target_os = "windows")]
unsafe fn explorer_pids() -> std::collections::HashSet<u32> {
    use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
    use winapi::um::tlhelp32::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    let mut pids = std::collections::HashSet::new();
    let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
    if snap == INVALID_HANDLE_VALUE {
        return pids;
    }
    let mut entry: PROCESSENTRY32W = std::mem::zeroed();
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    if Process32FirstW(snap, &mut entry) != 0 {
        loop {
            let end = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
            let name = String::from_utf16_lossy(&entry.szExeFile[..end]).to_ascii_lowercase();
            if name == "explorer.exe" {
                pids.insert(entry.th32ProcessID);
            }
            if Process32NextW(snap, &mut entry) == 0 {
                break;
            }
        }
    }
    CloseHandle(snap);
    pids
}

/// True when a Windows shell context menu (the desktop / Explorer right-click
/// menu) is currently open. Like the Start Menu, it does NOT auto-dismiss when
/// the user clicks one of our WS_EX_NOACTIVATE widget windows, because clicking
/// us never changes the foreground window.
///
/// The Win11 menu doesn't take foreground (a foreground-only probe misses it),
/// so we sweep top-level windows, but cheaply: GetClassName +
/// GetWindowThreadProcessId per window, NO per-window OpenProcess. The process
/// snapshot that confirms shell ownership runs ONLY when a candidate popup is
/// actually visible (the common no-menu click never reaches it), so the hot
/// path stays sub-millisecond. Runs off the WH_MOUSE_LL input hook, only from
/// the per-mousedown command.
#[cfg(target_os = "windows")]
unsafe fn shell_context_menu_is_open() -> bool {
    use winapi::um::winuser::EnumWindows;

    let mut scan = CtxMenuScan { found_classic: false, candidate_pids: Vec::new() };
    EnumWindows(
        Some(find_context_menu_proc),
        &mut scan as *mut CtxMenuScan as isize,
    );

    if scan.found_classic {
        return true;
    }
    if scan.candidate_pids.is_empty() {
        return false;
    }
    let shell = explorer_pids();
    scan.candidate_pids.iter().any(|pid| shell.contains(pid))
}

/// Fallback dismiss path for the shell context menu, mirroring
/// `dismiss_start_menu_if_open`. Called from the JS mousedown handler; the
/// WH_MOUSE_LL hook covers real OS clicks ahead of this.
#[tauri::command]
fn dismiss_context_menu_if_open() -> bool {
    #[cfg(target_os = "windows")]
    unsafe {
        if !shell_context_menu_is_open() {
            return false;
        }
        inject_escape();
        true
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

/// Returns the label of our widget window whose monitor the cursor is
/// currently on, or None if the cursor isn't over any of our windows.
/// Used to route single-window shortcuts (ctrl+z) to the screen the user
/// is actually looking at, regardless of which monitor they're on.
#[cfg(target_os = "windows")]
unsafe fn widget_window_label_at_cursor() -> Option<String> {
    use winapi::shared::windef::POINT;
    use winapi::um::winuser::GetCursorPos;

    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);

    let app = WATCHER_APP.get()?;
    use tauri::Manager;
    for (label, win) in app.webview_windows() {
        if let (Ok(pos), Ok(size)) = (win.outer_position(), win.outer_size()) {
            let x0 = pos.x;
            let y0 = pos.y;
            let x1 = x0 + size.width as i32;
            let y1 = y0 + size.height as i32;
            if pt.x >= x0 && pt.x < x1 && pt.y >= y0 && pt.y < y1 {
                return Some(label.clone());
            }
        }
    }
    None
}

/// Check whether the cursor is currently over the desktop area (not covered
/// by another app's window). Returns true when the window under the cursor is
/// the desktop itself or one of our own widget windows.
#[cfg(target_os = "windows")]
unsafe fn cursor_is_over_desktop() -> bool {
    use winapi::shared::windef::{HWND, POINT};
    use winapi::um::winuser::{GetAncestor, GetCursorPos, GetDesktopWindow, WindowFromPoint, GA_ROOT};

    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);
    let hwnd_at_cursor = WindowFromPoint(pt);
    if hwnd_at_cursor.is_null() {
        return false;
    }

    // Get the top-level window owning whatever is under the cursor
    let root = GetAncestor(hwnd_at_cursor, GA_ROOT);
    let root = if root.is_null() { hwnd_at_cursor } else { root };

    // Desktop window (Progman / WorkerW)
    let desktop = GetDesktopWindow();
    if root == desktop {
        return true;
    }

    // One of our own widget windows
    if let Some(app) = WATCHER_APP.get() {
        use tauri::Manager;
        for (_label, win) in app.webview_windows() {
            if let Ok(h) = win.hwnd() {
                if h.0 as usize as HWND == root {
                    return true;
                }
            }
        }
    }

    false
}

/// Outcome of resolving a folder-level keystroke to its target widget window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WidgetKbRouting {
    /// Consume the keystroke (`return 1`). Only true when the target window
    /// was live AND the emit succeeded. Otherwise the keystroke must fall
    /// through to `CallNextHookEx` so the desktop/shell still receives it.
    consume: bool,
    /// Clear the stale `WIDGET_KB_ACTIVE`/label state because the resolved
    /// label no longer maps to a live window.
    clear_stale: bool,
}

/// Pure routing decision for a folder-level keystroke once we know whether the
/// target widget window was found and whether the emit succeeded.
///
/// - window present + emit ok  → consume, keep routing.
/// - window present + emit err → consume (the live window owns the key; a
///   transient emit failure shouldn't leak the keystroke to the shell), keep
///   routing.
/// - window absent             → do NOT consume (fall through so the desktop
///   gets the key) and clear the stale route so we stop trying to use it.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn decide_widget_kb_routing(window_found: bool, emit_ok: bool) -> WidgetKbRouting {
    let _ = emit_ok; // emit failure on a live window still consumes (see above)
    WidgetKbRouting {
        consume: window_found,
        clear_stale: !window_found,
    }
}

/// WH_KEYBOARD_LL hook: fires for every key press globally.
/// Intercepts folder-level shortcuts (Ctrl+C/V/X/A, Delete, Escape, F2)
/// when WIDGET_KB_ACTIVE is set, and app-level shortcuts (Ctrl+E/W/1-9)
/// when the cursor is over the desktop area.
#[cfg(target_os = "windows")]
unsafe extern "system" fn kb_hook_proc(
    n_code: i32,
    w_param: winapi::shared::minwindef::WPARAM,
    l_param: winapi::shared::minwindef::LPARAM,
) -> winapi::shared::minwindef::LRESULT {
    // FFI boundary (A2): a panic unwinding into the OS hook chain is UB. Catch it
    // and, on panic, fall through to CallNextHookEx so the keystroke still reaches
    // the shell. Success path returns the impl's value unchanged.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        kb_hook_proc_impl(n_code, w_param, l_param)
    }))
    .unwrap_or_else(|_| unsafe {
        winapi::um::winuser::CallNextHookEx(std::ptr::null_mut(), n_code, w_param, l_param)
    })
}

unsafe fn kb_hook_proc_impl(
    n_code: i32,
    w_param: winapi::shared::minwindef::WPARAM,
    l_param: winapi::shared::minwindef::LPARAM,
) -> winapi::shared::minwindef::LRESULT {
    use std::sync::atomic::Ordering;
    use winapi::um::winuser::{
        CallNextHookEx, GetAsyncKeyState, KBDLLHOOKSTRUCT,
        WM_KEYDOWN, WM_SYSKEYDOWN,
    };

    // [HK-02] Liveness evidence; see `rearm_input_hooks_if_dead`.
    note_hook_alive();

    if n_code >= 0 {
        let msg = w_param as u32;
        if msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN {
            let kb = &*(l_param as *const KBDLLHOOKSTRUCT);
            let ctrl = GetAsyncKeyState(0x11_i32) < 0; // VK_CONTROL
            let shift = GetAsyncKeyState(0x10_i32) < 0; // VK_SHIFT
            let alt = GetAsyncKeyState(0x12_i32) < 0;   // VK_MENU

            // While a text input is focused, the overlay window holds real
            // Win32 focus, native delivery works and the DOM handlers own
            // every key. Intercepting here ate Backspace/Enter/Escape/arrows
            // in inline inputs (todo add-task, rename fields) and swallowed
            // the Backspaces Vietnamese IME tools inject to replace composed
            // characters ("Chào" → "Chaào"). Stand down entirely (issue #35).
            let text_input_focused = TEXT_INPUT_FOCUSED.load(Ordering::Relaxed);

            // ── App-level shortcuts (Ctrl+E/W/1-9, no Shift/Alt) ──
            // Checked by cursor position: works even with WS_EX_NOACTIVATE.
            if ctrl && !shift && !alt {
                let app_action: Option<String> = match kb.vkCode {
                    0x45 => Some("e".into()),       // Ctrl+E  toggle edit mode
                    0x57 => Some("w".into()),       // Ctrl+W  open widget picker
                    0x5A => Some("z".into()),       // Ctrl+Z  undo last file operation
                    vk @ 0x31..=0x39 => Some(String::from(char::from(vk as u8))), // Ctrl+1-9
                    _ => None,
                };

                if let Some(action) = app_action {
                    // Todo SPEC [S-38]: while a text input is focused, Ctrl+Z
                    // falls through to the input's native text undo and
                    // Ctrl+1-9 must not tear down the edit via a view switch.
                    // Ctrl+E/W stay consumed: passing them through hands
                    // them to WebView2's browser accelerators (Ctrl+W =
                    // close window); the JS dispatcher ignores them while an
                    // input is focused, so consuming is a safe no-op there.
                    if should_intercept_app_key(&action, text_input_focused)
                        && cursor_is_over_desktop()
                    {
                        // [HK-01] Hand the delivery to the emitter thread.
                        // Resolving a window or emitting takes Tauri's manager
                        // lock, which the main thread holds across window
                        // creation and teardown — i.e. exactly the display
                        // storms during which people keep typing. Doing it here
                        // is what got the hook evicted.
                        //
                        // For undo (Ctrl+Z) the action is non-idempotent, so it
                        // goes to one window; the rest broadcast.
                        request_hook_emit(if action == "z" {
                            HookEmit::AppKeyOneWindow(action)
                        } else {
                            HookEmit::AppKeyBroadcast(action)
                        });
                        return 1; // consume
                    }
                }
            }

            // ── Escape (no modifiers): app-level deselect ──
            // Whenever the cursor is over our desktop surface (desktop wallpaper
            // or any widget window), Escape broadcasts `folder-clear-selection`
            // to every window. This lets users cancel a cut/selection from
            // anywhere in the app, not just from inside the folder widget that
            // currently owns keyboard routing. Passes through untouched when
            // the cursor is over another app, so Chrome/VS Code/etc. still get
            // their own Escape handling. And when a text input is focused,
            // so Escape cancels the inline edit via its DOM keydown handler.
            if !ctrl && !shift && !alt && kb.vkCode == 0x1B
                && should_consume_desktop_escape(
                    text_input_focused,
                    ESCAPE_WANTED.load(Ordering::Relaxed),
                )
                && cursor_is_over_desktop() {
                    // [HK-01] Off-hook delivery; see the app-key path above.
                    request_hook_emit(HookEmit::ClearSelection);
                    return 1; // consume
                }

            // ── Folder-level shortcuts (Ctrl+C/V/X/A, Delete, F2,
            //    arrow nav, Enter, Backspace, Home/End/PageUp/PageDown) ──
            // Only consume when the cursor is over the desktop or one of our
            // own widget windows, otherwise keystrokes pass through to the
            // foreground app (so Ctrl+C/V/X in Chrome, VS Code, etc. still
            // work normally even while a folder widget is the active target).
            // Never consume while a text input is focused (issue #35): the
            // input owns these keys natively.
            if should_intercept_widget_keys(
                WIDGET_KB_ACTIVE.load(Ordering::Relaxed),
                text_input_focused,
            ) && cursor_is_over_desktop() {
                let action: Option<&str> = widget_kb_action(ctrl, kb.vkCode);

                if let Some(action) = action {
                    // Encode Shift state into the payload so widget-side can
                    // distinguish plain vs. range-selecting arrow keys.
                    let payload = if shift { format!("shift+{action}") } else { action.to_string() };
                    let label = widget_kb_label().lock().unwrap_or_else(|e| e.into_inner()).clone();

                    // Resolve the routing target and remember whether it was a
                    // live window. We only consume the keystroke when there is
                    // a window to route it to. Otherwise we must fall through
                    // to CallNextHookEx so the keystroke still reaches the
                    // desktop/shell instead of being swallowed by a dead route.
                    //
                    // [HK-01] The liveness test reads the lock-free overlay
                    // snapshot, not `Manager::get_webview_window`: the answer is
                    // the same, and this one cannot block behind the main
                    // thread's manager lock long enough to get the hook evicted.
                    let window_found = !label.is_empty()
                        && overlay_snapshot_contains(WIDGET_KB_HWND.load(Ordering::Relaxed));
                    if window_found {
                        request_hook_emit(HookEmit::FolderKey { label, payload });
                    }

                    // `emit_ok` is true because the event is queued, not yet
                    // delivered. The decision has never depended on it: a live
                    // window owns the key either way (see the doc comment).
                    let outcome = decide_widget_kb_routing(window_found, true);
                    if outcome.clear_stale {
                        // The resolved label no longer maps to a live window
                        // (target closed/destroyed). Stop routing to it so the
                        // hook doesn't keep swallowing keystrokes, and drop the
                        // stale label so future activations start clean.
                        WIDGET_KB_ACTIVE.store(false, Ordering::Relaxed);
                        WIDGET_KB_HWND.store(0, Ordering::Relaxed);
                        widget_kb_label().lock().unwrap_or_else(|e| e.into_inner()).clear();
                    }
                    if outcome.consume {
                        return 1; // consume: target received the event
                    }
                    // else: fall through to CallNextHookEx below.
                }
            }
        }
    }
    CallNextHookEx(std::ptr::null_mut(), n_code, w_param, l_param)
}

/// WH_MOUSE_LL hook: dismisses Start/Search the instant a real OS mouse
/// click lands on one of our widget windows, without waiting for the JS
/// IPC round-trip. This matches the sub-100ms responsiveness the shell
/// gives to normal apps (which get foreground-change activation on click).
/// The fast-path is cheap: `start_is_foreground()` returns immediately on
/// the common case where Start is closed.
#[cfg(target_os = "windows")]
unsafe extern "system" fn mouse_hook_proc(
    n_code: i32,
    w_param: winapi::shared::minwindef::WPARAM,
    l_param: winapi::shared::minwindef::LPARAM,
) -> winapi::shared::minwindef::LRESULT {
    // FFI boundary (A2): catch panics so they never unwind into the OS; on panic
    // pass the event through via CallNextHookEx.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        mouse_hook_proc_impl(n_code, w_param, l_param)
    }))
    .unwrap_or_else(|_| unsafe {
        winapi::um::winuser::CallNextHookEx(std::ptr::null_mut(), n_code, w_param, l_param)
    })
}

unsafe fn mouse_hook_proc_impl(
    n_code: i32,
    w_param: winapi::shared::minwindef::WPARAM,
    l_param: winapi::shared::minwindef::LPARAM,
) -> winapi::shared::minwindef::LRESULT {
    use winapi::um::winuser::{
        CallNextHookEx, MSLLHOOKSTRUCT,
        WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_RBUTTONDOWN,
    };

    // [HK-02] Liveness evidence: any callback at all proves the hook is still
    // installed. Read by `rearm_input_hooks_if_dead`.
    note_hook_alive();

    if n_code >= 0 {
        let msg = w_param as u32;
        if msg == WM_LBUTTONDOWN || msg == WM_RBUTTONDOWN || msg == WM_MBUTTONDOWN {
            let ms = &*(l_param as *const MSLLHOOKSTRUCT);
            let over_widget = point_over_widget_window(ms.pt);
            // A click into one of our WS_EX_NOACTIVATE widgets never changes
            // the foreground window, so the shell won't auto-dismiss an open
            // Start Menu the way it does for a normal app. Inject Escape to
            // close it. (The desktop/Explorer context menu is handled off this
            // hot path, from the JS mousedown handler via
            // `dismiss_context_menu_if_open`, detecting it needs a window scan,
            // too heavy to run inside a low-level input hook.)
            if over_widget && start_is_foreground() {
                inject_escape();
            }
            // A click that lands outside every themia window means the user
            // clicked away from the app. Broadcast `themia-blur` so selection-
            // bearing widgets (folder, shortcut, contacts) clear their
            // highlight. This is the reliable path: the WH_MOUSE_LL hook fires
            // on every physical click regardless of which window holds the
            // foreground, whereas a DOM `blur` / the foreground hook only fired
            // once because our WS_EX_NOACTIVATE windows don't cleanly
            // re-acquire focus after the first app-switch.
            //
            // [HK-01] Handed to the emitter thread rather than emitted here:
            // `Emitter::emit` walks every webview under the manager lock and
            // evals into each one. Doing that inline put an unbounded,
            // main-thread-owned lock on the LowLevelHooksTimeout budget.
            if !over_widget {
                request_blur_broadcast();
            }
        }
    }
    CallNextHookEx(std::ptr::null_mut(), n_code, w_param, l_param)
}

/// SetWinEventHook callback: fires when any other application's window
/// becomes the foreground window.  Clears WIDGET_KB_ACTIVE so our hook
/// stops intercepting keys once the user has moved to another app.
///
/// Also detects Show Desktop activation: when Progman or any WorkerW takes
/// the foreground, the shell has just raised the desktop layer above all
/// our widgets. We re-pin each widget HWND just above Progman so the
/// user-visible "minimized" effect goes away.
/// Returns true if `hwnd` is one of our overlay widget windows (tracked in
/// wndproc_map). Used by the foreground hook to detect themia self-activation.
#[cfg(target_os = "windows")]
fn is_widget_hwnd(hwnd: winapi::shared::windef::HWND) -> bool {
    // [HK-01] Reads the lock-free snapshot, not the map: this runs inside the
    // EVENT_SYSTEM_FOREGROUND callback, on the same thread as the low-level
    // input hooks, so a blocked lock here costs the hooks their timeout budget.
    overlay_snapshot_contains(hwnd as usize)
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn foreground_change_proc(
    hook:   winapi::shared::windef::HWINEVENTHOOK,
    event:  u32,
    hwnd:   winapi::shared::windef::HWND,
    id_obj: i32,
    id_ch:  i32,
    thread: u32,
    time:   u32,
) {
    // FFI boundary (A2): swallow any panic so it never unwinds into the OS
    // WinEvent dispatch.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        foreground_change_proc_impl(hook, event, hwnd, id_obj, id_ch, thread, time)
    }));
}

unsafe fn foreground_change_proc_impl(
    _hook:   winapi::shared::windef::HWINEVENTHOOK,
    _event:  u32,
    hwnd:    winapi::shared::windef::HWND,
    _id_obj: i32,
    _id_ch:  i32,
    _thread: u32,
    _time:   u32,
) {
    use std::sync::atomic::Ordering;

    // [ZP-01] A foreground change is the main precursor to a restack; hold the
    // z-order poll at its fast cadence for the next couple of seconds.
    note_shell_activity();

    WIDGET_KB_ACTIVE.store(false, Ordering::Relaxed);
    WIDGET_KB_HWND.store(0, Ordering::Relaxed);

    if hwnd.is_null() {
        return;
    }

    // Screen-time accrual (screentime SPEC [S-20]): every foreground
    // transition switches the active usage bucket. Cheap on this hook thread:
    // pid resolve + a short tracker-lock hold; never panics (crate contract).
    {
        let mut pid: winapi::shared::minwindef::DWORD = 0;
        winapi::um::winuser::GetWindowThreadProcessId(hwnd, &mut pid);
        if pid != 0 {
            widget_screentime::note_foreground(pid);
        }
    }

    use winapi::um::winuser::GetClassNameW;
    let mut buf = [0u16; 64];
    let n = GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
    if n <= 0 {
        return;
    }
    let class = String::from_utf16_lossy(&buf[..n as usize]);
    // The shell desktop layer (Progman / WorkerW / its child shell views)
    // gaining foreground is NECESSARY for Show Desktop, but not sufficient.
    // A plain click on the desktop also makes it foreground. Only treat it
    // as Show Desktop when no real app window is left visible; otherwise a
    // simple desktop click would flip themia topmost and yank it in front
    // of every other window.
    let is_desktop = matches!(
        class.as_str(),
        "Progman" | "WorkerW" | "SHELLDLL_DefView" | "SysListView32",
    );

    if is_desktop {
        // Desktop gained foreground (plain desktop click or Show Desktop):
        // broadcast deselect so any folder-item selections in themia widgets
        // clear, mirroring the way clicking the desktop deselects icons.
        if let Some(app) = WATCHER_APP.get() {
            use tauri::Emitter;
            let _ = app.emit("folder-clear-selection", ());
            // Generic "app lost focus" signal: every selection-bearing widget
            // (folder, shortcut, contacts) clears its item highlight on this.
            // A DOM `blur` is unreliable for our WS_EX_NOACTIVATE windows (they
            // don't cleanly re-acquire focus after the first cycle), but this
            // WinEvent foreground hook fires on every foreground transition.
            let _ = app.emit("themia-blur", ());
        }
        if any_normal_app_window_visible() {
            // Plain desktop click, apps are still up. Leave z-order alone;
            // themia stays anchored just above Progman, behind real apps.
            return;
        }
        // Genuine Show Desktop: pin widgets to the top of the topmost band so
        // they float above Progman (which the shell elevates past normal z-order).
        // Routed through the shared apply so the event path pins-to-top exactly
        // like the poll does, fixing the race where the hook only flipped the flag
        // and left secondary monitors below until the next ~200ms tick.
        log::info!(target: "z-order", "Show Desktop ({class}), pinning widgets topmost");
        apply_zaction(ZAction::PinTopmost);
        return;
    }

    // [ZS-01] A transient shell surface — Start menu, Search, Task View, the
    // taskbar or one of its flyouts — is NOT "a real app regained foreground".
    // Treating it as one is what made every widget vanish the instant the user
    // pressed the Windows key during a Show Desktop (themia-releases#104): the
    // topmost pin is the only thing holding widgets above the desktop layer the
    // shell elevates for Show Desktop, and dismissing Start hands the foreground
    // back to the taskbar (another shell surface), not to the desktop, so
    // nothing ever restored it. Hold the current z-state instead. The surface
    // itself is topmost and was activated after us, so it still paints on top.
    //
    // Focus is a separate question from z-order: the user HAS left the widgets,
    // so the blur broadcast still goes out and selections still clear ([S-37]).
    if hwnd_is_shell_surface(hwnd, &class) {
        if let Some(app) = WATCHER_APP.get() {
            use tauri::Emitter;
            let _ = app.emit("themia-blur", ());
        }
        return;
    }

    let overlay_self_activated = is_widget_hwnd(hwnd);
    if overlay_self_activated {
        // Themia self-activated from a widget click. Demote z-order so the
        // overlay still sits BEHIND real apps visually (SWP_NOACTIVATE
        // means we don't yank focus). Z-order and foreground are
        // independent in Win32: themia keeps focus to receive the click
        // event, but paints below other apps because of z-order.
        //
        // We intentionally do NOT call SetForegroundWindow here. Doing so
        // in the foreground hook runs between WM_LBUTTONDOWN and
        // WM_LBUTTONUP, so the WebView2 sees the foreground transition
        // mid-click and drops the click event entirely. Z-order demotion
        // alone gives the visual effect the user wants without breaking
        // input dispatch.
        // [ZS-02] Demoting the overlay is the WHOLE fix for "an activated
        // widget covered the settings window": the desktop anchor puts the
        // overlay below every normal-band window, settings included. We
        // deliberately never raise the settings window to repair z-order —
        // see `overlay_activation_demotes`.
        raise_widgets_above_desktop();
        return;
    }

    // Real app regained foreground: drop topmost and re-anchor below apps.
    // Also broadcast the generic "app lost focus" signal so widget selections
    // clear when the user clicks into another application (not just the
    // desktop). Reliable per-switch where a DOM `blur` is not.
    if let Some(app) = WATCHER_APP.get() {
        use tauri::Emitter;
        let _ = app.emit("themia-blur", ());
    }
    raise_widgets_above_desktop();
}

// When Show Desktop is active, the shell elevates Progman past normal z-order;
// SetWindowPos(widget, progman, ...) cannot beat it. The pragmatic workaround
// (also used by RocketDock and similar always-on-desktop apps) is to flip the
// widget to WS_EX_TOPMOST while the desktop is foreground, then drop back to
// non-topmost the moment any real app regains focus. This bool tracks the
// transient topmost state so we don't redundantly toggle.
#[cfg(target_os = "windows")]
static WIDGETS_PINNED_TOPMOST: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Total transitions of WIDGETS_PINNED_TOPMOST since process start. Used by
/// integration tests as a flicker detector: a single Show Desktop press
/// should produce at most one transition.
#[cfg(target_os = "windows")]
static TOPMOST_TRANSITIONS: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);

/// Test-only kill switch for the z-order polling tick. Lets specs observe
/// transient z-order states (e.g. after a deliberate demote) without the
/// polling thread restoring before the read lands.
#[cfg(target_os = "windows")]
static POLLING_PAUSED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Apply or remove WS_EX_TOPMOST z-order to every tracked widget HWND. Used to
/// briefly float widgets above Progman during Show Desktop, then drop them
/// back below normal app windows when a real app regains foreground.
#[cfg(target_os = "windows")]
fn set_widgets_topmost(topmost: bool) {
    use std::sync::atomic::Ordering;
    use winapi::shared::windef::HWND;
    use winapi::um::winuser::{
        IsWindow, SetWindowPos, HWND_NOTOPMOST, HWND_TOPMOST,
        SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    };

    if WIDGETS_PINNED_TOPMOST.swap(topmost, Ordering::Relaxed) == topmost {
        return; // already in target state, no transition, no churn
    }
    TOPMOST_TRANSITIONS.fetch_add(1, Ordering::Relaxed);

    let anchor = if topmost { HWND_TOPMOST } else { HWND_NOTOPMOST };
    let hwnds: Vec<usize> = match wndproc_map().lock() {
        Ok(map) => map.keys().copied().collect(),
        Err(_) => return,
    };
    for raw in hwnds {
        let hwnd = raw as HWND;
        if hwnd.is_null() { continue; }
        // [PL-01] A pin overlay is topmost in EVERY state. The demotion half of
        // this (`HWND_NOTOPMOST` when a real app comes back) is exactly what
        // would sink it behind that app, so it is excluded from both halves
        // rather than re-promoted afterwards.
        if hwnd_is_pin_layer(hwnd) { continue; }
        unsafe {
            if IsWindow(hwnd) == 0 { continue; }
            SetWindowPos(
                hwnd,
                anchor,
                0, 0, 0, 0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// [ZS-01] Transient shell surfaces
// ---------------------------------------------------------------------------

/// Window classes that host a *transient shell surface*: the Start menu,
/// Search, Task View, the taskbar and its flyouts. These are not the desktop
/// layer and they are not applications either — they float over whatever the
/// user was looking at and hand the foreground straight back when dismissed.
///
/// `Windows.UI.Core.CoreWindow` is the WinUI host class shared by Start,
/// Search and Task View, but also by ordinary UWP apps, so a class match alone
/// is not enough; every lookup is narrowed by owning process
/// ([`SHELL_SURFACE_PROCESSES`]).
const SHELL_SURFACE_CLASSES: &[&str] = &[
    "Windows.UI.Core.CoreWindow",  // Start / Search / Task View (WinUI host)
    "Shell_TrayWnd",               // taskbar, primary monitor
    "Shell_SecondaryTrayWnd",      // taskbar, secondary monitors
    "TrayNotifyWnd",               // clock / tray flyouts
    "NotifyIconOverflowWindow",    // tray overflow flyout
    "MultitaskingViewFrame",       // Task View (Win10)
    "DV2ControlHost",              // classic Start menu (Win7 shell, StartIsBack)
    "ClassicShell.CMenuContainer", // Open-Shell / Classic Shell Start menu
    "Xaml_WindowedPopupClass",     // Win11 shell flyouts
];

/// Processes allowed to own a [`SHELL_SURFACE_CLASSES`] window. `explorer.exe`
/// covers the taskbar, Task View, and the in-process Start replacements
/// (StartIsBack, Open-Shell) that reporters actually run; the rest are the
/// out-of-process Start / Search hosts across Win10 and Win11 releases.
const SHELL_SURFACE_PROCESSES: &[&str] = &[
    "explorer.exe",
    "startmenuexperiencehost.exe", // Win10 1903+ / Win11 Start
    "shellexperiencehost.exe",     // Win10 pre-1903 Start, Action Center
    "searchhost.exe",              // Win11 Search
    "searchapp.exe",               // Win10 Search
    "searchui.exe",                // Win10 pre-1909 Search
    "textinputhost.exe",           // touch keyboard / emoji panel
];

/// [ZS-01] Pure classifier: is this (class, owning process) pair a transient
/// shell surface? Platform-independent so the allowlist is unit-tested on every
/// target, not just Windows. A `None` process (the query failed) is never a
/// shell surface — the safe answer is "treat it like an app".
fn is_shell_surface(class: &str, process: Option<&str>) -> bool {
    SHELL_SURFACE_CLASSES.contains(&class)
        && process.is_some_and(|p| SHELL_SURFACE_PROCESSES.contains(&p))
}

/// [ZS-01] Live form of [`is_shell_surface`] for a specific HWND whose class
/// the caller has already read.
#[cfg(target_os = "windows")]
unsafe fn hwnd_is_shell_surface(hwnd: winapi::shared::windef::HWND, class: &str) -> bool {
    use winapi::um::winuser::GetWindowThreadProcessId;

    // Cheap reject first: the class allowlist costs nothing, the process-name
    // lookup is an OpenProcess + QueryFullProcessImageNameW.
    if !SHELL_SURFACE_CLASSES.contains(&class) {
        return false;
    }
    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, &mut pid);
    if pid == 0 {
        return false;
    }
    is_shell_surface(class, process_basename(pid).as_deref())
}

/// [ZS-01] True iff a transient shell surface currently owns the foreground.
/// The z-order reconcile uses this to hold its state instead of tearing the
/// Show-Desktop pin down (see `decide_zaction`).
#[cfg(target_os = "windows")]
fn shell_surface_is_foreground() -> bool {
    use winapi::um::winuser::{GetClassNameW, GetForegroundWindow};
    unsafe {
        let fg = GetForegroundWindow();
        if fg.is_null() {
            return false;
        }
        let mut buf = [0u16; 64];
        let n = GetClassNameW(fg, buf.as_mut_ptr(), buf.len() as i32);
        if n <= 0 {
            return false;
        }
        let class = String::from_utf16_lossy(&buf[..n as usize]);
        hwnd_is_shell_surface(fg, &class)
    }
}

#[cfg(test)]
mod shell_surface_tests {
    use super::is_shell_surface;

    // Regression (themia-releases#104): pressing the Windows key during a Show
    // Desktop must not read as "a real app came back". The Start menu is a
    // CoreWindow owned by one of the shell's Start hosts on every Windows
    // release the app supports, and StartIsBack / Open-Shell replacements run
    // inside explorer.exe.
    #[test]
    fn zs01_start_menu_hosts_are_shell_surfaces() {
        for proc in [
            "startmenuexperiencehost.exe",
            "shellexperiencehost.exe",
            "searchhost.exe",
            "searchapp.exe",
        ] {
            assert!(
                is_shell_surface("Windows.UI.Core.CoreWindow", Some(proc)),
                "{proc} should host a shell surface",
            );
        }
        assert!(is_shell_surface("DV2ControlHost", Some("explorer.exe")));
        assert!(is_shell_surface(
            "ClassicShell.CMenuContainer",
            Some("explorer.exe"),
        ));
    }

    #[test]
    fn zs01_taskbar_and_its_flyouts_are_shell_surfaces() {
        for class in [
            "Shell_TrayWnd",
            "Shell_SecondaryTrayWnd",
            "TrayNotifyWnd",
            "NotifyIconOverflowWindow",
            "MultitaskingViewFrame",
        ] {
            assert!(
                is_shell_surface(class, Some("explorer.exe")),
                "{class} should be a shell surface",
            );
        }
    }

    // The class allowlist alone is not a licence: CoreWindow is also every UWP
    // app's host class, and a File Explorer window is explorer.exe too. Both
    // must keep reading as ordinary apps so they still drop the topmost pin.
    #[test]
    fn zs01_uwp_apps_and_explorer_windows_are_not_shell_surfaces() {
        assert!(!is_shell_surface(
            "Windows.UI.Core.CoreWindow",
            Some("applicationframehost.exe"),
        ));
        assert!(!is_shell_surface(
            "Windows.UI.Core.CoreWindow",
            Some("calculator.exe"),
        ));
        assert!(!is_shell_surface("CabinetWClass", Some("explorer.exe")));
        assert!(!is_shell_surface("Chrome_WidgetWin_1", Some("chrome.exe")));
    }

    // An unresolvable owner (elevated process, race with exit) must fall back to
    // "app", the conservative side: a wrongly-held pin paints widgets over a
    // real window, a wrongly-dropped one only costs one reconcile tick.
    #[test]
    fn zs01_unknown_owner_is_not_a_shell_surface() {
        assert!(!is_shell_surface("Windows.UI.Core.CoreWindow", None));
        assert!(!is_shell_surface("Shell_TrayWnd", None));
    }
}

/// True iff the current foreground window belongs to the shell desktop layer
/// (Progman / any WorkerW / their child shell views). Used by the polling
/// safeguard to detect Show Desktop activation independently of the
/// EVENT_SYSTEM_FOREGROUND hook (which can miss events under load or on
/// multi-monitor setups where per-monitor WorkerW activations don't always
/// route through the global foreground).
#[cfg(target_os = "windows")]
fn shell_desktop_is_foreground() -> bool {
    use winapi::um::winuser::{GetClassNameW, GetForegroundWindow};
    unsafe {
        let fg = GetForegroundWindow();
        if fg.is_null() {
            return false;
        }
        let mut buf = [0u16; 64];
        let n = GetClassNameW(fg, buf.as_mut_ptr(), buf.len() as i32);
        if n <= 0 {
            return false;
        }
        let class = String::from_utf16_lossy(&buf[..n as usize]);
        matches!(
            class.as_str(),
            "Progman" | "WorkerW" | "SHELLDLL_DefView" | "SysListView32",
        )
    }
}

/// True iff the foreground thread is currently in menu mode: a popup menu
/// (e.g. the Explorer desktop context menu the user opened on a gap between
/// widgets) is open and running its modal loop. Mirrors the `GUI_INMENUMODE`
/// probe in `dismiss_foreign_menu`. The z-order poll uses this to stand down
/// entirely while a menu is up (see `zorder_tick_should_stand_down`).
#[cfg(target_os = "windows")]
fn foreground_menu_active() -> bool {
    use winapi::um::winuser::{GetGUIThreadInfo, GUITHREADINFO};
    const GUI_INMENUMODE: u32 = 0x0000_0004;
    unsafe {
        let mut gti: GUITHREADINFO = std::mem::zeroed();
        gti.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
        // Thread id 0 → the current foreground thread.
        if GetGUIThreadInfo(0, &mut gti) == 0 {
            return false;
        }
        gti.flags & GUI_INMENUMODE != 0
    }
}

/// EnumWindows callback for `any_normal_app_window_visible`. Sets `*lparam`
/// (a `*mut bool`) to true and stops enumeration the moment it finds one
/// genuine, visible, non-cloaked app window.
#[cfg(target_os = "windows")]
unsafe extern "system" fn find_visible_app_proc(
    hwnd: winapi::shared::windef::HWND,
    lparam: winapi::shared::minwindef::LPARAM,
) -> i32 {
    use winapi::um::dwmapi::DwmGetWindowAttribute;
    use winapi::um::winuser::{
        GetClassNameW, GetWindowLongW, GetWindowTextLengthW, IsIconic,
        IsWindowVisible, GWL_EXSTYLE, WS_EX_TOOLWINDOW,
    };

    const DWMWA_CLOAKED: u32 = 14;

    let found = &mut *(lparam as *mut bool);
    if *found {
        return 0; // already found one, stop enumeration
    }

    // Not visible / minimized → can't be the reason the user still has an
    // app "up", so skip.
    if IsWindowVisible(hwnd) == 0 {
        return 1;
    }
    if IsIconic(hwnd) != 0 {
        return 1;
    }

    // Tool windows (palettes, our own overlays, etc.) don't count as a
    // "real app window" for Show Desktop detection.
    let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
    if ex & WS_EX_TOOLWINDOW != 0 {
        return 1;
    }

    // No title → almost always a helper / message-only window.
    if GetWindowTextLengthW(hwnd) == 0 {
        return 1;
    }

    // Our own widget overlays don't count.
    if is_widget_hwnd(hwnd) {
        return 1;
    }

    // Shell surfaces (desktop, taskbar, start) don't count.
    let mut buf = [0u16; 64];
    let n = GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
    if n > 0 {
        let class = String::from_utf16_lossy(&buf[..n as usize]);
        if matches!(
            class.as_str(),
            "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
                | "SHELLDLL_DefView" | "SysListView32"
                | "Windows.UI.Core.CoreWindow",
        ) {
            return 1;
        }
    }

    // DWM-cloaked → hidden by Show Desktop on Win10/11. Skip.
    let mut cloaked: u32 = 0;
    DwmGetWindowAttribute(
        hwnd,
        DWMWA_CLOAKED,
        &mut cloaked as *mut u32 as *mut _,
        std::mem::size_of::<u32>() as u32,
    );
    if cloaked != 0 {
        return 1;
    }

    // Survived every filter: a real app window is still on screen.
    *found = true;
    0 // stop enumeration
}

/// True if at least one genuine, visible (non-minimized, non-cloaked) app
/// window is currently on screen. Used to tell a plain desktop click apart
/// from a real "Show Desktop": Show Desktop hides every app window, a click
/// does not.
#[cfg(target_os = "windows")]
fn any_normal_app_window_visible() -> bool {
    use winapi::um::winuser::EnumWindows;
    let mut found = false;
    unsafe {
        EnumWindows(Some(find_visible_app_proc), &mut found as *mut bool as isize);
    }
    found
}

/// EnumWindows callback for `any_app_window_cloaked`. Sets `*lparam` to true
/// and stops enumeration the moment it finds a single non-themia app window
/// that is currently DWM-cloaked (i.e. hidden by Show Desktop).
#[cfg(target_os = "windows")]
unsafe extern "system" fn find_cloaked_app_proc(
    hwnd: winapi::shared::windef::HWND,
    lparam: winapi::shared::minwindef::LPARAM,
) -> i32 {
    use winapi::um::dwmapi::DwmGetWindowAttribute;
    use winapi::um::winuser::{
        GetClassNameW, GetWindowLongW, GetWindowTextLengthW,
        GWL_EXSTYLE, WS_EX_TOOLWINDOW,
    };
    const DWMWA_CLOAKED: u32 = 14;

    let found = &mut *(lparam as *mut bool);
    if *found { return 0; }

    let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
    if ex & WS_EX_TOOLWINDOW != 0 { return 1; }
    if GetWindowTextLengthW(hwnd) == 0 { return 1; }
    if is_widget_hwnd(hwnd) { return 1; }

    let mut buf = [0u16; 64];
    let n = GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
    if n > 0 {
        let class = String::from_utf16_lossy(&buf[..n as usize]);
        if matches!(
            class.as_str(),
            "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
                | "SHELLDLL_DefView" | "SysListView32"
                | "Windows.UI.Core.CoreWindow",
        ) {
            return 1;
        }
    }

    let mut cloaked: u32 = 0;
    DwmGetWindowAttribute(
        hwnd,
        DWMWA_CLOAKED,
        &mut cloaked as *mut u32 as *mut _,
        std::mem::size_of::<u32>() as u32,
    );
    if cloaked != 0 {
        *found = true;
        return 0;
    }
    1
}

/// True iff at least one non-themia, non-shell app window is currently
/// DWM-cloaked. Show Desktop on Win10/11 hides apps via DWM cloak, so the
/// presence of a cloaked app is a positive signal that Show Desktop is
/// genuinely in effect (as opposed to "Progman happens to be foreground
/// because nothing else has focus").
#[cfg(target_os = "windows")]
fn any_app_window_cloaked() -> bool {
    use winapi::um::winuser::EnumWindows;
    let mut found = false;
    unsafe {
        EnumWindows(Some(find_cloaked_app_proc), &mut found as *mut bool as isize);
    }
    found
}

/// The z-order action the background reconcile takes once it is past the idle
/// fast-path. Pure decision core for the overlay z-state (W1 / 1.23): the
/// reconcile routes its non-idle branch through `decide_zaction_after_idle` so
/// the z-state logic lives in one unit-tested place instead of an inline tree.
/// (The idle fast-path and its slow self-heal backstop cadence stay inline in
/// the poll, since they are stateful timing, not a z-state decision.)
///
/// Platform-independent on purpose: pure `bool -> enum` logic with no Win32, so
/// it compiles and is unit-tested on every target, not just Windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ZAction {
    /// Genuine Show Desktop: pin widgets to the top of the topmost band so the
    /// shell can't elevate Progman past them.
    PinTopmost,
    /// Show Desktop dismissed (or never genuine) but the topmost pin is still
    /// held, drop back to the just-above-Progman anchor.
    DropToAnchor,
    /// Neither: re-anchor any widget that has fallen below Progman (external
    /// restack, missed event, per-monitor Show-Desktop quirk).
    SelfHeal,
    /// [ZS-01] A transient shell surface (Start / Search / Task View / taskbar)
    /// owns the foreground while we hold the pin: change nothing. Keeping the
    /// pin is what keeps widgets above the elevated desktop layer, and NOT
    /// re-raising is what lets the surface — activated after us, in the same
    /// topmost band — stay on top of them.
    HoldTopmost,
}

/// Pure z-decision for the reconcile's non-idle path. `show_desktop_active` is
/// the canonical determination (shell desktop foreground AND no normal app
/// visible AND an app cloaked); `pinned_topmost` is the current latch;
/// `shell_surface_foreground` is [ZS-01].
///
/// The shell-surface arm exists because the pin latch used to be driven by "did
/// something other than the desktop take the foreground", which the Start menu
/// satisfies. Dropping the pin there put every widget back under the desktop
/// layer the shell elevates during Show Desktop, i.e. they vanished on the
/// Windows key, and stayed gone because dismissing Start hands the foreground
/// to the taskbar rather than the desktop (themia-releases#104).
fn decide_zaction(
    show_desktop_active: bool,
    pinned_topmost: bool,
    shell_surface_foreground: bool,
) -> ZAction {
    if show_desktop_active {
        ZAction::PinTopmost
    } else if pinned_topmost {
        if shell_surface_foreground {
            ZAction::HoldTopmost
        } else {
            ZAction::DropToAnchor
        }
    } else {
        ZAction::SelfHeal
    }
}

/// The z-order poll must stand down completely while a shell popup menu is in
/// its modal loop. Pure `bool -> bool` so it is unit-tested on every target.
///
/// Why: when the user has a desktop / Explorer context menu open, the shell
/// desktop layer (Progman / WorkerW / SHELLDLL_DefView) owns the foreground,
/// so `shell_desktop_is_foreground()` reads true and the poll's idle fast-path
/// is bypassed. Without this guard the tick would run the EnumWindows-heavy
/// `reconcile_overlay_zorder()` at 12.5 Hz AND issue z-order `SetWindowPos`
/// calls against the desktop band *during the shell's modal `TrackPopupMenu`
/// loop*. That storm saturates the main-thread message pump (the reported
/// lag), starves the `requestAnimationFrame` that pushes the `SetWindowRgn`
/// hit-mask (the region "stops updating"), and can take down the WebView2 GPU
/// process (the occasional crash), all triggered by opening the themia
/// context menu on top of an already-open desktop menu.
///
/// A modal popup menu is never part of a genuine Show Desktop, and the
/// `EVENT_SYSTEM_FOREGROUND` hook + the 80 ms backstop resume reconciliation
/// the instant the menu closes, so standing down here costs no z-order safety.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn zorder_tick_should_stand_down(foreground_menu_active: bool) -> bool {
    foreground_menu_active
}

/// [ZS-02] What to do about z-order when a widget overlay is activated (a click
/// on an interactive widget). The answer is always "move the overlay", never
/// "move the settings window".
///
/// Why this is a rule and not a detail: the old repair raised every detached
/// Themia window with `SetWindowPos(HWND_TOPMOST)` followed by
/// `SetWindowPos(HWND_NOTOPMOST)`. The second call re-inserts the window at the
/// TOP of the entire non-topmost band — above every other application, not just
/// above our overlays. Since the repair ran on every interactivity flip
/// (hovering a focusable widget is enough), the settings window climbed in
/// front of whatever the user had switched to, seemingly on its own, "after
/// some time". Demoting the overlay to `z_above_desktop()` achieves the same
/// relative order (settings above overlay) while leaving every window the app
/// does not own exactly where the user put it.
///
/// The one exception is the Show Desktop pin: while widgets are deliberately
/// held topmost, demoting would drop them under the shell's elevated desktop
/// layer, which is the "all widgets disappear" failure. Hold instead.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn overlay_activation_demotes(widgets_pinned_topmost: bool) -> bool {
    !widgets_pinned_topmost
}

#[cfg(test)]
mod zorder_decision_tests {
    use super::{decide_zaction, overlay_activation_demotes, zorder_tick_should_stand_down, ZAction};

    // Characterizes the original inline tree in `ensure_widget_zorder_tick`:
    // Show Desktop wins regardless of the pin latch; otherwise the latch decides
    // drop-vs-heal. All four input combinations are pinned here so a later
    // refactor of the reconcile can't silently change the z-state mapping.
    #[test]
    fn show_desktop_pins_topmost_regardless_of_latch() {
        assert_eq!(decide_zaction(true, false, false), ZAction::PinTopmost);
        assert_eq!(decide_zaction(true, true, false), ZAction::PinTopmost);
    }

    #[test]
    fn dismissed_show_desktop_while_pinned_drops_to_anchor() {
        assert_eq!(decide_zaction(false, true, false), ZAction::DropToAnchor);
    }

    #[test]
    fn neither_show_desktop_nor_pinned_self_heals() {
        assert_eq!(decide_zaction(false, false, false), ZAction::SelfHeal);
    }

    // Regression (themia-releases#104): the Start menu / taskbar taking the
    // foreground during a Show Desktop must HOLD the pin, not drop it. Dropping
    // put the widgets back under the shell's elevated desktop layer, which is
    // what the reporter saw as "all widgets disappear when pressing the windows
    // button".
    #[test]
    fn zs01_shell_surface_foreground_holds_the_pin() {
        assert_eq!(decide_zaction(false, true, true), ZAction::HoldTopmost);
    }

    // The hold is only meaningful while the pin is actually held; with no pin
    // there is nothing to protect and the self-heal backstop still applies.
    #[test]
    fn zs01_shell_surface_without_a_pin_still_self_heals() {
        assert_eq!(decide_zaction(false, false, true), ZAction::SelfHeal);
    }

    // Regression (themia-releases#36): a still-open desktop right-click menu
    // makes the shell desktop layer the foreground window, which defeats the
    // poll's idle fast-path. The tick must skip ALL of its work while a popup
    // menu is modal. Otherwise the EnumWindows + z-order-write storm against
    // the desktop band laggs the app, freezes the SetWindowRgn hit-mask and can
    // crash it. The 80ms backstop + foreground hook resume on menu close.
    #[test]
    fn zorder_tick_stands_down_while_a_shell_menu_is_modal() {
        assert!(zorder_tick_should_stand_down(true));
        assert!(!zorder_tick_should_stand_down(false));
    }

    // Regression [ZS-02]: activating a widget overlay repairs z-order by
    // demoting the OVERLAY. The previous repair raised every detached Themia
    // window to the top of the non-topmost band instead, which is above every
    // other application — the settings window climbed in front of whatever app
    // the user had switched to on the next widget hover.
    #[test]
    fn zs02_overlay_activation_demotes_the_overlay() {
        assert!(overlay_activation_demotes(false));
    }

    // ...except while the Show Desktop pin is held: demoting there drops the
    // widgets under the shell's elevated desktop layer (themia-releases#104).
    #[test]
    fn zs02_overlay_activation_holds_while_pinned_topmost() {
        assert!(!overlay_activation_demotes(true));
    }
}

/// True iff "Show Desktop" is genuinely active: the shell desktop layer is
/// foreground AND no real app window is visible. A plain click on the
/// desktop makes the desktop foreground too, but leaves app windows up; we
/// must NOT treat that as Show Desktop, or themia flips itself topmost and
/// jumps in front of everything on every desktop click.
#[cfg(target_os = "windows")]
fn show_desktop_is_active() -> bool {
    // Show Desktop requires:
    //   (1) Progman/WorkerW currently owns foreground (shell desktop layer is
    //       on top), AND
    //   (2) at least one real app got DWM-cloaked by it.
    // The cloak signal disambiguates from the "no apps exist at all" case,
    // in which Progman happens to be foreground but there's nothing to hide
    // (e.g. a test env with no other apps open). Without (2) we would pin
    // widgets topmost forever in that environment.
    shell_desktop_is_foreground()
        && !any_normal_app_window_visible()
        && any_app_window_cloaked()
}

/// Returns true iff the given widget HWND is strictly above Progman in z-order.
#[cfg(target_os = "windows")]
fn is_widget_above_progman(widget: winapi::shared::windef::HWND) -> bool {
    use std::ptr;
    use winapi::um::winuser::{FindWindowW, GetWindow, GW_HWNDPREV};

    let progman_name: Vec<u16> = "Progman\0".encode_utf16().collect();
    let progman = unsafe { FindWindowW(progman_name.as_ptr(), ptr::null()) };
    if progman.is_null() {
        return true; // Can't tell; assume fine.
    }
    let mut cursor = unsafe { GetWindow(progman, GW_HWNDPREV) };
    let mut hops = 0;
    while !cursor.is_null() && hops < 8192 {
        if cursor == widget {
            return true;
        }
        cursor = unsafe { GetWindow(cursor, GW_HWNDPREV) };
        hops += 1;
    }
    false
}

/// Force every tracked widget to the TOP of the topmost group. Two-pass:
/// first ordinary SetWindowPos(HWND_TOPMOST), then if the WS_EX_TOPMOST bit
/// didn't actually take, force it via SetWindowLong + SWP_FRAMECHANGED.
///
/// KNOWN LIMITATION (upstream wry/WebView2 bug): only the FIRST window built
/// in the process can be promoted to WS_EX_TOPMOST. Every subsequent window
/// silently rejects the topmost flip, no error returned. Result on a
/// multi-monitor setup: only `monitor-0`'s widget stays above the desktop
/// during Show Desktop; other monitors' widgets get covered. Reproduced in
/// a minimal Tauri-only test app (see `repro/wry-multimonitor-topmost/`),
/// and verified that NO userland workaround (raw SetWindowPos, SetWindowLong,
/// AttachThreadInput, owner inheritance, hide/show cycles, Tauri's
/// `set_always_on_top`, builder `always_on_top(true)`) can re-promote the
/// rejected windows. Re-enable the multi-monitor e2e test in
/// `tests/e2e/specs/43-win-d-survives.spec.ts` after the upstream fix.
#[cfg(target_os = "windows")]
fn pin_widgets_to_topmost_top() {
    use winapi::shared::windef::HWND;
    use winapi::um::winuser::{
        GetWindowLongW, IsWindow, SetWindowLongW, SetWindowPos,
        GWL_EXSTYLE, HWND_TOPMOST, SWP_FRAMECHANGED, SWP_NOACTIVATE,
        SWP_NOMOVE, SWP_NOSIZE, WS_EX_TOPMOST,
    };

    let hwnds: Vec<usize> = match wndproc_map().lock() {
        Ok(map) => map.keys().copied().collect(),
        Err(_) => return,
    };

    for raw in hwnds {
        let hwnd = raw as HWND;
        if hwnd.is_null() { continue; }
        unsafe {
            if IsWindow(hwnd) == 0 { continue; }

            SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                0, 0, 0, 0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );

            let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
            if (ex & WS_EX_TOPMOST) == 0 {
                SetWindowLongW(hwnd, GWL_EXSTYLE, (ex | WS_EX_TOPMOST) as i32);
                SetWindowPos(
                    hwnd,
                    HWND_TOPMOST,
                    0, 0, 0, 0,
                    SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
    }

}

/// Apply a decided z-state to every tracked widget: the single z-band "apply"
/// side of the controller (W1). The reconcile and the foreground hook both go
/// through here, so "pin for Show Desktop" means the exact same thing at every
/// call site instead of one site flipping topmost and another also pinning-to-top.
#[cfg(target_os = "windows")]
fn apply_zaction(action: ZAction) {
    match action {
        ZAction::PinTopmost => {
            // Flip the flag (cheap if already set), then force every widget to the
            // TOP of the topmost group, required for secondary monitors whose
            // WorkerW can elevate INTO the topmost group after our flip.
            set_widgets_topmost(true);
            pin_widgets_to_topmost_top();
        }
        ZAction::DropToAnchor => raise_widgets_above_desktop(),
        ZAction::SelfHeal => self_heal_widgets_above_desktop(),
        // [ZS-01] Deliberately nothing: not even a re-raise, or we would fight
        // the Start menu for the top of the topmost band.
        ZAction::HoldTopmost => {}
    }
}

/// Decide + apply the overlay z-state from current shell signals: the single
/// reconcile entry point (W1). The 200 ms poll's non-idle path and the
/// foreground hook both call this, so the z-band can never be driven by two
/// divergent determinations. `show_desktop_is_active()` (not merely
/// shell-foreground) is required, since a plain desktop click also makes the desktop
/// foreground but must NOT pull themia topmost.
#[cfg(target_os = "windows")]
fn reconcile_overlay_zorder() {
    use std::sync::atomic::Ordering;
    let pinned = WIDGETS_PINNED_TOPMOST.load(Ordering::Relaxed);
    // Cheap probe before the expensive one. `shell_surface_is_foreground()` is a
    // class read plus a process-name lookup; `show_desktop_is_active()` is two
    // EnumWindows sweeps. A shell surface owning the foreground already settles
    // the decision, and it also implies the shell desktop does NOT own it, so
    // short-circuiting here cannot change the outcome — it just stops the sweeps
    // running at the fast cadence for as long as the Start menu stays open.
    let shell_surface = pinned && shell_surface_is_foreground();
    let show_desktop = !shell_surface && show_desktop_is_active();
    apply_zaction(decide_zaction(show_desktop, pinned, shell_surface));
}

// ---------------------------------------------------------------------------
// [ZP-01] Z-order poll pacing
// ---------------------------------------------------------------------------

/// Fast cadence, ~one frame at 60Hz. Fast enough to beat a secondary monitor's
/// WorkerW back into place after a Show Desktop.
const ZORDER_POLL_FAST_MS: u64 = 80;

/// Idle cadence. Nothing can legitimately restack us while the shell is quiet
/// and a normal app owns the foreground, so the poll is pure backstop there.
const ZORDER_POLL_IDLE_MS: u64 = 500;

/// How many fast ticks a single activity signal buys. 25 × 80ms ≈ 2s, which
/// comfortably covers a Show Desktop / restore transition and its aftershocks.
const ZORDER_FAST_TICKS: u32 = 25;

/// [HK-02] Hook liveness is checked at most this often.
#[cfg(target_os = "windows")]
const HOOK_HEALTH_INTERVAL_MS: u64 = 4_000;

/// Bumped by every shell signal that can precede a restack: foreground change,
/// DWM cloak, and the display/settings/taskbar broadcasts. Read by the poll to
/// decide whether it still needs the fast cadence.
#[cfg(target_os = "windows")]
static SHELL_ACTIVITY_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(target_os = "windows")]
fn note_shell_activity() {
    SHELL_ACTIVITY_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// [ZP-01] Pure pacing rule: an activity signal refills the fast budget, an
/// idle tick spends one. Saturating, so the idle state is a stable floor.
fn next_fast_ticks(current: u32, active: bool) -> u32 {
    if active {
        ZORDER_FAST_TICKS
    } else {
        current.saturating_sub(1)
    }
}

/// [ZP-01] Pure pacing rule: any remaining fast budget means the fast cadence.
fn zorder_poll_interval_ms(fast_ticks: u32) -> u64 {
    if fast_ticks > 0 {
        ZORDER_POLL_FAST_MS
    } else {
        ZORDER_POLL_IDLE_MS
    }
}

/// True while the current shell state is one where the desktop layer could
/// legitimately restack us, i.e. where the fast cadence still earns its keep:
/// the shell owns the foreground (Show Desktop, desktop click), or we are
/// currently holding the topmost pin and must keep re-asserting it.
#[cfg(target_os = "windows")]
fn zorder_race_possible() -> bool {
    use std::sync::atomic::Ordering;
    // Cheap: one GetForegroundWindow + GetClassNameW, plus an atomic load.
    WIDGETS_PINNED_TOPMOST.load(Ordering::Relaxed) || shell_desktop_is_foreground()
}

/// Background polling tick: reconcile widget z-order with current shell state.
/// Cadence is [`ZORDER_POLL_FAST_MS`] around shell activity, backing off to
/// [`ZORDER_POLL_IDLE_MS`] while idle ([ZP-01]).
#[cfg(target_os = "windows")]
fn ensure_widget_zorder_tick() {
    use std::sync::atomic::Ordering;

    if POLLING_PAUSED.load(Ordering::Relaxed) {
        return;
    }

    // Stand down completely while a shell popup menu is in its modal loop
    // (most commonly the Explorer desktop context menu). The shell desktop
    // layer owns the foreground then, so the idle fast-path below would be
    // bypassed and this tick would storm EnumWindows + z-order writes against
    // the desktop band during the menu's modal loop, lagging the app,
    // freezing the SetWindowRgn hit-mask and risking a crash (see
    // `zorder_tick_should_stand_down`). The foreground hook + the 80ms backstop
    // resume reconciliation the instant the menu closes.
    if zorder_tick_should_stand_down(foreground_menu_active()) {
        return;
    }

    use winapi::shared::windef::HWND;
    use winapi::um::winuser::IsWindow;

    // Safety net for the frameless invariant on INTERACTIVE windows. While a
    // window is interactive its region clip is cleared, so the region is no
    // longer hiding any residual OS frame, and tao / WebView2 can re-touch
    // the window's styles or proc at any point during the interaction (text
    // selection, IME, focus churn). `reassert_frameless` re-applies the
    // strip + subclass + frame recalc; it is idempotent and a no-op once the
    // invariant already holds. Limited to interactive windows so the common
    // (clipped) case pays nothing. Runs before the z-order branches below so
    // it happens every tick regardless of shell state.
    {
        let interactive: Vec<usize> = match wndproc_map().lock() {
            Ok(map) => map
                .keys()
                .copied()
                .filter(|h| is_window_interactive(*h))
                .collect(),
            Err(_) => Vec::new(),
        };
        for raw in interactive {
            let hwnd = raw as HWND;
            if hwnd.is_null() {
                continue;
            }
            unsafe {
                if IsWindow(hwnd) != 0 {
                    reassert_frameless(hwnd);
                }
            }
        }
    }

    // Idle fast-path. The shell desktop layer (Progman/WorkerW) can only
    // re-elevate past our widgets while IT owns the foreground, i.e. during
    // Show Desktop. When a normal app is foreground and we don't currently
    // hold the topmost pin, neither the Show-Desktop branch nor the
    // below-Progman self-heal below can legitimately fire, so the EnumWindows
    // scans inside `show_desktop_is_active()` and the per-widget z-order walks
    // are pure idle CPU burn at ~12.5 Hz. `shell_desktop_is_foreground()` is a
    // single GetForegroundWindow + GetClassNameW, so the steady-state tick
    // costs ~nothing and we still react to a real Show Desktop within one tick
    // (and the EVENT_SYSTEM_FOREGROUND hook catches it independently anyway).
    //
    // Keep a slow (~1s) self-heal backstop for the rare external restack (some
    // other app forcing a global z-order change) so we still recover, just
    // not a dozen times a second while genuinely idle.
    if !shell_desktop_is_foreground()
        && !WIDGETS_PINNED_TOPMOST.load(Ordering::Relaxed)
    {
        static IDLE_BACKSTOP: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        // ~80ms tick × 12 ≈ 1s between backstop sweeps.
        if !IDLE_BACKSTOP.fetch_add(1, Ordering::Relaxed).is_multiple_of(12) {
            return;
        }
        self_heal_widgets_above_desktop();
        return;
    }

    // Past the idle fast-path: one decision, one apply, through the shared core.
    reconcile_overlay_zorder();
}

/// Re-anchor any tracked widget that has fallen BELOW Progman in the z-order
/// (external restack, missed event, per-monitor Show Desktop quirk) back to
/// just above the desktop. Each below-Progman check walks the z-order chain,
/// so callers gate this behind the cheap foreground check rather than running
/// it on every 80ms tick while idle.
#[cfg(target_os = "windows")]
fn self_heal_widgets_above_desktop() {
    use winapi::shared::windef::HWND;
    use winapi::um::winuser::{
        IsWindow, SetWindowPos, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    };

    let anchor = z_above_desktop();
    let hwnds: Vec<usize> = match wndproc_map().lock() {
        Ok(map) => map.keys().copied().collect(),
        Err(_) => return,
    };
    for raw in hwnds {
        let hwnd = raw as HWND;
        if hwnd.is_null() {
            continue;
        }
        // [PL-01] "Below Progman" is not a state a topmost window can be in,
        // and the desktop anchor would demote it. Its own repair is the
        // WM_WINDOWPOSCHANGING rewrite, which re-asserts HWND_TOPMOST.
        if hwnd_is_pin_layer(hwnd) {
            continue;
        }
        unsafe {
            if IsWindow(hwnd) == 0 {
                continue;
            }
            if !is_widget_above_progman(hwnd) {
                SetWindowPos(
                    hwnd,
                    anchor,
                    0, 0, 0, 0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
    }
}

/// Re-pin every tracked widget HWND just above Progman in z-order. Called
/// after a real app regains foreground (Show Desktop deactivated) so the
/// widget settles back to its "below apps, above desktop" anchor.
#[cfg(target_os = "windows")]
fn raise_widgets_above_desktop() {
    set_widgets_topmost(false);
    // Then re-anchor below normal apps, above wallpaper.
    use winapi::shared::windef::HWND;
    use winapi::um::winuser::{
        IsWindow, SetWindowPos, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    };

    let anchor = z_above_desktop();
    let hwnds: Vec<usize> = match wndproc_map().lock() {
        Ok(map) => map.keys().copied().collect(),
        Err(_) => return,
    };
    for raw in hwnds {
        let hwnd = raw as HWND;
        if hwnd.is_null() {
            continue;
        }
        // [PL-01] Settling "below apps, above desktop" is the base overlay's
        // steady state, not the pin overlay's.
        if hwnd_is_pin_layer(hwnd) {
            continue;
        }
        unsafe {
            if IsWindow(hwnd) == 0 {
                continue;
            }
            SetWindowPos(
                hwnd,
                anchor,
                0, 0, 0, 0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }
}

/// SetWinEventHook callback for EVENT_OBJECT_CLOAKED. Win+D / "Show Desktop"
/// on Win10/11 hides apps via DWM cloaking, a path that does NOT go through
/// SC_MINIMIZE or WM_SHOWWINDOW, so the WndProc subclass cannot catch it.
/// Detect the cloak event and immediately uncloak our widget HWNDs.
///
/// We only act on HWNDs we own (those tracked in `wndproc_map`) and only on
/// the WINDOW object (id_obj == OBJID_WINDOW = 0); cloak fires for child
/// objects too which we don't care about.
#[cfg(target_os = "windows")]
unsafe extern "system" fn cloaked_event_proc(
    hook:   winapi::shared::windef::HWINEVENTHOOK,
    event:  u32,
    hwnd:   winapi::shared::windef::HWND,
    id_obj: i32,
    id_ch:  i32,
    thread: u32,
    time:   u32,
) {
    // FFI boundary (A2): swallow any panic so it never unwinds into the OS
    // WinEvent dispatch.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        cloaked_event_proc_impl(hook, event, hwnd, id_obj, id_ch, thread, time)
    }));
}

unsafe fn cloaked_event_proc_impl(
    _hook:   winapi::shared::windef::HWINEVENTHOOK,
    _event:  u32,
    hwnd:    winapi::shared::windef::HWND,
    id_obj:  i32,
    id_ch:   i32,
    _thread: u32,
    _time:   u32,
) {
    const OBJID_WINDOW: i32 = 0;
    const CHILDID_SELF: i32 = 0;

    // [ZP-01] A cloak is Show Desktop starting; keep the poll fast.
    note_shell_activity();

    if id_obj != OBJID_WINDOW || id_ch != CHILDID_SELF || hwnd.is_null() {
        return;
    }

    // Only uncloak windows we manage (subclassed via pin_to_desktop).
    let is_ours = wndproc_map()
        .lock()
        .ok()
        .map(|m| m.contains_key(&(hwnd as usize)))
        .unwrap_or(false);
    if !is_ours {
        return;
    }

    uncloak_widget(hwnd);
}

/// Undo any app/shell-applied DWM cloak on one of OUR widget windows. The
/// single uncloak path (W1): both the `EVENT_OBJECT_CLOAKED` hook and the
/// [ZC-01] backstop go through here, so "uncloak" means one thing.
#[cfg(target_os = "windows")]
fn uncloak_widget(hwnd: winapi::shared::windef::HWND) {
    use winapi::um::dwmapi::DwmSetWindowAttribute;
    use winapi::um::winuser::{ShowWindow, SW_SHOWNOACTIVATE};
    const DWMWA_CLOAK: u32 = 13;

    let cloak: u32 = 0; // FALSE
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_CLOAK,
            &cloak as *const _ as *const _,
            std::mem::size_of::<u32>() as u32,
        );

        // Belt-and-suspenders: also re-show in case the cloak was paired with
        // a hide. ShowWindow on an already-visible window is a no-op.
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
}

// ---------------------------------------------------------------------------
// [ZC-01] Cloak backstop
// ---------------------------------------------------------------------------

/// How often the [ZC-01] backstop re-checks the uncloaked invariant.
#[cfg(target_os = "windows")]
const CLOAK_CHECK_INTERVAL_MS: u64 = 1_000;

/// [ZC-01] Re-assert "not cloaked" on every widget window.
///
/// Show Desktop and a fullscreen exclusive app both hide windows by DWM-cloaking
/// them, and until now the only undo was `cloaked_event_proc` — an
/// out-of-context `WinEvent`, which Windows silently drops when the callback
/// can't be delivered inside the hook timeout. A fullscreen game is exactly the
/// load that makes that happen, and a dropped event left the widget cloaked
/// *forever*: still `IsWindowVisible`, still non-iconic, still above Progman,
/// simply never composited. That is the "widgets completely disappear with a
/// game open" half of themia-releases#104, and no amount of z-order work can
/// recover it, because z-order was never the problem.
///
/// Cheap enough for a 1 s cadence: one `DwmGetWindowAttribute` per widget
/// window, and a write only when the invariant is actually broken.
#[cfg(target_os = "windows")]
fn ensure_widget_cloak_tick() {
    use std::sync::atomic::Ordering;
    use winapi::shared::windef::HWND;
    use winapi::um::dwmapi::DwmGetWindowAttribute;
    use winapi::um::winuser::IsWindow;
    const DWMWA_CLOAKED: u32 = 14;

    if POLLING_PAUSED.load(Ordering::Relaxed) {
        return;
    }

    let hwnds: Vec<usize> = match wndproc_map().lock() {
        Ok(map) => map.keys().copied().collect(),
        Err(_) => return,
    };
    for raw in hwnds {
        let hwnd = raw as HWND;
        if hwnd.is_null() {
            continue;
        }
        unsafe {
            if IsWindow(hwnd) == 0 {
                continue;
            }
            let mut cloaked: u32 = 0;
            if DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED,
                &mut cloaked as *mut u32 as *mut _,
                std::mem::size_of::<u32>() as u32,
            ) != 0
            {
                continue; // query failed; nothing reliable to act on
            }
            if cloaked != 0 {
                log::warn!(
                    target: "z-order",
                    "widget {hwnd:?} found cloaked (reason {cloaked}) outside a cloak event, uncloaking",
                );
                uncloak_widget(hwnd);
            }
        }
    }
}

/// Returns the current cursor position (physical screen pixels) and
/// mouse/keyboard button states.  Used by the folder widget to track
/// drags on a WS_EX_NOACTIVATE window where the browser never fires
/// mousemove while a button is held on an unfocused window.
#[derive(serde::Serialize)]
struct CursorState {
    x: i32,
    y: i32,
    left_down: bool,
    ctrl_down: bool,
}

#[tauri::command]
fn get_cursor_state() -> CursorState {
    #[cfg(target_os = "windows")]
    {
        use winapi::shared::windef::POINT;
        use winapi::um::winuser::{GetAsyncKeyState, GetCursorPos};
        unsafe {
            let mut pt = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pt);
            let left_down = GetAsyncKeyState(0x01) < 0; // VK_LBUTTON
            let ctrl_down = GetAsyncKeyState(0x11) < 0; // VK_CONTROL
            CursorState { x: pt.x, y: pt.y, left_down, ctrl_down }
        }
    }
    #[cfg(not(target_os = "windows"))]
    CursorState { x: 0, y: 0, left_down: false, ctrl_down: false }
}

#[tauri::command]
async fn fs_delete(paths: Vec<String>) -> Result<Vec<widget_folder::TrashedFile>, String> {
    tauri::async_runtime::spawn_blocking(move || widget_folder::fs_delete(&paths))
        .await
        .map_err(|e| format!("spawn_blocking: {e}"))?
}

#[tauri::command]
fn fs_delete_permanent(paths: Vec<String>) -> Result<(), String> {
    widget_folder::fs_delete_permanent(&paths)
}

#[tauri::command]
fn fs_exists(path: String) -> bool {
    std::path::Path::new(&path).exists()
}

/// Fill state of the Recycle Bin (issue #94): the Shortcut widget badges a
/// `shell:RecycleBinFolder` item when the bin isn't empty. Runs on the blocking
/// pool because `SHQueryRecycleBin` walks the bin's metadata.
#[tauri::command]
async fn recycle_bin_status() -> Result<widget_folder::RecycleBinStatus, String> {
    tauri::async_runtime::spawn_blocking(widget_folder::recycle_bin_status)
        .await
        .map_err(|e| format!("spawn_blocking: {e}"))?
}

#[tauri::command]
async fn fs_restore_from_trash(items: Vec<widget_folder::TrashedFile>) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || widget_folder::fs_restore_from_trash(items))
        .await
        .map_err(|e| format!("spawn_blocking: {e}"))?
}

#[tauri::command]
fn fs_rename(path: String, new_name: String) -> Result<(), String> {
    widget_folder::fs_rename(&path, &new_name)
}

#[tauri::command]
fn show_file_properties(path: String) -> Result<(), String> {
    widget_folder::show_file_properties(&path)
}

#[tauri::command]
fn open_with(path: String) -> Result<(), String> {
    widget_folder::open_with(&path)
}

#[tauri::command]
async fn get_folder_item_counts(paths: Vec<String>) -> std::collections::HashMap<String, u32> {
    tauri::async_runtime::spawn_blocking(move || widget_folder::get_folder_item_counts(paths))
        .await
        .unwrap_or_default()
}

#[tauri::command]
async fn get_image_thumbnail(path: String, size: u32) -> Result<String, String> {
    tokio::task::spawn_blocking(move || widget_folder::get_image_thumbnail(&path, size))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn get_image_thumbnails_batch(
    paths: Vec<String>,
    size: u32,
) -> std::collections::HashMap<String, String> {
    tokio::task::spawn_blocking(move || widget_folder::get_image_thumbnails_batch(paths, size))
        .await
        .unwrap_or_default()
}

#[tauri::command]
fn fs_write_file(dest_dir: String, file_name: String, data: Vec<u8>) -> Result<(), String> {
    widget_folder::fs_write_file(&dest_dir, &file_name, &data)
}

#[tauri::command]
fn fs_download_url(url: String, dest_dir: String) -> Result<String, String> {
    widget_netguard::check_url(&url)?;
    widget_folder::fs_download_url(&url, &dest_dir)
}

/// Set the global SSRF bypass. Pushed by the frontend from the persisted design
/// store on startup and whenever the user toggles "Allow private network
/// access" in Settings. Default (before the frontend speaks) is secure: block.
#[tauri::command]
fn set_ssrf_bypass(enabled: bool) {
    widget_netguard::set_bypass(enabled);
}

#[tauri::command]
fn get_cloud_sync_folders() -> Vec<widget_folder::CloudFolder> {
    widget_folder::get_cloud_sync_folders()
}

#[tauri::command]
fn get_shell_new_entries() -> Vec<widget_folder::ShellNewEntry> {
    widget_folder::get_shell_new_entries()
}

#[tauri::command]
fn fs_create_folder(dest_dir: String, name: String) -> Result<String, String> {
    widget_folder::fs_create_folder(&dest_dir, &name)
}

#[tauri::command]
fn get_shell_menu_entries(is_directory: bool, is_background: bool) -> Vec<widget_folder::ShellMenuEntry> {
    widget_folder::get_shell_menu_entries(is_directory, is_background)
}

#[tauri::command]
fn execute_shell_command(
    verb_id: String,
    target_path: String,
    is_directory: bool,
    is_background: bool,
) -> Result<(), String> {
    widget_folder::execute_shell_command(&verb_id, &target_path, is_directory, is_background)
}

#[tauri::command]
fn get_file_attributes(path: String) -> u32 {
    widget_folder::get_file_attributes(&path)
}

#[tauri::command]
fn set_cloud_pin(paths: Vec<String>, mode: String) -> Result<(), String> {
    widget_folder::set_cloud_pin(&paths, &mode)
}

#[tauri::command]
async fn get_file_attributes_batch(
    paths: Vec<String>,
) -> std::collections::HashMap<String, u32> {
    tokio::task::spawn_blocking(move || widget_folder::get_file_attributes_batch(paths))
        .await
        .unwrap_or_default()
}

#[tauri::command]
async fn folder_read_dir(
    path: String,
) -> Result<Vec<widget_folder::FolderDirEntry>, String> {
    tokio::task::spawn_blocking(move || widget_folder::folder_read_dir(path))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn get_file_stats_batch(
    paths: Vec<String>,
) -> std::collections::HashMap<String, (u64, u64)> {
    tokio::task::spawn_blocking(move || widget_folder::get_file_stats_batch(paths))
        .await
        .unwrap_or_default()
}

#[tauri::command]
async fn get_git_status(path: String) -> widget_folder::GitStatus {
    tokio::task::spawn_blocking(move || widget_folder::get_git_status(&path))
        .await
        .unwrap_or_else(|_| widget_folder::get_git_status(""))
}

// ── Volume (default output device) ───────────────────────────────────────────

#[tauri::command]
fn volume_get() -> Result<widget_volume::VolumeState, String> {
    widget_volume::get_volume()
}

#[tauri::command]
fn volume_set(level: f32) -> Result<(), String> {
    widget_volume::set_volume(level)
}

#[tauri::command]
fn volume_set_mute(muted: bool) -> Result<(), String> {
    widget_volume::set_mute(muted)
}

// ── Mixer (per-application sessions on the default output device) ────────────

// Async + spawn_blocking, unlike the volume commands above: enumerating audio
// sessions walks every process holding one, and resolving an app's display name
// reads its version resource FROM DISK on the first sight of that binary. A
// sync command runs inline on the WebView2 main thread, so a cold-cache tick
// would stall the whole UI (mixer SPEC [S-02]).

#[tauri::command]
async fn mixer_get_sessions(grouped: bool) -> Result<Vec<widget_mixer::MixerApp>, String> {
    tauri::async_runtime::spawn_blocking(move || widget_mixer::get_sessions(grouped))
        .await
        .map_err(|e| format!("spawn_blocking: {e}"))?
}

#[tauri::command]
async fn mixer_set_volume(key: String, level: f32) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || widget_mixer::set_level(&key, level))
        .await
        .map_err(|e| format!("spawn_blocking: {e}"))?
}

#[tauri::command]
async fn mixer_set_mute(key: String, muted: bool) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || widget_mixer::set_muted(&key, muted))
        .await
        .map_err(|e| format!("spawn_blocking: {e}"))?
}

// ── Power (shut down / restart / sleep / lock / …) ───────────────────────────

// Async + spawn_blocking, unlike the volume commands: a sync command runs
// inline on the main event-loop thread, and every action here blocks it.
// ExitWindowsEx waits while each app is asked to close, the monitor-off
// broadcast waits on every top-level window, and SetSuspendState does not
// return until the machine RESUMES — potentially hours later (power SPEC
// [S-04]). `action` is a typed enum, so an unknown value is rejected by serde
// at the IPC boundary instead of reaching Win32.
#[tauri::command]
async fn power_run(action: widget_power::PowerAction) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || widget_power::run(action))
        .await
        .map_err(|e| format!("spawn_blocking: {e}"))?
}

#[tauri::command]
async fn get_git_repo_info_batch(paths: Vec<String>) -> std::collections::HashMap<String, widget_folder::GitRepoInfo> {
    tokio::task::spawn_blocking(move || widget_folder::get_git_repo_info_batch(paths))
        .await
        .unwrap_or_default()
}

#[tauri::command]
async fn scan_images(path: String, recursive: bool) -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(move || widget_gallery::scan_images(&path, recursive))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn read_image_base64(path: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || widget_gallery::read_image_base64(&path))
        .await
        .map_err(|e| e.to_string())?
}

// ── Local notes store ───────────────────────────────────────────────────────
// Small text files, but still disk IO on every keystroke-debounce flush, so
// each call goes to the blocking pool rather than stalling the UI thread.

#[tauri::command]
async fn notes_list(app: tauri::AppHandle) -> Vec<widget_notes::NoteMeta> {
    tauri::async_runtime::spawn_blocking(move || widget_notes::list_notes(&app))
        .await
        // ignore: a panicking blocking task is already logged by the runtime;
        // an empty list keeps the picker usable ("New note" still works).
        .unwrap_or_default()
}

#[tauri::command]
async fn notes_read(app: tauri::AppHandle, id: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || widget_notes::read_note(&app, &id))
        .await
        .map_err(|e| format!("spawn_blocking: {e}"))?
}

#[tauri::command]
async fn notes_write(app: tauri::AppHandle, id: String, content: String) -> Result<u64, String> {
    tauri::async_runtime::spawn_blocking(move || widget_notes::write_note(&app, &id, &content))
        .await
        .map_err(|e| format!("spawn_blocking: {e}"))?
}

#[tauri::command]
async fn notes_create(
    app: tauri::AppHandle,
    title: String,
    content: String,
) -> Result<widget_notes::NoteMeta, String> {
    tauri::async_runtime::spawn_blocking(move || widget_notes::create_note(&app, &title, &content))
        .await
        .map_err(|e| format!("spawn_blocking: {e}"))?
}

#[tauri::command]
async fn notes_rename(app: tauri::AppHandle, id: String, title: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || widget_notes::rename_note(&app, &id, &title))
        .await
        .map_err(|e| format!("spawn_blocking: {e}"))?
}

#[tauri::command]
async fn notes_delete(app: tauri::AppHandle, id: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || widget_notes::delete_note(&app, &id))
        .await
        .map_err(|e| format!("spawn_blocking: {e}"))?
}

#[tauri::command]
async fn ping_hosts(hosts: Vec<widget_ping::PingHost>) -> Result<Vec<widget_ping::PingResult>, String> {
    // `widget_ping::ping_hosts` blocks up to ~2s per down host (spawns `ping`).
    // Run it off the runtime so a ping-widget refresh never stalls the event loop.
    tauri::async_runtime::spawn_blocking(move || widget_ping::ping_hosts(hosts))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn calendar_get_events(
    app: tauri::AppHandle,
    account_id: String,
    start_dt: String,
    end_dt: String,
    calendar_id: Option<String>,
) -> Result<Vec<widget_email::CalendarEvent>, String> {
    widget_email::get_calendar_events(
        &app,
        &account_id,
        &start_dt,
        &end_dt,
        calendar_id.as_deref(),
    )
    .await
}

#[tauri::command]
async fn calendar_list_calendars(
    app: tauri::AppHandle,
    account_id: String,
) -> Result<Vec<widget_email::CalendarInfo>, String> {
    widget_email::list_calendars(&app, &account_id).await
}

#[tauri::command]
async fn calendar_get_birthdays(
    app: tauri::AppHandle,
    account_id: String,
) -> Result<Vec<widget_email::ContactBirthday>, String> {
    widget_email::get_contact_birthdays(&app, &account_id).await
}

#[tauri::command]
async fn todo_get_lists(
    app: tauri::AppHandle,
    account_id: String,
) -> Result<Vec<widget_email::TodoTaskList>, String> {
    widget_email::todo_get_lists(&app, &account_id).await
}

#[tauri::command]
async fn todo_get_tasks(
    app: tauri::AppHandle,
    account_id: String,
    list_id: String,
    show_completed: bool,
) -> Result<Vec<widget_email::TodoTask>, String> {
    widget_email::todo_get_tasks(&app, &account_id, &list_id, show_completed).await
}

#[tauri::command]
async fn todo_update_task(
    app: tauri::AppHandle,
    account_id: String,
    list_id: String,
    task_id: String,
    status: String,
) -> Result<(), String> {
    widget_email::todo_update_task(&app, &account_id, &list_id, &task_id, &status).await
}

#[tauri::command]
async fn todo_rename_task(
    app: tauri::AppHandle,
    account_id: String,
    list_id: String,
    task_id: String,
    title: String,
) -> Result<(), String> {
    widget_email::todo_rename_task(&app, &account_id, &list_id, &task_id, &title).await
}

#[tauri::command]
async fn todo_delete_task(
    app: tauri::AppHandle,
    account_id: String,
    list_id: String,
    task_id: String,
) -> Result<(), String> {
    widget_email::todo_delete_task(&app, &account_id, &list_id, &task_id).await
}

#[tauri::command]
async fn todo_create_task(
    app: tauri::AppHandle,
    account_id: String,
    list_id: String,
    title: String,
    // [S-52] `YYYY-MM-DD`, sent when the task is created from the Today view so
    // it is actually due today; absent everywhere else.
    due_date: Option<String>,
) -> Result<widget_email::TodoTask, String> {
    widget_email::todo_create_task(&app, &account_id, &list_id, &title, due_date.as_deref()).await
}

#[tauri::command]
async fn contacts_list(
    app: tauri::AppHandle,
    account_id: String,
    search: String,
    count: u32,
) -> Result<Vec<widget_email::Contact>, String> {
    widget_email::get_contacts(&app, &account_id, &search, count).await
}

// ── OneDrive ─────────────────────────────────────────────────────────────────

#[tauri::command]
async fn onedrive_recent_files(
    app: tauri::AppHandle,
    account_id: String,
    count: u32,
) -> Result<Vec<widget_email::OneDriveItem>, String> {
    widget_email::onedrive_recent_files(&app, &account_id, count).await
}

#[tauri::command]
async fn onedrive_list_folder(
    app: tauri::AppHandle,
    account_id: String,
    folder_id: String,
    count: u32,
) -> Result<Vec<widget_email::OneDriveItem>, String> {
    widget_email::onedrive_list_folder(&app, &account_id, &folder_id, count).await
}

// ── Battery ──────────────────────────────────────────────────────────────────

#[tauri::command]
fn get_battery_stats() -> widget_battery::BatteryStats {
    widget_battery::get_battery_stats_inner()
}

// ── Bluetooth ────────────────────────────────────────────────────────────────

#[tauri::command]
fn get_bluetooth_devices() -> Vec<widget_bluetooth::BluetoothDevice> {
    widget_bluetooth::get_bluetooth_devices_inner()
}

// ── Apps launcher ─────────────────────────────────────────────────────────────

#[derive(serde::Serialize, Clone)]
struct AppEntry {
    name: String,
    path: String,
    category: String,
}

fn scan_lnk_dir(dir: &std::path::Path, category: &str, out: &mut Vec<AppEntry>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let folder_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(category)
                .to_string();
            // Skip Startup folder, those aren't launcher items
            if folder_name.eq_ignore_ascii_case("Startup") { continue; }
            let sub_cat = if category.is_empty() { folder_name.clone() } else { folder_name };
            scan_lnk_dir(&path, &sub_cat, out);
        } else if path.extension().and_then(|e| e.to_str()).map(|e| e.eq_ignore_ascii_case("lnk")).unwrap_or(false) {
            let name = path
                .file_stem()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            let name_lower = name.to_lowercase();
            if name_lower.contains("uninstall") || name_lower.contains("uninst") { continue; }
            out.push(AppEntry {
                name,
                path: path.to_string_lossy().into_owned(),
                category: category.to_string(),
            });
        }
    }
}

#[tauri::command]
fn list_start_menu_apps() -> Vec<AppEntry> {
    let mut apps: Vec<AppEntry> = Vec::new();

    // User start menu
    if let Some(appdata) = std::env::var_os("APPDATA") {
        let p = std::path::Path::new(&appdata)
            .join(r"Microsoft\Windows\Start Menu\Programs");
        scan_lnk_dir(&p, "", &mut apps);
    }

    // System start menu
    if let Some(programdata) = std::env::var_os("PROGRAMDATA") {
        let p = std::path::Path::new(&programdata)
            .join(r"Microsoft\Windows\Start Menu\Programs");
        scan_lnk_dir(&p, "", &mut apps);
    }

    // Deduplicate by lower-case name (first occurrence wins, user overrides system)
    let mut seen = std::collections::HashSet::new();
    apps.retain(|a| seen.insert(a.name.to_lowercase()));

    apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    apps
}

// Stats sampling (sysinfo process fan-out, PDH collection, per-volume
// free-space queries, NVML) blocks for tens of ms, worst case seconds when a
// removable volume has spun down. Sync commands execute inline in the WebView2
// WebResourceRequested callback on the main event-loop thread, so both stats
// commands run via `spawn_blocking` like the music commands below.
// `include_procs` / `include_temp` gate the expensive process-table refresh
// and the WMI temperature sampler (cpu [S-21]).
#[tauri::command]
async fn get_system_stats(
    include_procs: Option<bool>,
    include_temp: Option<bool>,
) -> Result<widget_cpu::SystemStats, String> {
    tauri::async_runtime::spawn_blocking(move || {
        widget_cpu::get_system_stats_inner(
            include_procs.unwrap_or(false),
            include_temp.unwrap_or(false),
        )
    })
    .await
    .map_err(|e| e.to_string())
}

// Screen-time query (screentime SPEC [S-30]). The tracker lock can be held
// across a store flush (file IO) by the sampler thread, so run off the main
// event-loop thread like get_system_stats.
#[tauri::command]
async fn screentime_get_day(date: Option<String>) -> Result<widget_screentime::DayStats, String> {
    tauri::async_runtime::spawn_blocking(move || widget_screentime::get_day(date))
        .await
        .map_err(|e| e.to_string())
}

// Screen-time rolling window (screentime SPEC [S-41]): one DayStats per day of
// the last `days` days ending today, for the week/month charts. Same off-thread
// treatment as screentime_get_day.
#[tauri::command]
async fn screentime_get_range(days: u32) -> Result<Vec<widget_screentime::DayStats>, String> {
    tauri::async_runtime::spawn_blocking(move || widget_screentime::get_range(days))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn list_gpus() -> Vec<widget_gpu::GpuInfo> {
    widget_gpu::list_gpus_inner()
}

#[tauri::command]
async fn get_gpu_stats(gpu_index: u32) -> Result<widget_gpu::GpuStats, String> {
    tauri::async_runtime::spawn_blocking(move || widget_gpu::get_gpu_stats_inner(gpu_index))
        .await
        .map_err(|e| e.to_string())
}

// The music inner fns block on WinRT `.get()` (SMTC). Run them off the async
// runtime via `spawn_blocking` so they don't occupy tokio runtime threads.
#[tauri::command]
async fn music_get_now_playing() -> Result<Option<widget_music::NowPlayingInfo>, String> {
    tauri::async_runtime::spawn_blocking(widget_music::get_now_playing_inner)
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn music_play_pause() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(widget_music::music_play_pause_inner)
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn music_next() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(widget_music::music_next_inner)
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn music_previous() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(widget_music::music_previous_inner)
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn music_seek(position_ms: u64) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || widget_music::music_seek_inner(position_ms))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn music_start_watcher(app: tauri::AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || widget_music::start_watcher(app))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
fn music_start_spectrum(app: tauri::AppHandle) -> Result<(), String> {
    widget_music::start_spectrum(app)
}

#[tauri::command]
fn music_stop_spectrum() {
    widget_music::stop_spectrum();
}

// ── Clipboard history ────────────────────────────────────────────────────────

#[tauri::command]
fn clipboard_start(app: tauri::AppHandle) -> Result<(), String> {
    widget_clipboard::start(app)
}

#[tauri::command]
fn clipboard_set(text: String) -> Result<(), String> {
    widget_clipboard::set_text(text)
}

#[tauri::command]
fn show_devtools(app: tauri::AppHandle, label: String) {
    if let Some(window) = app.get_webview_window(&label) {
        window.open_devtools();
    }
}

#[tauri::command]
fn close_devtools(app: tauri::AppHandle, label: String) {
    if let Some(window) = app.get_webview_window(&label) {
        window.close_devtools();
    }
}

/// [WA-03] Where a detached child window (config / connect / layout / picker)
/// must be CREATED so it comes up centered on the monitor the user invoked it
/// from. Pure, so the mixed-DPI arithmetic is testable without a desktop.
///
/// `mon_*` is the target monitor's PHYSICAL rect, `scale` its scale factor,
/// and `w`/`h` the LOGICAL inner size the builder is handed.
///
/// The result is logical because `WebviewWindowBuilder::position` accepts
/// nothing else (`tauri-runtime-wry` wraps it in a `LogicalPosition`). tao
/// resolves that by converting it with EACH connected monitor's own DPI and
/// taking the first monitor whose physical rect contains the result, then sizes
/// the window with that monitor's scale factor. Deriving the logical position
/// from the TARGET monitor's own scale therefore round-trips exactly back to
/// the physical point we asked for, on the monitor we asked for — it is not a
/// "logical desktop" coordinate, which under per-monitor DPI does not exist.
fn detached_window_logical_position(
    mon_x: i32,
    mon_y: i32,
    mon_w: u32,
    mon_h: u32,
    scale: f64,
    w: f64,
    h: f64,
) -> (f64, f64) {
    // Windows does not report a non-finite or non-positive scale factor, but
    // the value crosses an FFI boundary to get here and dividing by it must
    // never hand the builder NaN window coordinates.
    let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
    (
        mon_x as f64 / scale + (mon_w as f64 / scale - w) / 2.0,
        mon_y as f64 / scale + (mon_h as f64 / scale - h) / 2.0,
    )
}

/// [WA-03] The creation position for a detached window opened from
/// `parent_label`, or `None` when the parent (or its monitor) can't be
/// resolved — the caller's signal to fall back to `.center()` on the window it
/// builds.
fn detached_window_placement(
    app: &tauri::AppHandle,
    parent_label: &str,
    w: f64,
    h: f64,
) -> Option<(f64, f64)> {
    let parent = app.get_webview_window(parent_label)?;
    let monitor = parent.current_monitor().ok().flatten()?;
    let mp = monitor.position();
    let ms = monitor.size();
    // The MONITOR's scale factor, not the parent window's: they are the same
    // number only while the parent has settled on that display, and it is the
    // monitor we are placing on.
    let scale = monitor.scale_factor();
    let pos = detached_window_logical_position(mp.x, mp.y, ms.width, ms.height, scale, w, h);
    log::info!(
        target: "window",
        "[WA-03] placing detached window from {parent_label}: monitor ({},{}) {}x{} @{scale} \
         -> logical ({:.1},{:.1})",
        mp.x, mp.y, ms.width, ms.height, pos.0, pos.1,
    );
    Some(pos)
}

/// Static spec for a detached child window (config / connect / layout / layout
/// picker). These four windows are identical but for these fields; one
/// `build_detached_window` call turns a spec into a built, centered, shown +
/// focused window (2.6: one window bootstrap, no copy-pasted builders).
struct DetachedWindowSpec {
    label: &'static str,
    title: &'static str,
    size: (f64, f64),
    min_size: (f64, f64),
    /// Turn off WebView2's built-in drag/drop handler, since it swallows HTML5 DnD
    /// (in-panel reorder handles). Off for config/layout/picker; the connect
    /// window keeps it on (it never needs HTML5 DnD).
    disable_drag_drop: bool,
}

const CONFIG_WINDOW: DetachedWindowSpec = DetachedWindowSpec {
    label: "config", title: "Themia Settings",
    size: (960.0, 720.0), min_size: (640.0, 480.0), disable_drag_drop: true,
};
// Wider than the other dialogs because the picker lays providers out in three
// columns ([S-49 email]); at the old 560px they collapsed to two.
const CONNECT_WINDOW: DetachedWindowSpec = DetachedWindowSpec {
    label: "connect", title: "Connect account",
    size: (760.0, 700.0), min_size: (520.0, 520.0), disable_drag_drop: false,
};
const LAYOUT_WINDOW: DetachedWindowSpec = DetachedWindowSpec {
    label: "layout", title: "Edit app layout",
    size: (900.0, 680.0), min_size: (560.0, 460.0), disable_drag_drop: true,
};
const LAYOUT_PICKER_WINDOW: DetachedWindowSpec = DetachedWindowSpec {
    label: "layout-app-picker", title: "Assign app",
    size: (520.0, 600.0), min_size: (420.0, 480.0), disable_drag_drop: true,
};
const SHORTCUT_ADD_WINDOW: DetachedWindowSpec = DetachedWindowSpec {
    label: "shortcut-add", title: "Add shortcut",
    size: (460.0, 640.0), min_size: (360.0, 420.0), disable_drag_drop: true,
};
const WIDGET_WINDOW: DetachedWindowSpec = DetachedWindowSpec {
    label: "widget-window", title: "Themia",
    size: (380.0, 520.0), min_size: (220.0, 180.0), disable_drag_drop: true,
};

/// Build a detached child window from its spec, centered on the parent's
/// monitor, then show + focus it. Returns the window so a caller can do
/// window-specific follow-up if needed.
fn build_detached_window(
    app: &tauri::AppHandle,
    spec: &DetachedWindowSpec,
    url: String,
    parent_label: &str,
) -> Result<tauri::WebviewWindow, String> {
    // [WA-03] Decided BEFORE the build and handed to the builder, never applied
    // to the finished window: tao places + sizes the window on the target
    // monitor at that monitor's DPI, so nothing has to cross a DPI boundary
    // after the WebView2 controller exists.
    let placement = detached_window_placement(app, parent_label, spec.size.0, spec.size.1);

    let mut builder = WebviewWindowBuilder::new(app, spec.label, WebviewUrl::App(url.into()))
        .additional_browser_args(WEBVIEW_BROWSER_ARGS)
        .title(spec.title)
        .decorations(true)
        .resizable(true)
        .skip_taskbar(false)
        .visible(false)
        .inner_size(spec.size.0, spec.size.1)
        .min_inner_size(spec.min_size.0, spec.min_size.1);
    if let Some((x, y)) = placement {
        builder = builder.position(x, y);
    }
    if spec.disable_drag_drop {
        builder = builder.disable_drag_drop_handler();
    }
    // [WA-02] Creation itself activates: `focused` reaches the WEBVIEW too, and
    // wry's `MoveFocus` during controller build takes the foreground even while
    // the window is still `.visible(false)`. Suppressing only `present_window`
    // would leave that half of the steal in place.
    if no_activate_mode() {
        builder = builder.focused(false);
    }
    let win = builder
        .build()
        .map_err(|e| format!("failed to build {} window: {e}", spec.label))?;

    // [WA-03] Only the degraded case (no resolvable parent monitor) still
    // moves a built window, and `.center()` keeps it on the display it was
    // created on.
    if placement.is_none() {
        let _ = win.center();
    }
    present_window(&win);
    Ok(win)
}

/// Open the unified configuration window (app settings, screen settings,
/// screen-bar settings and widget config, all in one tree-structured window).
/// Only ever one such window exists: re-invoking focuses the existing window
/// and emits `config-navigate` so it jumps to the requested target instead of
/// spawning a duplicate. `target` is a colon-encoded selector
/// (e.g. "widget:clock-123", "screen:default", "app:license"); `None` opens at
/// the default node. Window is centered on the calling window's monitor.
#[tauri::command]
async fn open_config_window(
    app: tauri::AppHandle,
    parent_label: String,
    target: Option<String>,
) -> Result<(), String> {
    show_config_window(&app, &parent_label, target);
    Ok(())
}

/// Everything that must not travel raw inside a query-string VALUE: `#` (which
/// would start the URL fragment), `&` and `=` (which would end the pair), `+`
/// (which `URLSearchParams` decodes back as a space), `%` itself, `\` (which a
/// URL parser may normalise), and the controls/space no URL may carry.
const TARGET_ENCODE_SET: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'&')
    .add(b'+')
    .add(b'/')
    .add(b'<')
    .add(b'=')
    .add(b'>')
    .add(b'?')
    .add(b'\\')
    .add(b'`');

/// The URL a fresh config window opens on. Pure, so the encoding is testable
/// without a shell.
///
/// The target carries a monitor's STABLE hardware id ([MI-01]), which on Windows
/// is a device path — `\\?\DISPLAY#SAM1015#5&2c7ec5a2&0&UID4353#{guid}`. Pasted
/// into the query raw, the first `#` starts the URL FRAGMENT and everything
/// after it is dropped: the window came up on the truncated key `\\?\DISPLAY`
/// and wrote every per-monitor setting there, so the display it was opened FOR
/// never saw any of them. `&` inside the id would have split the pair too.
fn config_window_url(target: Option<&str>) -> String {
    let mut url = "index.html?config=1".to_string();
    if let Some(t) = target.filter(|t| !t.is_empty()) {
        url.push_str("&target=");
        url.push_str(&percent_encoding::utf8_percent_encode(t, TARGET_ENCODE_SET).to_string());
    }
    url
}

/// Core of `open_config_window`, callable from non-command contexts (the
/// `setup()` hook and the single-instance callback, which both need to open
/// the settings window but can't propagate a `Result`). Failures are logged
/// rather than returned.
fn show_config_window(app: &tauri::AppHandle, parent_label: &str, target: Option<String>) {
    let label = "config";

    // Idempotent: focus the existing window and tell it where to navigate.
    if let Some(existing) = app.get_webview_window(label) {
        refocus_window(&existing);
        let _ = existing.emit("config-navigate", &target);
        return;
    }

    let url = config_window_url(target.as_deref());

    // Build, center, track, show + focus (2.6). The drag/drop handler is off for
    // this window (CONFIG_WINDOW.disable_drag_drop): WebView2's built-in handler
    // swallows HTML5 DnD like the shortcut config's reorder handles, and the
    // config window only adds files via the native open() dialog, never OS drop.
    // [ZS-02] It is an ordinary normal-band window from here on: nothing in the
    // app ever re-raises it, widget overlays are demoted instead.
    if let Err(e) = build_detached_window(app, &CONFIG_WINDOW, url, parent_label) {
        log::error!(target: "config", "{e}");
    }
}

/// Open the dedicated "Connect account" window: a focused wizard for adding a
/// connection (pick provider → enter details / OAuth → verify → done). Only one
/// exists at a time: re-invoking focuses the existing window. `provider` (e.g.
/// "imap", "microsoft") pre-selects a provider. Centered on the calling
/// window's monitor.
#[tauri::command]
async fn open_connect_window(
    app: tauri::AppHandle,
    parent_label: String,
    provider: Option<String>,
) -> Result<(), String> {
    let label = "connect";

    if let Some(existing) = app.get_webview_window(label) {
        refocus_window(&existing);
        // Singleton window: re-invoking with a (possibly different) provider must
        // switch the picker instead of stranding the user on the previously
        // chosen provider. Mirrors `open_layout_window`'s navigate emit.
        let _ = existing.emit("connect-navigate", serde_json::json!({ "provider": provider }));
        return Ok(());
    }

    let mut url = "index.html?connect=1".to_string();
    if let Some(p) = provider.as_deref() {
        if !p.is_empty() { url.push_str("&provider="); url.push_str(p); }
    }

    build_detached_window(&app, &CONNECT_WINDOW, url, &parent_label)?;
    Ok(())
}

/// Open the dedicated app-layout editor window for a Shortcut widget's layout
/// item. Singleton: re-invoking focuses the existing window and emits
/// `layout-navigate` so it switches to the requested item instead of spawning a
/// duplicate. Centered on the calling window's monitor. `widget_id` / `view_id`
/// / `item_id` identify which layout item to edit (the editor reads + writes
/// that item's `layout` tree via the shared layout store / localStorage).
#[tauri::command]
async fn open_layout_window(
    app: tauri::AppHandle,
    parent_label: String,
    widget_id: String,
    view_id: String,
    item_id: String,
) -> Result<(), String> {
    let label = "layout";

    if let Some(existing) = app.get_webview_window(label) {
        refocus_window(&existing);
        let _ = existing.emit("layout-navigate", serde_json::json!({
            "widget": widget_id, "view": view_id, "item": item_id,
        }));
        return Ok(());
    }

    // Widget / view / item ids are URL-safe (uuid / `type-timestamp` / `s-…`),
    // so plain concatenation is fine, mirroring `open_config_window`'s target.
    let url = format!(
        "index.html?layout=1&widget={widget_id}&view={view_id}&item={item_id}",
    );

    build_detached_window(&app, &LAYOUT_WINDOW, url, &parent_label)?;
    Ok(())
}

/// Open the "Add shortcut" catalogue window for one Shortcut widget: a list of
/// every kind of shortcut that can be added (link, file, folder, app layout,
/// and the Windows locations). Singleton: re-invoking focuses the existing
/// window and emits `shortcut-add-navigate` so it retargets the requested
/// widget instead of spawning a duplicate. Items are written through the shared
/// per-view layout store, so the widget and config window update live.
#[tauri::command]
async fn open_shortcut_add_window(
    app: tauri::AppHandle,
    parent_label: String,
    widget_id: String,
    view_id: String,
) -> Result<(), String> {
    if let Some(existing) = app.get_webview_window(SHORTCUT_ADD_WINDOW.label) {
        refocus_window(&existing);
        let _ = existing.emit("shortcut-add-navigate", serde_json::json!({
            "widget": widget_id, "view": view_id,
        }));
        return Ok(());
    }

    let url = format!("index.html?shortcutadd=1&widget={widget_id}&view={view_id}");
    build_detached_window(&app, &SHORTCUT_ADD_WINDOW, url, &parent_label)?;
    Ok(())
}

/// Open the dedicated "assign app" picker window for a single layout pane.
/// Singleton: re-invoking focuses the existing window and emits
/// `layout-picker-navigate` so it retargets to the requested leaf. On confirm
/// the picker broadcasts the chosen assignment back to the layout editor window
/// (via the `layout-assign` transient message), keyed by `leaf_id`.
#[tauri::command]
async fn open_layout_app_picker_window(
    app: tauri::AppHandle,
    parent_label: String,
    leaf_id: String,
) -> Result<(), String> {
    let label = "layout-app-picker";

    if let Some(existing) = app.get_webview_window(label) {
        refocus_window(&existing);
        let _ = existing.emit("layout-picker-navigate", serde_json::json!({ "leaf": leaf_id }));
        return Ok(());
    }

    let url = format!("index.html?layoutpicker=1&leaf={leaf_id}");

    build_detached_window(&app, &LAYOUT_PICKER_WINDOW, url, &parent_label)?;
    Ok(())
}

/// Pop a widget out into its own decorated OS window. Singleton: re-invoking
/// focuses the existing window, retitles it, and emits `widget-window-navigate`
/// so it retargets the requested widget instead of spawning a duplicate.
/// Centered on the calling window's monitor. `widget_id` / `view_id` identify
/// which widget to show; the window renders that widget bound to the shared
/// per-view layout store, staying in sync with the grid tile cross-window.
/// `title` is the widget's display label, used for the OS title bar.
#[tauri::command]
async fn open_widget_window(
    app: tauri::AppHandle,
    parent_label: String,
    widget_type: String,
    widget_id: String,
    view_id: String,
    title: String,
) -> Result<(), String> {
    let label = "widget-window";
    let title = if title.trim().is_empty() { "Themia".to_string() } else { title };

    if let Some(existing) = app.get_webview_window(label) {
        refocus_window(&existing);
        let _ = existing.set_title(&title);
        let _ = existing.emit("widget-window-navigate", serde_json::json!({
            "type": widget_type, "widget": widget_id, "view": view_id,
        }));
        return Ok(());
    }

    // Widget type / id / view are URL-safe (`type` / `type-timestamp` / view
    // slug), so plain concatenation is fine, mirroring `open_layout_window`.
    let url = format!(
        "index.html?widgetwindow=1&type={widget_type}&widget={widget_id}&view={view_id}",
    );

    let win = build_detached_window(&app, &WIDGET_WINDOW, url, &parent_label)?;
    let _ = win.set_title(&title);
    Ok(())
}

// ── Web Element widget: live WebView2 picker + off-screen renderer ───────────
//
// Both load a LIVE site in a real WebView2 (executes JavaScript). Page→host
// messages arrive over the native WebView2 channel (`add_WebMessageReceived`)
// from our injected scripts (picker-inject.js / render-inject.js). No Tauri IPC
// is granted to the remote page: only `window.chrome.webview.postMessage`.

const WE_RENDER_INJECT_JS: &str = include_str!("../render-inject.js");
const WE_PICKER_INJECT_JS: &str = include_str!("../picker-inject.js");

/// Serialize render requests through the single off-screen render webview.
#[cfg(target_os = "windows")]
static WE_RENDER_SERIALIZE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
/// Delivery slot: the UI-thread `WebMessageReceived` handler drops the settled
/// HTML here for the awaiting `web_element_render_html` call.
#[cfg(target_os = "windows")]
static WE_RENDER_TX: std::sync::Mutex<Option<std::sync::mpsc::Sender<String>>> =
    std::sync::Mutex::new(None);

#[cfg(target_os = "windows")]
fn we_render_slot() -> std::sync::MutexGuard<'static, Option<std::sync::mpsc::Sender<String>>> {
    WE_RENDER_TX.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(target_os = "windows")]
fn we_handle_render_message(msg: &str) {
    #[derive(serde::Deserialize)]
    struct RenderMsg {
        kind: String,
        html: Option<String>,
    }
    if let Ok(m) = serde_json::from_str::<RenderMsg>(msg) {
        if m.kind == "themia-html" {
            if let Some(tx) = we_render_slot().take() {
                let _ = tx.send(m.html.unwrap_or_default());
            }
        }
    }
}

/// Register the page→host message handler once, on the webview/UI thread. COM
/// FFI is wrapped in `catch_unwind` so a panic can never unwind across it.
#[cfg(target_os = "windows")]
fn we_register_render_handler(pw: tauri::webview::PlatformWebview) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || unsafe {
        if let Ok(core) = pw.controller().CoreWebView2() {
            let handler = webview2_com::WebMessageReceivedEventHandler::create(Box::new(
                move |_wv, args| {
                    if let Some(args) = args {
                        let mut s = windows_core::PWSTR::null();
                        if args.TryGetWebMessageAsString(&mut s).is_ok() {
                            we_handle_render_message(&webview2_com::take_pwstr(s));
                        }
                    }
                    Ok(())
                },
            ));
            let mut token: i64 = 0;
            let _ = core.add_WebMessageReceived(&handler, &mut token);
        }
    }));
}

// ---------------------------------------------------------------------------
// [WV-01] WebView2 process-failure recovery
// ---------------------------------------------------------------------------
//
// A widget overlay is a long-lived window that nobody ever closes or reopens,
// so anything that kills its content is permanent from the user's point of
// view. WebView2 renderers do die in the field — GPU driver resets, the
// 192 MB V8 old-space cap in `WEBVIEW_BROWSER_ARGS`, a WebView2 Runtime that
// auto-updates underneath a running process — and when one does, the overlay
// goes blank or frozen with a live HWND. The existing [DW-06] heal only fires
// when the HWND itself is gone, so it never sees this. Without a handler the
// only cure is restarting Themia ("widgets stop working after a while").

/// What to do about a given `COREWEBVIEW2_PROCESS_FAILED_KIND`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
enum ProcessFailureAction {
    /// The content process is gone but the WebView2 host is intact: a reload
    /// rebuilds it in place.
    Reload,
    /// The whole browser process died. The webview cannot be revived through
    /// its own API; the window has to be rebuilt from scratch.
    RebuildWindow,
    /// A helper process (GPU, utility, sandbox) died. WebView2 restarts these
    /// itself and the page keeps running, so acting would be worse than not.
    LogOnly,
}

/// [WV-01] Pure kind → action mapping, so the policy is testable without a
/// crashed renderer. Takes the raw i32 the COM enum wraps.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn process_failure_action(kind: i32) -> ProcessFailureAction {
    const BROWSER_PROCESS_EXITED: i32 = 0;
    const RENDER_PROCESS_EXITED: i32 = 1;
    const RENDER_PROCESS_UNRESPONSIVE: i32 = 2;
    const FRAME_RENDER_PROCESS_EXITED: i32 = 3;

    match kind {
        BROWSER_PROCESS_EXITED => ProcessFailureAction::RebuildWindow,
        // Unresponsive is recoverable the same way: the process is still there
        // but wedged, and a reload replaces the document and its script state.
        RENDER_PROCESS_EXITED | RENDER_PROCESS_UNRESPONSIVE | FRAME_RENDER_PROCESS_EXITED => {
            ProcessFailureAction::Reload
        }
        _ => ProcessFailureAction::LogOnly,
    }
}

/// [WV-01] Attach a `ProcessFailed` handler to one overlay's WebView2.
///
/// `label` identifies the window in logs and for the rebuild path.
#[cfg(target_os = "windows")]
fn register_process_failed_handler(pw: tauri::webview::PlatformWebview, label: String) {
    // FFI boundary (A2): a panic here would unwind into the WebView2 COM
    // dispatch. Registration and the callback are each wrapped.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || unsafe {
        let core = match pw.controller().CoreWebView2() {
            Ok(c) => c,
            Err(e) => {
                log::error!(target: "displays", "[WV-01] {label}: no CoreWebView2, crash recovery not armed: {e}");
                return;
            }
        };

        let handler_label = label.clone();
        let handler = webview2_com::ProcessFailedEventHandler::create(Box::new(
            move |webview, args| {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    on_webview_process_failed(&handler_label, webview.as_ref(), args.as_ref());
                }));
                Ok(())
            },
        ));

        let mut token: i64 = 0;
        if let Err(e) = core.add_ProcessFailed(&handler, &mut token) {
            log::error!(target: "displays", "[WV-01] {label}: add_ProcessFailed failed: {e}");
        }
    }));
}

/// [WV-01] The recovery itself, split out so the COM callback stays trivial.
#[cfg(target_os = "windows")]
fn on_webview_process_failed(
    label: &str,
    webview: Option<&webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2>,
    args: Option<&webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2ProcessFailedEventArgs>,
) {
    use webview2_com::Microsoft::Web::WebView2::Win32::COREWEBVIEW2_PROCESS_FAILED_KIND;

    // Without the args we cannot tell a fatal failure from a helper restart.
    // Reload is the conservative choice: it is a no-op for a healthy page and
    // the cure for the failures that actually strand a widget.
    let kind = args
        .and_then(|a| unsafe {
            let mut k = COREWEBVIEW2_PROCESS_FAILED_KIND::default();
            a.ProcessFailedKind(&mut k).ok().map(|()| k.0)
        })
        .unwrap_or(1);

    let action = process_failure_action(kind);
    log::error!(
        target: "displays",
        "[WV-01] {label}: WebView2 process failed (kind={kind}) -> {action:?}"
    );

    match action {
        ProcessFailureAction::Reload => {
            match webview {
                Some(wv) => {
                    if let Err(e) = unsafe { wv.Reload() } {
                        log::error!(
                            target: "displays",
                            "[WV-01] {label}: reload after crash failed: {e}; falling back to a \
                             window rebuild"
                        );
                        request_widget_window_rebuild(label);
                    }
                }
                None => request_widget_window_rebuild(label),
            }
        }
        ProcessFailureAction::RebuildWindow => request_widget_window_rebuild(label),
        ProcessFailureAction::LogOnly => {}
    }
}

/// [WV-01] Drop the stranded overlay and let the [DW-06] display pass build a
/// fresh one. Going through the existing pass keeps window construction on its
/// one supported path instead of adding a second one.
#[cfg(target_os = "windows")]
fn request_widget_window_rebuild(label: &str) {
    use tauri::Manager;

    let Some(app) = WATCHER_APP.get() else { return };
    let Some(win) = app.get_webview_window(label) else {
        // Already gone; the pass below will notice the missing overlay.
        request_display_pass();
        return;
    };

    drag_drop::revoke_drop_target(label);
    if let Err(e) = win.destroy() {
        log::error!(target: "displays", "[WV-01] {label}: destroy before rebuild failed: {e:?}");
    }
    // [EX-01] guards the window count reaching zero between the destroy above
    // and the rebuild this schedules.
    request_display_pass();
}

/// The reused off-screen render webview (created lazily). Off-screen but VISIBLE,
/// since a hidden webview throttles JS/timers, so SPA content never settles.
#[cfg(target_os = "windows")]
fn we_ensure_render_webview(app: &tauri::AppHandle) -> Result<tauri::WebviewWindow, String> {
    if let Some(w) = app.get_webview_window("webelement-render") {
        return Ok(w);
    }
    let blank = tauri::WebviewUrl::External(
        "about:blank".parse().map_err(|e: url::ParseError| e.to_string())?,
    );
    let win = tauri::WebviewWindowBuilder::new(app, "webelement-render", blank)
        .initialization_script(WE_RENDER_INJECT_JS)
        .additional_browser_args(WEBVIEW_BROWSER_ARGS)
        .visible(true)
        .focused(false)
        .skip_taskbar(true)
        .decorations(false)
        .inner_size(1280.0, 1600.0)
        .position(-32000.0, -32000.0)
        .build()
        .map_err(|e| format!("render webview build failed: {e}"))?;
    win.with_webview(we_register_render_handler)
        .map_err(|e| e.to_string())?;
    Ok(win)
}

/// Render a live URL with JavaScript in an off-screen WebView2 and return the
/// settled, post-JS `document.documentElement.outerHTML`. The widget then clips
/// to its pinned element. SSRF-guarded and serialized through one reused webview.
#[cfg(target_os = "windows")]
#[tauri::command]
async fn web_element_render_html(app: tauri::AppHandle, url: String) -> Result<String, String> {
    let u = url.clone();
    tauri::async_runtime::spawn_blocking(move || widget_netguard::check_url(&u))
        .await
        .map_err(|e| e.to_string())??;
    let parsed: url::Url = url.parse().map_err(|e: url::ParseError| e.to_string())?;

    let _serialize = WE_RENDER_SERIALIZE.lock().await;

    let (tx, rx) = std::sync::mpsc::channel::<String>();
    *we_render_slot() = Some(tx);

    let win = we_ensure_render_webview(&app)?;
    win.navigate(parsed).map_err(|e| e.to_string())?;

    let result = tauri::async_runtime::spawn_blocking(move || {
        rx.recv_timeout(std::time::Duration::from_secs(12))
    })
    .await
    .map_err(|e| e.to_string())?;

    *we_render_slot() = None;
    result.map_err(|_| "Render timed out, the page didn't load in time".to_string())
}

#[cfg(not(target_os = "windows"))]
#[tauri::command]
async fn web_element_render_html(_app: tauri::AppHandle, _url: String) -> Result<String, String> {
    Err("Web Element rendering is only supported on Windows".into())
}

#[cfg(target_os = "windows")]
fn we_handle_picker_message(app: &tauri::AppHandle, widget_id: &str, url: &str, msg: &str) {
    #[derive(serde::Deserialize)]
    struct PickMsg {
        kind: String,
        selector: Option<String>,
        label: Option<String>,
        /// The page's CURRENT url at pick time, differs from the opening url if
        /// the user navigated (e.g. past a consent gate) before picking.
        url: Option<String>,
    }
    let Ok(m) = serde_json::from_str::<PickMsg>(msg) else {
        return;
    };
    match m.kind.as_str() {
        "themia-picked" => {
            let selector = m.selector.unwrap_or_default();
            let picked_url = m
                .url
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| url.to_string());
            if !selector.is_empty() {
                if let Some(cfg) = app.get_webview_window("config") {
                    let _ = cfg.emit(
                        "web-element-picked",
                        serde_json::json!({
                            "widgetId": widget_id,
                            "url": picked_url,
                            "selector": selector,
                            "pickedLabel": m.label.unwrap_or_default(),
                        }),
                    );
                }
            }
            if let Some(w) = app.get_webview_window("webelement-picker") {
                let _ = w.close();
            }
        }
        "themia-cancel" => {
            if let Some(w) = app.get_webview_window("webelement-picker") {
                let _ = w.close();
            }
        }
        _ => {}
    }
}

#[cfg(target_os = "windows")]
fn we_register_picker_handler(
    pw: tauri::webview::PlatformWebview,
    app: tauri::AppHandle,
    widget_id: String,
    url: String,
) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || unsafe {
        if let Ok(core) = pw.controller().CoreWebView2() {
            let handler = webview2_com::WebMessageReceivedEventHandler::create(Box::new(
                move |_wv, args| {
                    if let Some(args) = args {
                        let mut s = windows_core::PWSTR::null();
                        if args.TryGetWebMessageAsString(&mut s).is_ok() {
                            we_handle_picker_message(
                                &app,
                                &widget_id,
                                &url,
                                &webview2_com::take_pwstr(s),
                            );
                        }
                    }
                    Ok(())
                },
            ));
            let mut token: i64 = 0;
            let _ = core.add_WebMessageReceived(&handler, &mut token);
        }
    }));
}

/// Open the LIVE element-picker window for the Web Element widget. Loads the real
/// site in a WebView2 (JS runs); the injected `picker-inject.js` handles clicks
/// and posts the chosen selector back, which the host emits to the config window
/// as `web-element-picked` then closes the picker. Fresh window per pick.
#[tauri::command]
async fn open_web_element_picker_window(
    app: tauri::AppHandle,
    parent_label: String,
    target_url: String,
    widget_id: String,
) -> Result<(), String> {
    let u = target_url.clone();
    tauri::async_runtime::spawn_blocking(move || widget_netguard::check_url(&u))
        .await
        .map_err(|e| e.to_string())??;
    let parsed: url::Url = target_url
        .parse()
        .map_err(|e: url::ParseError| e.to_string())?;

    if let Some(existing) = app.get_webview_window("webelement-picker") {
        let _ = existing.close();
    }

    // [WA-03] See `build_detached_window`: placement is a builder input, not a
    // post-build move.
    let placement = detached_window_placement(&app, &parent_label, 1100.0, 800.0);

    let mut picker = tauri::WebviewWindowBuilder::new(
        &app,
        "webelement-picker",
        tauri::WebviewUrl::External(parsed),
    )
    .additional_browser_args(WEBVIEW_BROWSER_ARGS)
    .initialization_script(WE_PICKER_INJECT_JS)
    .title("Pick an element")
    .decorations(true)
    .resizable(true)
    .skip_taskbar(false)
    .visible(false)
    .inner_size(1100.0, 800.0)
    .min_inner_size(640.0, 480.0);
    if let Some((x, y)) = placement {
        picker = picker.position(x, y);
    }
    // [WA-02] See `build_detached_window`: creation activates on its own.
    if no_activate_mode() {
        picker = picker.focused(false);
    }
    let win = picker
        .build()
        .map_err(|e| format!("picker build failed: {e}"))?;

    if placement.is_none() {
        let _ = win.center();
    }
    #[cfg(target_os = "windows")]
    {
        let app2 = app.clone();
        let _ = win.with_webview(move |pw| we_register_picker_handler(pw, app2, widget_id, target_url));
    }
    present_window(&win);
    Ok(())
}

/// [ZS-02] Re-anchor a single overlay HWND to `z_above_desktop()` — the
/// "below every real app, above the wallpaper" slot every widget window is
/// supposed to occupy. Used on the activation path, where the overlay is the
/// window that moved and therefore the window that must move back.
#[cfg(target_os = "windows")]
fn anchor_widget_above_desktop(hwnd: winapi::shared::windef::HWND) {
    use winapi::um::winuser::{IsWindow, SetWindowPos, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE};
    if hwnd.is_null() {
        return;
    }
    // SAFETY: live overlay HWND owned by a Tauri window; `IsWindow` rejects a
    // handle that was destroyed between the caller's read and this call.
    unsafe {
        if IsWindow(hwnd) == 0 {
            return;
        }
        SetWindowPos(
            hwnd,
            z_anchor_for(hwnd),
            0, 0, 0, 0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// Per-window diagnostic state used by Win+D / Show Desktop integration tests
/// (and by humans poking at devtools). All fields are best-effort; absent values
/// (e.g. on non-Windows builds) come back as `None` / `0`.
#[derive(serde::Serialize, Default)]
struct WindowDiagnostics {
    hwnd: i64,
    is_visible: bool,
    is_iconic: bool,
    is_cloaked: bool,
    /// 0 = not cloaked, 1 = DWM_CLOAKED_APP, 2 = DWM_CLOAKED_SHELL,
    /// 4 = DWM_CLOAKED_INHERITED. Bitmask in practice.
    cloak_reason: u32,
    /// Window's screen-rect after any Show Desktop transition.
    rect: [i32; 4],
    /// True if our pin_to_desktop subclass is installed on this HWND.
    has_subclass: bool,
    /// Class name of the topmost window at this widget's center point. If the
    /// shell brings Progman/WorkerW to foreground (Show Desktop), our widget
    /// will report visible=true / cloaked=false but be visually obscured,
    /// detectable here as "Progman" or "WorkerW" instead of our class.
    topmost_class_at_center: String,
    /// True iff our HWND is above (closer to top in z-order than) the Progman
    /// shell window. False means Progman covers us: the user perceives the
    /// widget as "minimized" even though it's logically visible.
    above_progman: bool,
    /// True if our foreground hook has flipped widgets to WS_EX_TOPMOST in
    /// response to Show Desktop activating. Used to confirm the hook fired.
    widgets_pinned_topmost: bool,
    /// Current WS_EX style on this widget (hex via toString). 0x8 = WS_EX_TOPMOST.
    ex_style: u32,
}

/// Z-order-aware "what's at this screen point" check. Walks top-level windows
/// from the topmost (HWND_TOPMOST group) down, returning the first visible,
/// not-cloaked window whose rect contains `pt`. Unlike `WindowFromPoint`,
/// this does NOT skip transparent layered windows: themia widgets are
/// click-through layered so `WindowFromPoint` always returns the desktop
/// underneath even when the widget is logically on top.
#[cfg(target_os = "windows")]
fn topmost_class_at_point_zorder(pt: winapi::shared::windef::POINT) -> Option<String> {
    use winapi::shared::windef::{HWND, RECT};
    use winapi::um::dwmapi::DwmGetWindowAttribute;
    use winapi::um::winuser::{
        GetClassNameW, GetDesktopWindow, GetWindow, GetWindowRect, IsIconic,
        IsWindowVisible, GW_CHILD, GW_HWNDNEXT,
    };
    const DWMWA_CLOAKED: u32 = 14;

    unsafe {
        // GW_CHILD on the desktop window gives the topmost top-level window
        // (HWND_TOPMOST group front). GW_HWNDNEXT walks from there toward the
        // bottom of the z-order, returning normal then bottom-most windows.
        let mut cursor: HWND = GetWindow(GetDesktopWindow(), GW_CHILD);
        let mut hops = 0;
        while !cursor.is_null() && hops < 8192 {
            if IsWindowVisible(cursor) != 0 && IsIconic(cursor) == 0 {
                let mut cloaked: u32 = 0;
                DwmGetWindowAttribute(
                    cursor,
                    DWMWA_CLOAKED,
                    &mut cloaked as *mut _ as *mut _,
                    std::mem::size_of::<u32>() as u32,
                );
                if cloaked == 0 {
                    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
                    GetWindowRect(cursor, &mut r);
                    if pt.x >= r.left && pt.x < r.right
                        && pt.y >= r.top  && pt.y < r.bottom
                    {
                        let mut buf = [0u16; 64];
                        let n = GetClassNameW(cursor, buf.as_mut_ptr(), buf.len() as i32);
                        if n > 0 {
                            return Some(String::from_utf16_lossy(&buf[..n as usize]));
                        }
                        return Some(String::new());
                    }
                }
            }
            cursor = GetWindow(cursor, GW_HWNDNEXT);
            hops += 1;
        }
    }
    None
}

#[tauri::command]
fn diag_window_state(app: tauri::AppHandle, label: String) -> WindowDiagnostics {
    #[cfg(target_os = "windows")]
    {
        use std::ptr;
        use winapi::shared::windef::{HWND, POINT, RECT};
        use winapi::um::dwmapi::DwmGetWindowAttribute;
        use winapi::um::winuser::{
            FindWindowW, GetWindow, GetWindowRect, IsIconic, IsWindowVisible,
            GW_HWNDPREV,
        };

        const DWMWA_CLOAKED: u32 = 14;

        let Some(window) = app.get_webview_window(&label) else {
            return WindowDiagnostics::default();
        };
        let Ok(h) = window.hwnd() else {
            return WindowDiagnostics::default();
        };
        let hwnd = h.0 as usize as HWND;
        if hwnd.is_null() {
            return WindowDiagnostics::default();
        }

        let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        let mut cloak_reason: u32 = 0;
        unsafe {
            GetWindowRect(hwnd, &mut rect);
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED,
                &mut cloak_reason as *mut _ as *mut _,
                std::mem::size_of::<u32>() as u32,
            );

            // Walk top-level windows in z-order (HWND_TOPMOST → bottom) and
            // return the first one whose rect contains the widget's center
            // point and which is visible + not cloaked. Unlike WindowFromPoint,
            // this does NOT skip transparent layered windows: themia widgets
            // are click-through layered, so WindowFromPoint always returns the
            // desktop underneath, giving a false "obscured" reading. The
            // z-order walk reflects actual paint order.
            let cx = (rect.left + rect.right) / 2;
            let cy = (rect.top + rect.bottom) / 2;
            let pt = POINT { x: cx, y: cy };

            let topmost_class_at_center =
                topmost_class_at_point_zorder(pt).unwrap_or_default();

            // Z-order check: is Progman strictly below us? Walk GW_HWNDPREV
            // (toward top of z-order) from Progman; if we encounter our HWND,
            // we're above Progman.
            let progman_name: Vec<u16> = "Progman\0".encode_utf16().collect();
            let progman = FindWindowW(progman_name.as_ptr(), ptr::null());
            let mut above_progman = false;
            if !progman.is_null() {
                let mut cursor = GetWindow(progman, GW_HWNDPREV);
                let mut hops = 0;
                while !cursor.is_null() && hops < 4096 {
                    if cursor == hwnd {
                        above_progman = true;
                        break;
                    }
                    cursor = GetWindow(cursor, GW_HWNDPREV);
                    hops += 1;
                }
            }

            WindowDiagnostics {
                hwnd: hwnd as usize as i64,
                is_visible: IsWindowVisible(hwnd) != 0,
                is_iconic: IsIconic(hwnd) != 0,
                is_cloaked: cloak_reason != 0,
                cloak_reason,
                rect: [rect.left, rect.top, rect.right, rect.bottom],
                has_subclass: wndproc_map()
                    .lock()
                    .ok()
                    .map(|m| m.contains_key(&(hwnd as usize)))
                    .unwrap_or(false),
                topmost_class_at_center,
                above_progman,
                widgets_pinned_topmost: WIDGETS_PINNED_TOPMOST
                    .load(std::sync::atomic::Ordering::Relaxed),
                ex_style: winapi::um::winuser::GetWindowLongW(
                    hwnd,
                    winapi::um::winuser::GWL_EXSTYLE,
                ) as u32,
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (app, label);
        WindowDiagnostics::default()
    }
}

/// Invoke the shell's "Show Desktop" action directly via the Shell.Application
/// COM object (IShellDispatch4::ToggleDesktop). This is the exact API the
/// taskbar's Show Desktop button calls, more authoritative than SendInput
/// Win+D, which the shell can ignore for non-UIAccess processes.
///
/// Returns the powershell exit code as a sanity check (0 = success).
#[tauri::command]
fn diag_toggle_desktop() -> i32 {
    #[cfg(target_os = "windows")]
    {
        use std::process::Command;
        let out = Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "(New-Object -ComObject Shell.Application).ToggleDesktop()",
            ])
            .output();
        match out {
            Ok(o) => o.status.code().unwrap_or(-1),
            Err(_) => -1,
        }
    }
    #[cfg(not(target_os = "windows"))]
    { 0 }
}

/// Process-wide z-order telemetry. `topmost_transitions` is monotonically
/// increasing: call `diag_reset_counters` between test phases to measure
/// transitions induced by a specific action (flicker detector).
#[derive(serde::Serialize, Default)]
struct ZOrderCounters {
    topmost_transitions: u32,
    currently_topmost: bool,
    /// [ZS-01] Class of the current foreground window, for diagnosing which
    /// surface a Start-menu replacement actually presents.
    foreground_class: String,
    /// [ZS-01] Whether that foreground window classifies as a transient shell
    /// surface (Start / Search / Task View / taskbar), i.e. whether the pin is
    /// being held rather than dropped.
    shell_surface_foreground: bool,
}

#[tauri::command]
fn diag_z_counters() -> ZOrderCounters {
    #[cfg(target_os = "windows")]
    {
        use std::sync::atomic::Ordering;
        use winapi::um::winuser::{GetClassNameW, GetForegroundWindow};

        let mut foreground_class = String::new();
        unsafe {
            let fg = GetForegroundWindow();
            if !fg.is_null() {
                let mut buf = [0u16; 64];
                let n = GetClassNameW(fg, buf.as_mut_ptr(), buf.len() as i32);
                if n > 0 {
                    foreground_class = String::from_utf16_lossy(&buf[..n as usize]);
                }
            }
        }

        ZOrderCounters {
            topmost_transitions: TOPMOST_TRANSITIONS.load(Ordering::Relaxed),
            currently_topmost: WIDGETS_PINNED_TOPMOST.load(Ordering::Relaxed),
            shell_surface_foreground: shell_surface_is_foreground(),
            foreground_class,
        }
    }
    #[cfg(not(target_os = "windows"))]
    { ZOrderCounters::default() }
}

#[tauri::command]
fn diag_reset_counters() {
    #[cfg(target_os = "windows")]
    {
        use std::sync::atomic::Ordering;
        use winapi::shared::windef::HWND;
        use winapi::um::winuser::{
            IsWindow, SetWindowPos, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
        };

        // First clear the topmost-pin flag without going through
        // `set_widgets_topmost` (which would bump the transition counter we
        // are about to reset, leaving the next test's baseline at 1).
        WIDGETS_PINNED_TOPMOST.store(false, Ordering::Relaxed);

        // Physically re-anchor every widget below normal apps + above the
        // desktop so the next test sees a clean steady state regardless of
        // what the previous test left running through SetWindowPos.
        let anchor = z_above_desktop();
        let hwnds: Vec<usize> = match wndproc_map().lock() {
            Ok(map) => map.keys().copied().collect(),
            Err(_) => Vec::new(),
        };
        for raw in hwnds {
            let hwnd = raw as HWND;
            if hwnd.is_null() { continue; }
            // [PL-01] A pin overlay's clean steady state IS topmost.
            if hwnd_is_pin_layer(hwnd) { continue; }
            unsafe {
                if IsWindow(hwnd) == 0 { continue; }
                SetWindowPos(
                    hwnd,
                    anchor,
                    0, 0, 0, 0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }

        // Reset the counter LAST so any earlier flips above don't leave a
        // non-zero baseline.
        TOPMOST_TRANSITIONS.store(0, Ordering::Relaxed);
    }
}

/// Result of a forced SetWindowPos(HWND_TOPMOST) on a specific widget. Tests
/// use this to confirm whether the topmost flip is actually landing per-window
/// (vs. silently no-op'ing on secondary monitors).
#[derive(serde::Serialize, Default)]
struct ForceTopmostResult {
    succeeded_pass1: bool,
    ex_style_after_pass1: u32,
    succeeded_pass2: bool,
    ex_style_after_pass2: u32,
    last_error: u32,
}

#[tauri::command]
fn diag_force_topmost(app: tauri::AppHandle, label: String) -> ForceTopmostResult {
    #[cfg(target_os = "windows")]
    {
        use std::sync::atomic::Ordering;
        use winapi::shared::windef::HWND;
        use winapi::um::winuser::{
            GetWindowLongPtrW, GetWindowLongW, SetWindowLongPtrW, SetWindowLongW,
            SetWindowPos, GWLP_WNDPROC, GWL_EXSTYLE, HWND_TOPMOST,
            SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, WS_EX_TOPMOST,
        };
        extern "system" { fn GetLastError() -> u32; }

        let Some(window) = app.get_webview_window(&label) else { return ForceTopmostResult::default() };
        let Ok(h) = window.hwnd() else { return ForceTopmostResult::default() };
        let hwnd = h.0 as usize as HWND;
        if hwnd.is_null() { return ForceTopmostResult::default() }

        // Mirror the runtime "Show Desktop active" state so our own wnd_proc
        // subclass doesn't override hwndInsertAfter back to Progman.
        let prev_flag = WIDGETS_PINNED_TOPMOST.swap(true, Ordering::Relaxed);

        // Temporarily uninstall our subclass so we can isolate whether the
        // topmost rejection is from OUR subclass or from a downstream Tauri
        // / WebView2 wndproc that we're forwarding through.
        let orig = wndproc_map()
            .lock().ok()
            .and_then(|m| m.get(&(hwnd as usize)).copied())
            .unwrap_or(0);
        let our_proc = unsafe { GetWindowLongPtrW(hwnd, GWLP_WNDPROC) };
        if orig != 0 {
            unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, orig); }
        }

        let result = unsafe {
            let ok1 = SetWindowPos(
                hwnd, HWND_TOPMOST,
                0, 0, 0, 0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            ) != 0;
            let err1 = if ok1 { 0 } else { GetLastError() };
            let ex1 = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;

            let ex_with = ex1 | WS_EX_TOPMOST;
            SetWindowLongW(hwnd, GWL_EXSTYLE, ex_with as i32);
            let ok2 = SetWindowPos(
                hwnd, HWND_TOPMOST,
                0, 0, 0, 0,
                SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            ) != 0;
            let ex2 = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;

            ForceTopmostResult {
                succeeded_pass1: ok1,
                ex_style_after_pass1: ex1,
                succeeded_pass2: ok2,
                ex_style_after_pass2: ex2,
                last_error: err1,
            }
        };

        // Re-install our subclass.
        if orig != 0 {
            unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, our_proc); }
        }
        WIDGETS_PINNED_TOPMOST.store(prev_flag, Ordering::Relaxed);
        result
    }
    #[cfg(not(target_os = "windows"))]
    { let _ = (app, label); ForceTopmostResult::default() }
}

/// One-line summary per widget surface, used by multi-monitor specs to assert
/// every monitor's widget tracks correctly. The spec runs through this list
/// on the actual user rig; monitor-1, monitor-2, etc. only exist when the
/// user has multiple displays attached.
#[derive(serde::Serialize, Default)]
struct WidgetSummary {
    label: String,
    hwnd: i64,
    has_subclass: bool,
    above_progman: bool,
    is_topmost: bool,
    is_visible: bool,
    is_iconic: bool,
    is_cloaked: bool,
    rect: [i32; 4],
    /// GetWindow(hwnd, GW_OWNER). Owned windows inherit topmost state from
    /// their owner: if this is non-zero and the owner isn't topmost, our
    /// SetWindowPos(HWND_TOPMOST) will be silently rejected.
    owner_hwnd: i64,
    /// True iff the owner (if any) currently has WS_EX_TOPMOST set.
    owner_is_topmost: bool,
    /// Full WS_EX style (hex). Tells us if WS_EX_NOACTIVATE / WS_EX_TOOLWINDOW
    /// / WS_EX_LAYERED differ between monitors.
    ex_style: u32,
    /// Full WS style (hex). WS_POPUP, WS_VISIBLE, WS_CHILD etc.
    style: u32,
    /// Window class name. If different between monitors, WNDCLASS-level
    /// behaviour could differ (e.g. CS_NOCLOSE etc.).
    class_name: String,
    /// GetParent, distinct from GW_OWNER. Tells us if Tauri parented this
    /// widget under another HWND.
    parent_hwnd: i64,
    /// Process ID owning this HWND. Tauri/WebView2 sometimes spawn separate
    /// hosts; if different from our own PID, that explains a lot.
    process_id: u32,
}

#[tauri::command]
fn diag_list_widgets(app: tauri::AppHandle) -> Vec<WidgetSummary> {
    #[cfg(target_os = "windows")]
    {
        use winapi::shared::windef::{HWND, RECT};
        use winapi::um::dwmapi::DwmGetWindowAttribute;
        use winapi::um::winuser::{
            GetClassNameW, GetParent, GetWindow, GetWindowLongW, GetWindowRect,
            GetWindowThreadProcessId, IsIconic, IsWindowVisible,
            GWL_EXSTYLE, GWL_STYLE, GW_OWNER, WS_EX_TOPMOST,
        };
        const DWMWA_CLOAKED: u32 = 14;

        let mut out = Vec::new();
        for window in app.webview_windows().values() {
            let label = window.label().to_string();
            if !label.starts_with("monitor-") {
                continue;
            }
            let Ok(h) = window.hwnd() else { continue };
            let hwnd = h.0 as usize as HWND;
            if hwnd.is_null() { continue }

            let has_subclass = wndproc_map()
                .lock()
                .ok()
                .map(|m| m.contains_key(&(hwnd as usize)))
                .unwrap_or(false);

            let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            let mut cloak_reason: u32 = 0;
            unsafe {
                GetWindowRect(hwnd, &mut rect);
                DwmGetWindowAttribute(
                    hwnd, DWMWA_CLOAKED,
                    &mut cloak_reason as *mut _ as *mut _,
                    std::mem::size_of::<u32>() as u32,
                );
                let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
                let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
                let owner = GetWindow(hwnd, GW_OWNER);
                let owner_topmost = if owner.is_null() {
                    false
                } else {
                    let owner_ex = GetWindowLongW(owner, GWL_EXSTYLE) as u32;
                    (owner_ex & WS_EX_TOPMOST) != 0
                };
                let mut class_buf = [0u16; 64];
                let class_n = GetClassNameW(hwnd, class_buf.as_mut_ptr(), class_buf.len() as i32);
                let class_name = if class_n > 0 {
                    String::from_utf16_lossy(&class_buf[..class_n as usize])
                } else {
                    String::new()
                };
                let parent = GetParent(hwnd);
                let mut pid: u32 = 0;
                GetWindowThreadProcessId(hwnd, &mut pid);

                out.push(WidgetSummary {
                    label,
                    hwnd: hwnd as usize as i64,
                    has_subclass,
                    above_progman: is_widget_above_progman(hwnd),
                    is_topmost: (ex & WS_EX_TOPMOST) != 0,
                    is_visible: IsWindowVisible(hwnd) != 0,
                    is_iconic: IsIconic(hwnd) != 0,
                    is_cloaked: cloak_reason != 0,
                    rect: [rect.left, rect.top, rect.right, rect.bottom],
                    owner_hwnd: owner as usize as i64,
                    owner_is_topmost: owner_topmost,
                    ex_style: ex,
                    style,
                    class_name,
                    parent_hwnd: parent as usize as i64,
                    process_id: pid,
                });
            }
        }
        // Stable order makes test failure messages easier to read.
        out.sort_by(|a, b| a.label.cmp(&b.label));
        out
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = app;
        Vec::new()
    }
}

/// Test-only: pause/resume the z-order polling thread so specs can observe
/// transient demote states without the safeguard restoring mid-test.
#[tauri::command]
fn diag_set_polling_paused(paused: bool) {
    #[cfg(target_os = "windows")]
    POLLING_PAUSED.store(paused, std::sync::atomic::Ordering::Relaxed);
    #[cfg(not(target_os = "windows"))]
    let _ = paused;
}

/// Result of querying z-order between our primary widget and an external HWND
/// looked up by class name. Used to test "apps can come above the widget after
/// Show Desktop is dismissed".
#[derive(serde::Serialize, Default)]
struct ZComparison {
    widget_hwnd: i64,
    other_hwnd: i64,
    other_found: bool,
    /// True iff `other_hwnd` is strictly higher in z-order than the widget
    /// (the widget would NOT cover it).
    other_above_widget: bool,
}

/// Outcome of a demote-and-read sequence. `above_progman_immediately_after`
/// is read synchronously after SetWindowPos returns, so the polling thread
/// cannot interleave and mask the demotion.
#[derive(serde::Serialize, Default)]
struct DemoteResult {
    succeeded: bool,
    above_progman_immediately_after: bool,
}

/// [ZC-01] Forcibly DWM-cloak a widget window, reproducing what Show Desktop
/// and a fullscreen exclusive app do to it. Returns whether the cloak actually
/// landed (read back synchronously, before any healing path can run), so a test
/// can tell "the cloak never applied" apart from "the cloak applied and was
/// healed". Recovery is then the app's job: the `EVENT_OBJECT_CLOAKED` hook
/// normally, and `ensure_widget_cloak_tick` when that event is dropped.
#[tauri::command]
fn diag_cloak_widget(app: tauri::AppHandle, label: String) -> bool {
    #[cfg(target_os = "windows")]
    {
        use winapi::shared::windef::HWND;
        use winapi::um::dwmapi::{DwmGetWindowAttribute, DwmSetWindowAttribute};
        const DWMWA_CLOAK: u32 = 13;
        const DWMWA_CLOAKED: u32 = 14;

        let Some(window) = app.get_webview_window(&label) else { return false };
        let Ok(h) = window.hwnd() else { return false };
        let hwnd = h.0 as usize as HWND;
        if hwnd.is_null() {
            return false;
        }
        unsafe {
            let cloak: u32 = 1;
            if DwmSetWindowAttribute(
                hwnd,
                DWMWA_CLOAK,
                &cloak as *const _ as *const _,
                std::mem::size_of::<u32>() as u32,
            ) != 0
            {
                return false;
            }
            let mut cloaked: u32 = 0;
            if DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED,
                &mut cloaked as *mut u32 as *mut _,
                std::mem::size_of::<u32>() as u32,
            ) != 0
            {
                return false;
            }
            cloaked != 0
        }
    }
    #[cfg(not(target_os = "windows"))]
    { let _ = (app, label); false }
}

/// Forcibly demote the widget HWND to HWND_BOTTOM, simulating the failure
/// mode the user sees on secondary monitors where Show Desktop covers the
/// widget despite our event-driven hook. Bypasses the WndProc subclass by
/// temporarily uninstalling it so the demotion actually lands; the polling
/// safeguard then has the job of detecting and restoring z-order.
#[tauri::command]
fn diag_demote_widget(app: tauri::AppHandle, label: String) -> DemoteResult {
    #[cfg(target_os = "windows")]
    {
        use winapi::shared::windef::HWND;
        use winapi::um::winuser::{
            CallWindowProcW, GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos,
            GWLP_WNDPROC, HWND_BOTTOM, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
        };

        let Some(window) = app.get_webview_window(&label) else { return DemoteResult::default() };
        let Ok(h) = window.hwnd() else { return DemoteResult::default() };
        let hwnd = h.0 as usize as HWND;
        if hwnd.is_null() { return DemoteResult::default() }

        unsafe {
            // Restore the original wndproc (captured when pin_to_desktop ran),
            // perform the demote, then re-install our subclass. Without this
            // the subclass would catch WM_WINDOWPOSCHANGING and override
            // hwndInsertAfter back to "above progman".
            let orig = wndproc_map()
                .lock()
                .ok()
                .and_then(|m| m.get(&(hwnd as usize)).copied())
                .unwrap_or(0);

            let our_proc = GetWindowLongPtrW(hwnd, GWLP_WNDPROC);
            if orig != 0 {
                SetWindowLongPtrW(hwnd, GWLP_WNDPROC, orig);
            }
            // Also clear topmost so the demote isn't fighting WS_EX_TOPMOST.
            WIDGETS_PINNED_TOPMOST.store(false, std::sync::atomic::Ordering::Relaxed);
            SetWindowPos(
                hwnd,
                HWND_BOTTOM,
                0, 0, 0, 0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
            // Re-install our subclass so the polling thread's restoration
            // routes back through it (matches normal runtime state).
            if orig != 0 {
                SetWindowLongPtrW(hwnd, GWLP_WNDPROC, our_proc);
            }
            // Touch CallWindowProcW so the unused-import warning doesn't bite
            // (we reference it for clarity even if not called here).
            let _ = CallWindowProcW;

            // Read z-order synchronously, BEFORE returning to JS, otherwise
            // the polling thread (every 200ms) might restore the demote
            // before the test can observe it.
            DemoteResult {
                succeeded: true,
                above_progman_immediately_after: is_widget_above_progman(hwnd),
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    { let _ = (app, label); DemoteResult::default() }
}

/// Close all webview windows whose label starts with the given prefix. Used
/// by tests to clean up spawned detached config windows between specs.
#[tauri::command]
fn diag_close_windows_by_prefix(app: tauri::AppHandle, prefix: String) -> Vec<String> {
    let mut closed = Vec::new();
    let labels: Vec<String> = app.webview_windows().keys().cloned().collect();
    for label in labels {
        if label.starts_with(&prefix) {
            if let Some(w) = app.get_webview_window(&label) {
                if w.close().is_ok() {
                    closed.push(label);
                }
            }
        }
    }
    closed
}

/// Report whether `top_label` is strictly above `bottom_label` in the global
/// z-order. Used by tests to verify a detached config window remains above
/// its parent widget window after right-clicking the widget.
#[tauri::command]
fn diag_window_above(
    app: tauri::AppHandle,
    top_label: String,
    bottom_label: String,
) -> bool {
    #[cfg(target_os = "windows")]
    {
        use winapi::shared::windef::HWND;
        use winapi::um::winuser::{GetWindow, GW_HWNDPREV};

        let Some(top_w) = app.get_webview_window(&top_label) else { return false };
        let Some(bot_w) = app.get_webview_window(&bottom_label) else { return false };
        let Ok(top_h) = top_w.hwnd() else { return false };
        let Ok(bot_h) = bot_w.hwnd() else { return false };
        let top = top_h.0 as usize as HWND;
        let bot = bot_h.0 as usize as HWND;
        if top.is_null() || bot.is_null() { return false; }

        // Walk upward (GW_HWNDPREV, toward top of z-order) from `bot`. If we
        // encounter `top` before exhausting the chain, top is above bot.
        let mut cursor = unsafe { GetWindow(bot, GW_HWNDPREV) };
        let mut hops = 0;
        while !cursor.is_null() && hops < 8192 {
            if cursor == top { return true; }
            cursor = unsafe { GetWindow(cursor, GW_HWNDPREV) };
            hops += 1;
        }
        false
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (app, top_label, bottom_label);
        false
    }
}

/// Locate a foreign window by class name (substring) and report its z-order
/// relative to the primary widget. Used by tests to verify that real app
/// windows can sit above our widget, i.e. our WS_EX_TOPMOST flip during
/// Show Desktop has been properly reverted.
#[tauri::command]
fn diag_compare_zorder(
    app: tauri::AppHandle,
    widget_label: String,
    other_class: String,
) -> ZComparison {
    #[cfg(target_os = "windows")]
    {
        use std::ptr;
        use winapi::shared::windef::HWND;
        use winapi::um::winuser::{FindWindowW, GetWindow, GW_HWNDPREV};

        let Some(window) = app.get_webview_window(&widget_label) else {
            return ZComparison::default();
        };
        let Ok(h) = window.hwnd() else {
            return ZComparison::default();
        };
        let widget = h.0 as usize as HWND;
        if widget.is_null() {
            return ZComparison::default();
        }

        let class_w: Vec<u16> =
            other_class.encode_utf16().chain(std::iter::once(0)).collect();
        let other = unsafe { FindWindowW(class_w.as_ptr(), ptr::null()) };
        if other.is_null() {
            return ZComparison {
                widget_hwnd: widget as usize as i64,
                ..Default::default()
            };
        }

        // Walk z-order upward (GW_HWNDPREV) from the widget. If we reach `other`
        // before NULL, `other` is above the widget.
        let mut other_above_widget = false;
        let mut cursor = unsafe { GetWindow(widget, GW_HWNDPREV) };
        let mut hops = 0;
        while !cursor.is_null() && hops < 8192 {
            if cursor == other {
                other_above_widget = true;
                break;
            }
            cursor = unsafe { GetWindow(cursor, GW_HWNDPREV) };
            hops += 1;
        }

        ZComparison {
            widget_hwnd: widget as usize as i64,
            other_hwnd: other as usize as i64,
            other_found: true,
            other_above_widget,
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (app, widget_label, other_class);
        ZComparison::default()
    }
}

/// Synthesise the Win+D ("Show Desktop") chord via SendInput so e2e tests can
/// trigger the same shell behavior the user does. Pressing it again toggles
/// back. No-op on non-Windows.
#[tauri::command]
fn diag_send_show_desktop() {
    #[cfg(target_os = "windows")]
    unsafe {
        use winapi::um::winuser::{
            SendInput, INPUT, INPUT_KEYBOARD, KEYEVENTF_KEYUP,
            VK_LWIN,
        };

        const VK_D: u16 = 0x44;

        fn key(vk: u16, up: bool) -> INPUT {
            let mut input: INPUT = unsafe { std::mem::zeroed() };
            input.type_ = INPUT_KEYBOARD;
            unsafe {
                let ki = input.u.ki_mut();
                ki.wVk = vk;
                ki.dwFlags = if up { KEYEVENTF_KEYUP } else { 0 };
            }
            input
        }

        let mut inputs = [
            key(VK_LWIN as u16, false),
            key(VK_D, false),
            key(VK_D, true),
            key(VK_LWIN as u16, true),
        ];
        SendInput(
            inputs.len() as u32,
            inputs.as_mut_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        );
    }
}

/// Diagnostic: simulate a real OS left-click at physical screen (x, y) and
/// return the time in ms between the click and Start/Search no longer being
/// foreground. `None` if Start was not foreground at call time, or if it
/// didn't close before `timeout_ms`. Used by the start-menu-dismiss latency
/// e2e spec to measure end-to-end click→close time without driving
/// PowerShell from Node (which adds ~150ms of process-startup noise).
#[tauri::command]
fn diag_click_and_measure_start_close(x: i32, y: i32, timeout_ms: u64) -> Option<u64> {
    #[cfg(target_os = "windows")]
    unsafe {
        use winapi::um::winuser::{
            SendInput, SetCursorPos, INPUT, INPUT_MOUSE,
            MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
        };

        if !start_is_foreground() {
            return None;
        }

        SetCursorPos(x, y);

        fn mouse(flags: u32) -> INPUT {
            let mut input: INPUT = unsafe { std::mem::zeroed() };
            input.type_ = INPUT_MOUSE;
            unsafe {
                let mi = input.u.mi_mut();
                mi.dwFlags = flags;
            }
            input
        }
        let mut inputs = [
            mouse(MOUSEEVENTF_LEFTDOWN),
            mouse(MOUSEEVENTF_LEFTUP),
        ];
        let t0 = std::time::Instant::now();
        SendInput(
            inputs.len() as u32,
            inputs.as_mut_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        );

        let deadline = std::time::Duration::from_millis(timeout_ms);
        while t0.elapsed() < deadline {
            if !start_is_foreground() {
                return Some(t0.elapsed().as_millis() as u64);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        None
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (x, y, timeout_ms);
        None
    }
}

#[tauri::command]
fn set_interactive(app: tauri::AppHandle, label: String, interactive: bool) {
    #[cfg(target_os = "windows")]
    {
        use std::sync::atomic::Ordering;
        use winapi::shared::windef::HWND;
        use winapi::um::winuser::{
            GetWindowLongW, SetWindowLongW, SetWindowPos,
            GWL_EXSTYLE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
            WS_EX_NOACTIVATE,
        };

        if let Some(window) = app.get_webview_window(&label) {
            if let Ok(h) = window.hwnd() {
                let hwnd = h.0 as usize as HWND;
                // Per-window interactive state, keyed by this window's HWND
                // so a second monitor's overlay is unaffected.
                set_window_interactive(hwnd as usize, interactive);
                unsafe {
                    let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
                    if interactive {
                        // Remove WS_EX_NOACTIVATE so the webview can accept keyboard input
                        SetWindowLongW(hwnd, GWL_EXSTYLE, (ex & !WS_EX_NOACTIVATE) as i32);
                        // Clear the widget-shape region so the entire window
                        // is interactive (drop target for paste/drag, modal
                        // backdrops, etc.). Region is re-applied when
                        // interactive flips back to false.
                        apply_widget_region(hwnd, false);
                    } else {
                        // Re-add WS_EX_NOACTIVATE and push back to "just above
                        // Progman" so the widget is below all real apps but
                        // above the shell desktop (survives Show Desktop).
                        SetWindowLongW(hwnd, GWL_EXSTYLE, (ex | WS_EX_NOACTIVATE) as i32);
                        SetWindowPos(hwnd, z_anchor_for(hwnd), 0, 0, 0, 0,
                            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
                        // Restore the widget-shape region so empty areas of
                        // the overlay pass clicks through to the desktop.
                        apply_widget_region(hwnd, true);
                        // Clear folder keyboard hook when going non-interactive
                        WIDGET_KB_ACTIVE.store(false, Ordering::Relaxed);
                    }
                }
            }
        }
    }
}

// Cached HWND of the desktop icon list-view (stored as usize for AtomicUsize).
// [DI-02] state source, and the target of the LVM_SETITEMSTATE deselect.
#[cfg(target_os = "windows")]
static DESKTOP_ICONS_HWND: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

// [DI-01] Cached HWND of the list-view's SHELLDLL_DefView parent — the window
// that owns the shell's "toggle desktop icons" command. Reset on TaskbarCreated.
#[cfg(target_os = "windows")]
static DESKTOP_DEFVIEW_HWND: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

// Cached HWND of the shell's Progman window. Used as our z-order anchor:
// widgets are pinned just above Progman so Show Desktop (which raises
// Progman to foreground) cannot visually obscure them. Reset on TaskbarCreated.
#[cfg(target_os = "windows")]
static PROGMAN_HWND: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Returns the z-order anchor we want our widget to sit just above. This is
/// Progman (the shell's desktop window). When Show Desktop is invoked, the
/// shell raises Progman to the top of the z-order; by inserting ourselves
/// directly above Progman we travel up with it instead of being covered.
///
/// Falls back to `HWND_BOTTOM` if Progman cannot be found (e.g. early in
/// boot before explorer.exe is up). Cached and validated with `IsWindow`.
#[cfg(target_os = "windows")]
fn z_above_desktop() -> winapi::shared::windef::HWND {
    use std::ptr;
    use std::sync::atomic::Ordering;
    use winapi::shared::windef::HWND;
    use winapi::um::winuser::{FindWindowW, IsWindow, HWND_BOTTOM};

    let cached = PROGMAN_HWND.load(Ordering::Relaxed);
    if cached != 0 {
        let h = cached as HWND;
        if unsafe { IsWindow(h) } != 0 {
            return h;
        }
        PROGMAN_HWND.store(0, Ordering::Relaxed);
    }

    let name: Vec<u16> = "Progman\0".encode_utf16().collect();
    unsafe {
        let h = FindWindowW(name.as_ptr(), ptr::null());
        if !h.is_null() {
            PROGMAN_HWND.store(h as usize, Ordering::Relaxed);
            return h;
        }
    }
    HWND_BOTTOM
}

// Stores the original WndProc for each widget window so our subclass can forward messages
#[cfg(target_os = "windows")]
static ORIGINAL_WNDPROC: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<usize, isize>>,
> = std::sync::OnceLock::new();

#[cfg(target_os = "windows")]
fn wndproc_map() -> &'static std::sync::Mutex<std::collections::HashMap<usize, isize>> {
    ORIGINAL_WNDPROC.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

// Per-window interactive client-space rectangles (physical pixels). Each
// entry is (x, y, w, h, corner_radius), built into an HRGN that becomes the
// window's hit-test + visible shape via SetWindowRgn. Areas of the window
// outside the region are literally not part of the window: mouse input,
// painting, hit-testing all skip them, so the Windows desktop / Explorer
// underneath receives clicks for free. Pushed by the frontend on every
// layout change; keyed by HWND.
#[cfg(target_os = "windows")]
static WIDGET_RECTS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<usize, Vec<ShapedRect>>>,
> = std::sync::OnceLock::new();

/// A widget / chrome rectangle plus its corner radius (physical px). Used to
/// build the per-window HRGN. Radius 0 → sharp rectangle.
#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug)]
pub(crate) struct ShapedRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub radius: i32,
}

#[cfg(target_os = "windows")]
fn widget_rects_map()
    -> &'static std::sync::Mutex<std::collections::HashMap<usize, Vec<ShapedRect>>>
{
    WIDGET_RECTS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// True when client-area point (`x`, `y`, physical px relative to the window
/// client origin) falls inside any of the window's pushed widget rects.
///
/// Used by the OLE drop target to decide "is this drop over a widget?" without
/// depending on the async `set_drag_over_zone` round-trip: that flag is set by
/// a JS invoke responding to DragOver, so a fast drag→drop can reach `Drop()`
/// before it lands, leaving `over_zone` stale-false and a file drop forwarded
/// to the desktop. The rects map is the same source of truth that shapes the
/// window region, so a point inside it is, by definition, over a widget.
#[cfg(target_os = "windows")]
pub(crate) fn point_in_widget_rect(hwnd_usize: usize, x: i32, y: i32) -> bool {
    widget_rects_map()
        .lock()
        .ok()
        .and_then(|m| m.get(&hwnd_usize).map(|rects| {
            rects.iter().any(|r| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
        }))
        .unwrap_or(false)
}

/// Build a region as the union of the input shaped rectangles. Caller takes
/// ownership of the returned HRGN: must hand it to `SetWindowRgn` (which
/// transfers ownership) or `DeleteObject` it. Returns null if `rects` is
/// empty (empty regions break SetWindowRgn, so the caller passes null to clear).
#[cfg(target_os = "windows")]
unsafe fn build_widget_region(rects: &[ShapedRect]) -> winapi::shared::minwindef::HRGN {
    use std::ptr;
    use winapi::shared::minwindef::HRGN;
    use winapi::um::wingdi::{
        CombineRgn, CreateRectRgn, CreateRoundRectRgn, DeleteObject, RGN_OR,
    };

    if rects.is_empty() {
        return ptr::null_mut();
    }

    let mut acc: HRGN = ptr::null_mut();
    for r in rects {
        // Skip degenerate (zero-area) rects, they'd add nothing to the
        // region and CreateRoundRectRgn dislikes them.
        if r.right <= r.left || r.bottom <= r.top {
            continue;
        }
        // Add 1 to the round-rect bounds: Windows GDI treats the right /
        // bottom edges as exclusive for CreateRectRgn but inclusive for
        // CreateRoundRectRgn, so passing the same coords makes the rounded
        // version one pixel smaller on the right and bottom. Add 1 to keep
        // both code paths visually identical.
        let piece: HRGN = if r.radius > 0 {
            CreateRoundRectRgn(
                r.left, r.top, r.right + 1, r.bottom + 1,
                r.radius * 2, r.radius * 2,
            )
        } else {
            CreateRectRgn(r.left, r.top, r.right, r.bottom)
        };
        if piece.is_null() {
            continue;
        }
        if acc.is_null() {
            acc = piece;
        } else {
            // CombineRgn destination must be a pre-existing region. Combine
            // acc | piece into acc; then delete piece.
            CombineRgn(acc, acc, piece, RGN_OR);
            DeleteObject(piece as *mut _);
        }
    }
    acc
}

/// Cancel any popup menu currently open in the foreground thread, e.g. the
/// Explorer desktop context menu the user opened by right-clicking a gap
/// between widgets.
///
/// Normally clicking any window dismisses such a menu, but themia's overlay
/// is WS_EX_NOACTIVATE and returns MA_NOACTIVATE, so it never changes the
/// foreground window, so the shell menu's modal loop never notices the click
/// and stays open. When the user clicks (or right-clicks) into a widget we
/// explicitly post WM_CANCELMODE to the menu's owner, which ends the menu
/// the same way an outside click on a normal window would.
#[cfg(target_os = "windows")]
unsafe fn dismiss_foreign_menu() {
    use winapi::um::winuser::{GetGUIThreadInfo, PostMessageW, GUITHREADINFO};

    const GUI_INMENUMODE: u32 = 0x0000_0004;
    const WM_CANCELMODE: u32  = 0x001F;

    let mut gti: GUITHREADINFO = std::mem::zeroed();
    gti.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
    // Thread id 0 → the current foreground thread. If a shell menu is up,
    // that thread is in menu mode and hwndMenuOwner is the menu's owner.
    if GetGUIThreadInfo(0, &mut gti) == 0 {
        return;
    }
    if gti.flags & GUI_INMENUMODE != 0 && !gti.hwndMenuOwner.is_null() {
        PostMessageW(gti.hwndMenuOwner, WM_CANCELMODE, 0, 0);
    }
}

/// Which clip `apply_widget_region` should install for the current pushed
/// rect state. Pure decision kept standalone so it can be unit-tested
/// without a live HWND (mirrors `decide_refuse_activation`).
///
/// - `Full`:  no rect push received yet (startup), so leave the window
///   un-clipped; a brief full transparent cover beats an invisible window.
/// - `Empty`: the frontend explicitly pushed zero interactive rects (a
///   zero-widget view with no visible chrome), so install an EMPTY region so
///   the overlay has no hit-test area and the desktop receives all input,
///   including right-click ([S-18], nwalser/themia-releases#34). A NULL
///   clip here would make the whole transparent overlay swallow mouse
///   input indefinitely.
/// - `Rects`: clip to the union of the pushed rects.
#[cfg(target_os = "windows")]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum RegionClip { Full, Empty, Rects }

#[cfg(target_os = "windows")]
pub(crate) fn decide_region_clip(pushed: Option<&[ShapedRect]>) -> RegionClip {
    match pushed {
        None => RegionClip::Full,
        // Degenerate (zero-area) rects build no region piece, so a list of
        // only-degenerate rects is an empty push too.
        Some(rects) if rects.iter().any(|r| r.right > r.left && r.bottom > r.top) => {
            RegionClip::Rects
        }
        Some(_) => RegionClip::Empty,
    }
}

/// Apply the widget region to the given overlay window. The HRGN is built
/// from the current `widget_rects_map` entry for this HWND. SetWindowRgn
/// transfers ownership of the region; we must not free it ourselves.
///
/// `clip_to_rects = false` applies a NULL region, clearing any previous
/// clip, restoring the full window as a hit-test target. We use this in
/// `set_interactive(true)` (edit mode / modal) so the whole window can be
/// dragged into / dropped onto. With `clip_to_rects = true` the clip is
/// chosen by `decide_region_clip` (see its cases).
#[cfg(target_os = "windows")]
unsafe fn apply_widget_region(hwnd: winapi::shared::windef::HWND, clip_to_rects: bool) {
    use std::ptr;
    use winapi::shared::minwindef::TRUE;
    use winapi::um::wingdi::CreateRectRgn;
    use winapi::um::winuser::SetWindowRgn;

    if !clip_to_rects {
        // Full window, no clipping. SetWindowRgn(null) removes any region.
        SetWindowRgn(hwnd, ptr::null_mut(), TRUE);
        // The region was the only thing clipping away any residual OS frame;
        // with it gone the frame can flash back as a grey "top bar". Re-assert
        // the frameless invariant now that the full window is visible.
        reassert_frameless(hwnd);
        return;
    }

    let rects: Option<Vec<ShapedRect>> = widget_rects_map()
        .lock()
        .ok()
        .and_then(|m| m.get(&(hwnd as usize)).cloned());

    match decide_region_clip(rects.as_deref()) {
        RegionClip::Full => {
            // No push yet: leave un-clipped. The user briefly sees the full
            // transparent overlay (not click-through) until layout settles.
            SetWindowRgn(hwnd, ptr::null_mut(), TRUE);
        }
        RegionClip::Empty => {
            // Explicit zero-rect push (zero-widget view): EMPTY region,
            // zero hit-test area, the desktop gets all input ([S-18]).
            // SetWindowRgn takes ownership of the HRGN on success.
            let empty = CreateRectRgn(0, 0, 0, 0);
            if empty.is_null() {
                // GDI allocation failure: fall back to clearing the clip,
                // a full-cover overlay is recoverable (toggle edit mode), a
                // stale widget-shaped clip on an empty view is not.
                SetWindowRgn(hwnd, ptr::null_mut(), TRUE);
            } else {
                SetWindowRgn(hwnd, empty, TRUE);
            }
        }
        RegionClip::Rects => {
            let region = build_widget_region(&rects.unwrap_or_default());
            // region is null only if every CreateRectRgn/CreateRoundRectRgn
            // call failed (GDI exhaustion), so SetWindowRgn(null) then clears
            // the clip, same recoverable fallback as the Empty arm.
            SetWindowRgn(hwnd, region, TRUE);
        }
    }
    // A pushed rect set can cover the full window (e.g. the context-menu
    // backdrop is tagged `data-themia-interactive` and spans the whole
    // overlay), which un-clips the frame area just like a null region. Cheap
    // and idempotent, so re-assert unconditionally rather than measuring the
    // region against the window rect.
    reassert_frameless(hwnd);
}

/// Frontend pushes a list of [x, y, w, h, radius] tuples (physical px,
/// client coords). The Rust side builds an HRGN that becomes the window's
/// shape via SetWindowRgn, clipping the overlay to exactly the widget +
/// chrome bounds. Everything outside is hit-test-transparent at the OS
/// level, so the Windows desktop receives input there for free.
#[tauri::command]
fn update_widget_rects(window: tauri::Window, rects: Vec<[i32; 5]>, interactive: bool) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        let hwnd_usize = window.hwnd().map_err(|e| e.to_string())?.0 as usize;
        let v: Vec<ShapedRect> = rects.iter().map(|r| ShapedRect {
            left: r[0],
            top: r[1],
            right: r[0].saturating_add(r[2]),
            bottom: r[1].saturating_add(r[3]),
            radius: r[4].max(0),
        }).collect();
        let non_empty = !v.is_empty();
        widget_rects_map().lock().unwrap_or_else(|e| e.into_inner()).insert(hwnd_usize, v);

        // The frontend ships its own view of interactivity with every push.
        // This rect push fires on every relevant reactive change (edit mode,
        // placing, context menu, ...), so it is the freshest authority on
        // whether the whole window should stay a drop target. Drive BOTH the
        // per-window flag and the region clip from it, keeping them in lockstep.
        //
        // Previously the clip decision read a flag that only `set_interactive`
        // wrote. Those are two independent, unordered IPC commands: when edit
        // mode turned on, a rect push that reached Rust before `set_interactive`
        // flipped the flag would clip the window to widget bounds even though
        // edit mode was on, empty grid cells then fell through to the desktop
        // and the mask looked "not updated". Driving the flag from the rects
        // here removes that race; `set_interactive` still owns the
        // WS_EX_NOACTIVATE strip + z-order re-raise.
        set_window_interactive(hwnd_usize, interactive);

        use winapi::shared::windef::HWND;
        if !interactive {
            unsafe { apply_widget_region(hwnd_usize as HWND, true); }

            // Reveal on the first rect push regardless of count. A non-empty
            // region clips to widget bounds (the original intent); an empty
            // region applies a NULL clip via `apply_widget_region` ->
            // `build_widget_region` -> SetWindowRgn(null), so the window is
            // un-clipped (fully transparent surface), not zero-area. Keeping
            // the show conditional on non-emptiness left zero-widget views
            // permanently invisible if the fallback timer also missed.
            let _ = non_empty;
            show_overlay_once(hwnd_usize);
        } else {
            // Whole window interactive (edit mode / modal): clear the region so
            // empty areas are part of the window (drop target + clickable empty
            // grid cells). `apply_widget_region(false)` re-asserts framelessness
            // too, so the residual OS frame can't flash back.
            unsafe { apply_widget_region(hwnd_usize as HWND, false); }
            show_overlay_once(hwnd_usize);
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (window, rects, interactive);
    }
    Ok(())
}

/// Pure decision mirrored by the WndProc's WM_MOUSEACTIVATE branch: should
/// the overlay refuse activation on the current mouse-down?
///
/// `true`  → return MA_NOACTIVATE; the click is still delivered but the
///           window does not get promoted to the foreground.
/// `false` → fall through to default activation (needed so focused text
///           inputs / edit mode / the picker can receive keyboard input).
///
/// The rule is simply "refuse unless the overlay is in an interactive
/// state". Kept as a standalone fn purely so it can be unit-tested without
/// a live HWND + message pump.
pub(crate) fn decide_refuse_activation(force_interactive: bool) -> bool {
    !force_interactive
}

/// Custom WndProc that silently swallows minimize requests (Show Desktop, Win+D, SC_MINIMIZE).
/// All other messages are forwarded to the original WebView2 WndProc.
#[cfg(target_os = "windows")]
unsafe extern "system" fn no_minimize_wnd_proc(
    hwnd: winapi::shared::windef::HWND,
    msg: winapi::shared::minwindef::UINT,
    wparam: winapi::shared::minwindef::WPARAM,
    lparam: winapi::shared::minwindef::LPARAM,
) -> winapi::shared::minwindef::LRESULT {
    // FFI boundary (A2): a panic in this subclass WndProc would unwind into the OS
    // message pump (UB). Catch it; on panic, fall back to DefWindowProcW.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        no_minimize_wnd_proc_impl(hwnd, msg, wparam, lparam)
    }))
    .unwrap_or_else(|_| unsafe {
        winapi::um::winuser::DefWindowProcW(hwnd, msg, wparam, lparam)
    })
}

unsafe fn no_minimize_wnd_proc_impl(
    hwnd: winapi::shared::windef::HWND,
    msg: winapi::shared::minwindef::UINT,
    wparam: winapi::shared::minwindef::WPARAM,
    lparam: winapi::shared::minwindef::LPARAM,
) -> winapi::shared::minwindef::LRESULT {
    use winapi::um::winuser::{
        CallWindowProcW, DefWindowProcW, SetWindowPos, ShowWindow,
        SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
        SW_SHOWNOACTIVATE, WINDOWPOS,
    };

    const WM_SYSCOMMAND: u32       = 0x0112;
    const WM_WINDOWPOSCHANGING: u32 = 0x0046;
    const WM_SIZE: u32             = 0x0005;
    const WM_SHOWWINDOW: u32       = 0x0018;
    const WM_NCCALCSIZE: u32       = 0x0083;
    const WM_NCACTIVATE: u32       = 0x0086;
    const WM_NCPAINT: u32          = 0x0085;
    const WM_NCDESTROY: u32        = 0x0082;
    const WM_ACTIVATE: u32         = 0x0006;
    const WM_MOUSEACTIVATE: u32    = 0x0021;
    const WM_DWMCOMPOSITIONCHANGED: u32 = 0x031E;
    const SC_MINIMIZE: usize       = 0xF020;
    const SIZE_MINIMIZED: usize    = 1;
    const SWP_HIDEWINDOW: u32      = 0x0080;
    const SW_PARENTCLOSING: usize  = 1;
    const SW_OTHERUNZOOM: usize    = 4;
    const SW_PARENTOPENING: usize  = 3;
    const SW_OTHERZOOM: usize      = 2;
    const MA_NOACTIVATE: isize     = 3;

    // Empty-area desktop pass-through is handled by SetWindowRgn (the window is
    // clipped to widget shapes), not by WM_NCHITTEST/HTTRANSPARENT.

    // Force a zero non-client area: when wParam is TRUE, returning 0 without
    // shrinking rgrc[0] makes the entire window rect the client area: no
    // caption, no border, no sizing frame, ever. Stripping the frame styles
    // in pin_to_desktop is a one-shot fix, but Tauri / WebView2 can reset
    // window styles (DPI change, internal SetWindowPos passes), which let a
    // "top bar" flash back. Handling WM_NCCALCSIZE makes framelessness a
    // permanent property of the window, independent of its style bits.
    if msg == WM_NCCALCSIZE && wparam != 0 {
        return 0;
    }

    // Suppress all non-client painting. Even with zero NC area from
    // WM_NCCALCSIZE, DefWindowProc paints a caption on activation
    // transitions if WS_CAPTION has been transiently restored by
    // tao / WebView2 (DPI change, internal SetWindowPos passes).
    // Returning TRUE from WM_NCACTIVATE tells Windows "NC area is
    // already up to date, don't repaint" kills the grey top-bar
    // flash on alt-tab back and on right-click while interactive.
    // WM_NCPAINT is swallowed for the same reason on ordinary
    // repaints (theme change, monitor change). Client-area focus,
    // IME, and WM_ACTIVATE delivery are unaffected; these only
    // suppress non-client drawing, exactly what a frameless overlay
    // wants.
    if msg == WM_NCACTIVATE { return 1; }
    if msg == WM_NCPAINT    { return 0; }

    // [BD-03] DWM restarted (or composition was toggled): every DWM window
    // attribute is back at its default, which re-opens the overlay to a
    // backdrop. Re-assert the denial here and fall through, so the original
    // proc still sees the message. This is the ONLY event that can undo
    // [BD-02] behind our back, so there is deliberately no polling pass.
    if msg == WM_DWMCOMPOSITIONCHANGED {
        deny_dwm_backdrop(hwnd);
    }

    // Refuse activation on mouse-down unless the overlay is in an
    // "interactive" state (edit mode, picker, focused text input, anything
    // that called set_interactive(true) and stripped WS_EX_NOACTIVATE).
    //
    // WS_EX_NOACTIVATE alone is NOT honored here: the WebView2 child window
    // handles the mouse-down and, by default, activates its top-level
    // parent, so an ordinary right-click (or left-click) on a widget would
    // promote the overlay above other apps. Returning MA_NOACTIVATE from the
    // parent's WndProc is the documented way to veto that. The mouse message
    // is still delivered (MA_NOACTIVATE does not eat it), so widgets and
    // context menus still receive the click; they just don't drag the
    // window to the foreground.
    //
    // When THIS window is interactive the user genuinely wants keyboard
    // focus (typing into a search field, etc.), so we fall through to
    // default activation behavior. Checked per-HWND: a second monitor's
    // interactive state must not let this window steal the foreground.
    if msg == WM_MOUSEACTIVATE {
        // Any click on a widget should dismiss a foreign popup menu (most
        // commonly the Explorer desktop context menu opened by right-
        // clicking a gap between widgets). Because our overlay never takes
        // the foreground, the shell menu's modal loop won't close it on its
        // own, so cancel it explicitly.
        dismiss_foreign_menu();

        if decide_refuse_activation(is_window_interactive(hwnd as usize)) {
            return MA_NOACTIVATE;
        }
    }

    // Block "Show Desktop" / Win+D / right-click-taskbar Minimize All
    if msg == WM_SYSCOMMAND && (wparam & 0xFFF0) == SC_MINIMIZE {
        return 0;
    }

    // WM_SHOWWINDOW(wparam=FALSE, lparam=SW_PARENTCLOSING|SW_OTHERZOOM) is
    // sent by the shell when "Show Desktop" hides apps. Swallow the hide
    // signal but pass through the un-hide variants so genuine show events
    // (parent restored, zoom undone) still reach the window.
    if msg == WM_SHOWWINDOW && wparam == 0 {
        let reason = lparam as usize;
        if reason == SW_PARENTCLOSING || reason == SW_OTHERZOOM {
            return 0;
        }
    }
    let _ = SW_OTHERUNZOOM;
    let _ = SW_PARENTOPENING;

    // Some ShowDesktop paths use WM_WINDOWPOSCHANGING with SWP_HIDEWINDOW.
    // Anchor z-order just above Progman so the widget rides up with the
    // shell on Show Desktop instead of being covered by it. Real apps still
    // sit above Progman and therefore above us.
    //
    // EXCEPTION: when our foreground hook has flipped widgets to topmost
    // (Show Desktop active), DO NOT overwrite hwndInsertAfter, otherwise
    // we'd cancel our own topmost SetWindowPos and stay below Progman.
    if msg == WM_WINDOWPOSCHANGING && lparam != 0 {
        let wp = &mut *(lparam as *mut WINDOWPOS);
        wp.flags &= !SWP_HIDEWINDOW;
        // [PL-01] A pin overlay is exempt from the Show-Desktop latch check:
        // its anchor is HWND_TOPMOST in every state, so there is nothing for the
        // latch to protect, and letting the rewrite run keeps an external
        // restack from dropping it out of the topmost band.
        if wp.flags & SWP_NOZORDER == 0
            && (hwnd_is_pin_layer(hwnd)
                || !WIDGETS_PINNED_TOPMOST.load(std::sync::atomic::Ordering::Relaxed))
        {
            wp.hwndInsertAfter = z_anchor_for(hwnd);
        }
    }

    // Last-resort safety net: if the window somehow ends up minimized, immediately restore it
    if msg == WM_SIZE && wparam == SIZE_MINIMIZED {
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        SetWindowPos(
            hwnd,
            z_anchor_for(hwnd),
            0, 0, 0, 0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
        return 0;
    }

    // Window is being destroyed: forward to the original proc, then prune every
    // HWND-keyed map so they don't accumulate dead entries across a session of
    // windows opening/closing (W8). WM_NCDESTROY is the last message a window
    // ever receives, so this runs exactly once per window. Poison-safe locks: a
    // teardown path must never panic in a WndProc.
    if msg == WM_NCDESTROY {
        let key = hwnd as usize;
        let old = wndproc_map()
            .lock()
            .ok()
            .and_then(|m| m.get(&key).copied())
            .unwrap_or(0);
        let result = if old != 0 {
            CallWindowProcW(std::mem::transmute::<isize, winapi::um::winuser::WNDPROC>(old), hwnd, msg, wparam, lparam)
        } else {
            DefWindowProcW(hwnd, msg, wparam, lparam)
        };
        if let Ok(mut m) = wndproc_map().lock() { m.remove(&key); }
        if let Ok(mut m) = widget_rects_map().lock() { m.remove(&key); }
        if let Ok(mut m) = interactive_windows().lock() { m.remove(&key); }
        if let Ok(mut m) = shown_windows().lock() { m.remove(&key); }
        // [PL-01] Never leave a dead HWND claiming the topmost anchor: a
        // recycled handle would be restacked as a pin overlay it isn't.
        if let Ok(mut s) = pin_layer_hwnds().lock() { s.remove(&key); }
        // [HK-01] Keep the hook's lock-free view in step with the map.
        publish_overlay_snapshot();
        return result;
    }

    let old = wndproc_map()
        .lock()
        .ok()
        .and_then(|m| m.get(&(hwnd as usize)).copied())
        .unwrap_or(0);

    let result = if old != 0 {
        CallWindowProcW(std::mem::transmute::<isize, winapi::um::winuser::WNDPROC>(old), hwnd, msg, wparam, lparam)
    } else {
        DefWindowProcW(hwnd, msg, wparam, lparam)
    };

    // [ZS-02] After the overlay is activated, push it back down to its desktop
    // anchor. The foreground hook (`foreground_change_proc`) handles the common
    // case where activation moves the foreground TO the overlay, but a second
    // click on the already-foreground overlay (e.g. two right-clicks in a row
    // while a context menu is up) does not change foreground, so the hook never
    // fires, yet DefWindowProc's BringWindowToTop during activation can still
    // re-arrange the z-band and bury the settings window beneath the overlay.
    // WM_ACTIVATE is delivered after that rearrange, so re-anchoring here
    // undoes it. LOWORD(wparam) == 0 is WA_INACTIVE; we only react to actual
    // activation (WA_ACTIVE / WA_CLICKACTIVE).
    //
    // The overlay moves, never the settings window: repairing z-order by
    // raising the settings window put it in front of whatever app the user had
    // switched to, because a NOTOPMOST re-insert lands at the TOP of the whole
    // normal band, not merely above our own overlays.
    if msg == WM_ACTIVATE
        && (wparam & 0xFFFF) != 0
        && overlay_activation_demotes(
            WIDGETS_PINNED_TOPMOST.load(std::sync::atomic::Ordering::Relaxed),
        )
    {
        anchor_widget_above_desktop(hwnd);
    }

    result
}

// Shared app handle for the display-change watcher thread
#[cfg(target_os = "windows")]
static WATCHER_APP: std::sync::OnceLock<tauri::AppHandle> = std::sync::OnceLock::new();

// Trailing debounce: wnd_proc bumps a monotonic event-sequence counter; a
// dedicated thread waits for a quiet period before firing on_display_change.
// [DW-03] A sequence counter, never wall clock. The previous implementation
// stored SystemTime millis: a backwards clock step (NTP correction) stamped
// new events BEFORE the last-handled timestamp, so every later display change
// was silently dropped and a taskbar-overlapping overlay stayed wrong until
// the app was restarted.
#[cfg(target_os = "windows")]
static DISPLAY_CHANGE_SEQ: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
#[cfg(target_os = "windows")]
static DISPLAY_HANDLED_SEQ: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Queue a debounced display re-pin pass ([DW-02]). Callable from any thread
/// (watcher wnd_proc, work-area sweep); the debounce thread coalesces bursts
/// and runs `on_display_change` once events settle.
#[cfg(target_os = "windows")]
fn request_display_pass() {
    DISPLAY_CHANGE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // [ZP-01] Topology / taskbar / theme churn is exactly when the shell
    // restacks; keep the z-order poll at its fast cadence through it.
    note_shell_activity();
}

/// [DW-02] A re-pin pass is due when events arrived that the debounce thread
/// has not yet handled. Pure inequality on a monotonic counter, immune to
/// clock steps ([DW-03]); no desync state can strand events.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn display_pass_due(seq: u64, handled: u64) -> bool {
    seq != handled
}

/// [DW-02] The debounce window is quiet when no new event arrived while the
/// debounce thread slept (sequence unchanged since the snapshot).
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn display_events_settled(snapshot: u64, current: u64) -> bool {
    snapshot == current
}

// Per-window "interactive" state. A window is interactive when edit mode is
// on, a modal/picker is open, or a text input is focused, anything that
// needs keyboard focus + the full window shape. themia runs one overlay
// window per monitor, so this MUST be per-HWND: a global flag let one
// monitor's interactive state leak into another's WM_MOUSEACTIVATE handling
// and region updates (the multi-monitor flicker / front-jump bug).
#[cfg(target_os = "windows")]
static INTERACTIVE_WINDOWS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashSet<usize>>,
> = std::sync::OnceLock::new();

#[cfg(target_os = "windows")]
fn interactive_windows()
    -> &'static std::sync::Mutex<std::collections::HashSet<usize>>
{
    INTERACTIVE_WINDOWS.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// Mark a specific overlay window interactive / not. Interactive windows fall
/// through to default activation (keyboard focus) and skip region clipping.
#[cfg(target_os = "windows")]
fn set_window_interactive(hwnd_usize: usize, interactive: bool) {
    let mut set = interactive_windows().lock().unwrap_or_else(|e| e.into_inner());
    if interactive {
        set.insert(hwnd_usize);
    } else {
        set.remove(&hwnd_usize);
    }
}

/// True if the given overlay window HWND is currently interactive.
#[cfg(target_os = "windows")]
fn is_window_interactive(hwnd_usize: usize) -> bool {
    interactive_windows()
        .lock()
        .map(|s| s.contains(&hwnd_usize))
        .unwrap_or(false)
}

// Overlay windows are born `.visible(false)` and stay hidden until they are
// region-clipped to the widget shapes; showing them while still unclipped
// lets the grey OS titlebar flash during the async gap before the frontend
// pushes rects. This set records which HWNDs have already been un-hidden so
// the two show paths (first non-empty `update_widget_rects`, fallback timer)
// race-free settle on exactly one `ShowWindow`. Per-HWND like the
// interactive set, one entry per monitor overlay.
#[cfg(target_os = "windows")]
static SHOWN_WINDOWS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashSet<usize>>,
> = std::sync::OnceLock::new();

#[cfg(target_os = "windows")]
fn shown_windows()
    -> &'static std::sync::Mutex<std::collections::HashSet<usize>>
{
    SHOWN_WINDOWS.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// First call for a given HWND returns true (caller should `ShowWindow`),
/// every later call returns false. Pure bookkeeping, kept separate from the
/// `ShowWindow` call so it can be unit-tested without a live HWND. Both real
/// callers run on the main thread, so the `HashSet::insert` return value is
/// the only guard needed.
#[cfg(target_os = "windows")]
fn mark_shown_once(hwnd_usize: usize) -> bool {
    shown_windows().lock().unwrap_or_else(|e| e.into_inner()).insert(hwnd_usize)
}

/// Un-hide the overlay window exactly once. `SW_SHOWNOACTIVATE` never steals
/// focus or changes activation/z-order.
#[cfg(target_os = "windows")]
fn show_overlay_once(hwnd_usize: usize) -> bool {
    use winapi::um::winuser::{ShowWindow, SW_SHOWNOACTIVATE};
    if mark_shown_once(hwnd_usize) {
        unsafe { ShowWindow(hwnd_usize as _, SW_SHOWNOACTIVATE); }
        true
    } else {
        false
    }
}

// ---------------------------------------------------------------------------
// Per-monitor disable: destroy / rebuild (zero-resource for disabled)
// ---------------------------------------------------------------------------
//
// Goal: a disabled monitor consumes NO resources: no HWND, no WebView2
// process, no memory. Achieved by skipping window creation at startup for
// monitors in the persisted-disabled list, and `WebviewWindow::close()` /
// `build_widget_window` on runtime toggles.
//
// The critical correctness constraint, learned the hard way:
//
//   `WebviewWindowBuilder::build()` dispatches `Message::CreateWindow` via
//   the Tauri/Wry event-loop proxy and BLOCKS until the main thread pump
//   processes it. tauri-runtime-wry-2.11.1 lib.rs:2168 explicitly warns:
//   "must be called from a separate thread, otherwise the channel will
//   introduce a deadlock." Wrapping in `run_on_main_thread` makes the
//   caller closure the main thread → pump can't run → build never returns.
//
// So `sync_disabled_monitors` calls reconcile DIRECTLY on the Tauri command
// worker thread. Build dispatches to main, main processes it, build
// returns. No marshal, no deadlock.
//
// The persistence file (`app_data_dir/disabled_monitors.json`) is the
// source of truth: written by the toggle command and read by the startup
// loop so the very first frame already skips overlays on disabled monitors.
// [MI-01] Its entries are STABLE monitor ids, never `\\.\DISPLAYn` ordinals;
// [MI-03] a pre-#70 file naming ordinals is re-keyed once, on the first
// enumeration that can resolve them.

/// Bumped whenever the MEANING of the entries changes. 1 (implicit, a bare
/// JSON array) held `\\.\DISPLAYn` ordinals; 2 holds [MI-01] stable ids.
/// The stamp is what makes the [MI-03] migration run-once.
const DISABLED_MONITORS_SCHEMA_VERSION: u32 = 2;

#[derive(serde::Serialize, serde::Deserialize)]
struct DisabledMonitorsFile {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    disabled: Vec<String>,
}

/// Parsed shape of `disabled_monitors.json`.
///
/// "Absent" and "corrupt" are deliberately distinct states, not one
/// `unwrap_or_default()`: absent is the normal first-run condition (silent),
/// corrupt is a real fault (logged) that must never be migrated or
/// overwritten, since doing so would replace whatever the user actually had
/// with an empty list.
#[derive(Debug, PartialEq, Eq)]
enum DisabledMonitorsDoc {
    /// No file yet (first run).
    Absent,
    /// Present but unparseable. Read as "nothing disabled", left on disk as-is.
    Corrupt,
    /// Pre-#70: a bare JSON array of `\\.\DISPLAYn` ordinals. Needs [MI-03].
    Legacy(Vec<String>),
    /// schemaVersion >= 2: [MI-01] stable ids. Nothing to do.
    Current(Vec<String>),
}

/// Trust boundary: the on-disk blob is parsed through the schema, never
/// `serde_json::from_str` straight into the live set.
fn parse_disabled_monitors(s: &str) -> DisabledMonitorsDoc {
    if let Ok(doc) = serde_json::from_str::<DisabledMonitorsFile>(s) {
        return if doc.schema_version >= DISABLED_MONITORS_SCHEMA_VERSION {
            // A NEWER schema than this build knows still holds stable ids
            // (versions only ever move forward), so leave it alone rather than
            // "migrating" a downgrade back into ordinals.
            DisabledMonitorsDoc::Current(doc.disabled)
        } else {
            DisabledMonitorsDoc::Legacy(doc.disabled)
        };
    }
    match serde_json::from_str::<Vec<String>>(s) {
        Ok(v) => DisabledMonitorsDoc::Legacy(v),
        Err(_) => DisabledMonitorsDoc::Corrupt,
    }
}

/// [MI-03] True for the pre-#70 key shape: a GDI ordinal `\\.\DISPLAY<n>`.
/// Purely syntactic on purpose. Anything else in a saved list is either a
/// stable id ([MI-01]) or a string we never wrote, and both must survive
/// migration untouched.
fn is_legacy_device_name(s: &str) -> bool {
    s.to_ascii_uppercase().starts_with(r"\\.\DISPLAY")
}

#[derive(Debug, PartialEq, Eq)]
struct DisabledMigration {
    /// The re-keyed set to persist (sorted, deduped).
    kept: Vec<String>,
    /// Ordinals that resolved to nothing and were discarded. Logged loudly.
    dropped: Vec<String>,
}

/// [MI-03] Re-key a saved disabled list from GDI ordinals onto stable ids.
///
/// `None` means "do not migrate, do not touch the file": the enumeration was
/// EMPTY, so nothing can be resolved and a rewrite would destroy the user's
/// config. That state is reachable in practice, not theoretical: the autostart
/// task uses a LogonTrigger with no delay ([AS-01]), so the first enumeration
/// can land before any panel has finished coming up.
///
/// Per entry:
///   * resolves via the live device_name→id map → replaced by the stable id;
///   * not ordinal-shaped (already a stable id) → passed through untouched,
///     even when its panel is unplugged right now: an id still names that exact
///     panel, so its disabled state is worth keeping while it is away;
///   * an ordinal that resolves to nothing → DROPPED.
///
/// The drop is deliberate. A bare ordinal cannot be resolved back to a panel
/// once that panel is gone, and keeping it is not neutral — it IS the #70 bug:
/// the entry would go on disabling whichever unrelated panel inherits that
/// ordinal next boot. The mild cost is that a monitor disconnected at migration
/// time forgets its disabled state once, which the user re-sets in one click.
fn migrate_disabled_ids(
    saved: &[String],
    device_name_to_id: &std::collections::HashMap<String, String>,
) -> Option<DisabledMigration> {
    if device_name_to_id.is_empty() {
        return None;
    }
    let mut kept: Vec<String> = Vec::new();
    let mut dropped: Vec<String> = Vec::new();
    for entry in saved {
        match device_name_to_id.get(entry) {
            Some(id) => {
                if !kept.iter().any(|k| k == id) {
                    kept.push(id.clone());
                }
            }
            None if is_legacy_device_name(entry) => dropped.push(entry.clone()),
            None => {
                if !kept.iter().any(|k| k == entry) {
                    kept.push(entry.clone());
                }
            }
        }
    }
    kept.sort();
    dropped.sort();
    Some(DisabledMigration { kept, dropped })
}

/// [MI-05] The build gate. A monitor gets an overlay unless its STABLE id is
/// in the persisted disabled set. Never keyed on the ordinal ([MI-01]).
fn monitor_should_have_window(
    id: &str,
    disabled: &std::collections::HashSet<String>,
) -> bool {
    !disabled.contains(id)
}

fn disabled_monitors_path(app: &tauri::AppHandle) -> std::path::PathBuf {
    use tauri::Manager;
    app.path()
        .app_data_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join("disabled_monitors.json")
}

/// Single-writer gate for `disabled_monitors.json`. Every window pushes its
/// disabled map at startup and the [MI-03] migration is a read-modify-write, so
/// the writes really do race.
static DISABLED_MONITORS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn read_disabled_monitors_doc_at(path: &std::path::Path) -> DisabledMonitorsDoc {
    let s = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return DisabledMonitorsDoc::Absent,
        Err(e) => {
            log::warn!(
                target: "displays",
                "read_disabled_monitors: read failed for {}: {e}, assuming none disabled",
                path.display()
            );
            return DisabledMonitorsDoc::Corrupt;
        }
    };
    let doc = parse_disabled_monitors(&s);
    if doc == DisabledMonitorsDoc::Corrupt {
        log::warn!(
            target: "displays",
            "read_disabled_monitors: corrupt JSON in {}, assuming none disabled \
             (the file is left as-is, never overwritten)",
            path.display()
        );
    }
    doc
}

/// Read the disabled set from a path. Missing file / corrupt JSON → empty set
/// (the safe default: assume nothing disabled, never destroy the user's
/// overlays over a recoverable read error).
fn read_disabled_monitors_at(path: &std::path::Path) -> std::collections::HashSet<String> {
    match read_disabled_monitors_doc_at(path) {
        DisabledMonitorsDoc::Absent | DisabledMonitorsDoc::Corrupt => {
            std::collections::HashSet::new()
        }
        DisabledMonitorsDoc::Legacy(v) | DisabledMonitorsDoc::Current(v) => {
            v.into_iter().collect()
        }
    }
}

/// Atomically replace `path`'s contents: write a sibling temp file, flush it to
/// disk, then rename over the target (`std::fs::rename` replaces an existing
/// file on Windows). A crash mid-write can then never leave a truncated list,
/// which would silently re-enable every overlay the user disabled.
fn write_file_atomic(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;

    let tmp = path.with_extension("json.tmp");
    let write_tmp = || -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()
    };
    if let Err(e) = write_tmp() {
        // Leave no debris behind on the failure path.
        let _ = std::fs::remove_file(&tmp); // ignore: best-effort temp cleanup
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp); // ignore: best-effort temp cleanup
        return Err(e);
    }
    Ok(())
}

/// Caller must already hold `DISABLED_MONITORS_LOCK`.
fn write_disabled_monitors_locked(
    path: &std::path::Path,
    set: &std::collections::HashSet<String>,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Sort for stable on-disk ordering: easier diffs, deterministic tests.
    let mut disabled: Vec<String> = set.iter().cloned().collect();
    disabled.sort();
    let doc = DisabledMonitorsFile {
        schema_version: DISABLED_MONITORS_SCHEMA_VERSION,
        disabled,
    };
    let s = serde_json::to_string(&doc)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    write_file_atomic(path, &s)
}

/// Write the disabled set ([MI-01] stable ids) to a path, creating the parent
/// directory if needed. Atomic + single-writer.
fn write_disabled_monitors_at(
    path: &std::path::Path,
    set: &std::collections::HashSet<String>,
) -> std::io::Result<()> {
    let _guard = DISABLED_MONITORS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    write_disabled_monitors_locked(path, set)
}

/// [MI-03] One-time, idempotent re-key of `disabled_monitors.json` onto stable
/// ids. Safe (and cheap) to call on every enumeration: a file already stamped
/// with the current schemaVersion is left alone.
fn migrate_disabled_monitors_file_at(path: &std::path::Path, monitors: &[MonitorInfo]) {
    let _guard = DISABLED_MONITORS_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let saved = match read_disabled_monitors_doc_at(path) {
        // Nothing written yet, or already migrated / newer: no work.
        DisabledMonitorsDoc::Absent | DisabledMonitorsDoc::Current(_) => return,
        // Never rewrite a file we could not parse; it may hold real state.
        DisabledMonitorsDoc::Corrupt => return,
        DisabledMonitorsDoc::Legacy(v) => v,
    };

    let map = device_name_to_id_map(monitors);
    let Some(migration) = migrate_disabled_ids(&saved, &map) else {
        log::warn!(
            target: "displays",
            "disabled_monitors: migration to stable ids deferred, no monitors \
             enumerated yet. The file is untouched and will be migrated on the \
             first enumeration that sees the displays."
        );
        return;
    };

    if !migration.dropped.is_empty() {
        log::warn!(
            target: "displays",
            "disabled_monitors: DROPPED {} disabled entr(ies) {:?} while migrating \
             to stable ids. They name display ordinals that no currently-connected \
             monitor holds, and an ordinal is not an identity (#70): keeping one \
             would disable whichever unrelated panel inherits it next boot. If a \
             monitor was meant to stay disabled, re-disable it once it is connected.",
            migration.dropped.len(),
            migration.dropped
        );
    }

    let set: std::collections::HashSet<String> = migration.kept.iter().cloned().collect();
    match write_disabled_monitors_locked(path, &set) {
        Ok(()) => log::info!(
            target: "displays",
            "disabled_monitors: migrated {} legacy entr(ies) to stable ids \
             (schemaVersion {DISABLED_MONITORS_SCHEMA_VERSION}), now disabled: {:?}",
            saved.len(),
            migration.kept
        ),
        Err(e) => log::error!(
            target: "displays",
            "disabled_monitors: migration write failed for {}: {e}. The legacy \
             file is left as-is and the migration will be retried.",
            path.display()
        ),
    }
}

fn read_disabled_monitors(app: &tauri::AppHandle) -> std::collections::HashSet<String> {
    read_disabled_monitors_at(&disabled_monitors_path(app))
}

fn write_disabled_monitors(
    app: &tauri::AppHandle,
    set: &std::collections::HashSet<String>,
) -> std::io::Result<()> {
    write_disabled_monitors_at(&disabled_monitors_path(app), set)
}

fn migrate_disabled_monitors_file(app: &tauri::AppHandle, monitors: &[MonitorInfo]) {
    migrate_disabled_monitors_file_at(&disabled_monitors_path(app), monitors)
}

/// The live device_name → stable id map for the current topology ([MI-03]).
fn device_name_to_id_map(monitors: &[MonitorInfo]) -> std::collections::HashMap<String, String> {
    monitors
        .iter()
        .map(|m| (m.device_name.clone(), m.id.clone()))
        .collect()
}

/// [MI-06] The overlay slot each connected monitor owns, keyed on its STABLE
/// id. This is #70's lesson applied to the WINDOW: `monitor-{i}` is an
/// enumeration ordinal, exactly as unstable as `\\.\DISPLAYn`, so deriving the
/// slot from the enumeration index re-binds live overlays to different physical
/// panels whenever the topology re-orders.
///
/// That is issue #93. Powering monitors off for the night drops them out of the
/// topology (DisplayPort HPD); powering them back on re-adds them in link-training
/// order, which is not the order they left in. Under index-keyed slots the pass
/// then re-pinned `monitor-0` — the window covering the secondary at that moment —
/// onto the primary's work area, so the user's widgets visibly moved to the other
/// screen, taking their stale click-through region with them (hence also "Themia
/// stops responding to right clicks"). Only a restart put it back.
///
/// `bound` maps a stable monitor id to the slot its overlay already occupies. A
/// monitor that already holds a slot KEEPS it; monitors with none take the lowest
/// free slot, in enumeration order. Slots are therefore allowed to be
/// non-contiguous — they always were, since a disabled monitor leaves a hole —
/// so no caller may assume `0..monitors.len()`.
fn assign_monitor_slots(
    monitors: &[MonitorInfo],
    bound: &std::collections::HashMap<String, usize>,
) -> Vec<usize> {
    // usize::MAX marks "not decided yet"; every entry is overwritten below.
    let mut slots = vec![usize::MAX; monitors.len()];
    let mut claimed: std::collections::HashSet<usize> = std::collections::HashSet::new();

    // Pass 1: keepers. `claimed.insert` false means two monitors resolve to the
    // same slot (a stale map entry, or the [MI-02] ordinal fallback colliding);
    // the first one in enumeration order keeps it, the other is treated as new.
    for (i, m) in monitors.iter().enumerate() {
        if let Some(&s) = bound.get(&m.id) {
            if claimed.insert(s) {
                slots[i] = s;
            }
        }
    }

    // Pass 2: newcomers fill the lowest free slots, so slots are reused rather
    // than growing without bound as panels come and go.
    let mut next = 0usize;
    for s in slots.iter_mut() {
        if *s != usize::MAX {
            continue;
        }
        while !claimed.insert(next) {
            next += 1;
        }
        *s = next;
    }
    slots
}

/// [MI-06] The live `stable id → slot` binding `assign_monitor_slots` needs.
///
/// Read from the `label → id` map, but only for labels that still have a window:
/// a stale entry (a close that failed, a webview that died) must not reserve a
/// slot forever. The map is cloned before the window lookups so the mutex is not
/// held across Tauri calls.
#[cfg(target_os = "windows")]
fn bound_monitor_slots(app: &tauri::AppHandle) -> std::collections::HashMap<String, usize> {
    use tauri::Manager;
    let snapshot: Vec<(String, String)> = window_monitor_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(label, id)| (label.clone(), id.clone()))
        .collect();
    snapshot
        .into_iter()
        .filter_map(|(label, id)| {
            let slot = label.strip_prefix("monitor-")?.parse::<usize>().ok()?;
            app.get_webview_window(&label)?;
            Some((id, slot))
        })
        .collect()
}

/// Pure decision for what `reconcile_disabled_windows` should do, factored
/// out for unit-testing. Returns one action per monitor.
#[derive(Debug, PartialEq, Eq)]
enum ReconcileAction {
    /// Window exists, monitor is disabled: close it (destroy the HWND,
    /// release the WebView2 process, free its memory).
    Close { label: String },
    /// Window doesn't exist, monitor is enabled: build it.
    Build { index: usize, monitor_id: String, x: i32, y: i32, width: i32, height: i32 },
    /// No change needed (steady state).
    Noop { label: String, reason: &'static str },
}

/// The ONE build/close decision, shared by startup (`window_exists` = never)
/// and every later reconcile pass. [MI-05]/[DW-07]: an empty monitor list
/// yields NO actions, so a topology we cannot see can never produce a phantom
/// overlay that ignores the disabled set.
///
/// `slots[i]` is the overlay slot monitor `i` owns ([MI-06], from
/// `assign_monitor_slots`); the label and the build index both come from it,
/// never from the enumeration index (#93).
fn plan_reconcile(
    monitors: &[MonitorInfo],
    slots: &[usize],
    disabled: &std::collections::HashSet<String>,
    window_exists: impl Fn(&str) -> bool,
) -> Vec<ReconcileAction> {
    debug_assert_eq!(slots.len(), monitors.len(), "one slot per monitor");
    let mut actions = Vec::with_capacity(monitors.len());
    for (i, m) in monitors.iter().enumerate() {
        // Falling back to the index keeps a caller bug from silently dropping a
        // monitor; `debug_assert` above catches it in tests.
        let slot = slots.get(i).copied().unwrap_or(i);
        let label = format!("monitor-{slot}");
        // [MI-01] Keyed on the STABLE id. Keying on `m.device_name` here was
        // half of #70: the ordinal designates a different panel every boot.
        let is_disabled = !monitor_should_have_window(&m.id, disabled);
        let exists = window_exists(&label);
        let action = match (is_disabled, exists) {
            (true,  true)  => ReconcileAction::Close { label },
            (false, false) => ReconcileAction::Build {
                index: slot, monitor_id: m.id.clone(),
                x: m.x, y: m.y, width: m.width, height: m.height,
            },
            (true,  false) => ReconcileAction::Noop { label, reason: "disabled+absent" },
            (false, true)  => ReconcileAction::Noop { label, reason: "enabled+present" },
        };
        actions.push(action);
    }
    actions
}

/// Make the live window set match the disabled list: close overlays whose
/// monitor is now disabled, build overlays for monitors that are not.
///
/// MUST be invoked from a Tauri command worker thread or `std::thread::spawn`,
/// never from the main thread / `.setup()` / `run_on_main_thread` closure.
/// `WebviewWindowBuilder::build()` posts a `Message::CreateWindow` to the
/// event-loop proxy and waits for the main thread pump to process it; calling
/// from the main thread blocks the pump and deadlocks. See module comment.
///
/// The one exception is startup, where `.setup()` runs before the event loop
/// has started; there the inline `send_user_message` path handles the
/// message synchronously without needing the pump. The startup loop in
/// `.setup()` calls `build_widget_window` directly for that reason.
#[cfg(target_os = "windows")]
fn reconcile_disabled_windows(
    app: &tauri::AppHandle,
    disabled: &std::collections::HashSet<String>,
) {
    use tauri::Manager;
    let monitors = enumerate_monitors();
    if monitors.is_empty() {
        return;
    }
    // [MI-06] Slots come from the live binding, so a runtime enable/disable
    // toggle never re-shuffles the overlays that are already up (#93).
    let slots = assign_monitor_slots(&monitors, &bound_monitor_slots(app));
    let actions = plan_reconcile(&monitors, &slots, disabled, |label| {
        app.get_webview_window(label).is_some()
    });

    for action in actions {
        match action {
            ReconcileAction::Close { label } => {
                drag_drop::revoke_drop_target(&label);
                if let Some(win) = app.get_webview_window(&label) {
                    if let Err(e) = win.close() {
                        log::error!(target: "displays", "close failed for {label}: {e:?}");
                    }
                }
                if let Ok(mut map) = window_monitor_map().lock() {
                    map.remove(&label);
                }
            }
            ReconcileAction::Build { index, monitor_id, x, y, width, height } => {
                if let Err(e) = build_widget_window(
                    app, index, x, y, width, height, &monitor_id, OverlayLayer::Base,
                ) {
                    log::error!(
                        target: "displays",
                        "build_widget_window failed for monitor-{index} ({monitor_id}): {e:?}"
                    );
                }
            }
            ReconcileAction::Noop { .. } => {}
        }
    }

    // [PL-02] A monitor that was just enabled or disabled gains or loses its
    // pin overlay with it; one reconcile owns both layers.
    sync_pin_overlays(app);
}

/// Frontend tells us which monitors are disabled, by [MI-01] STABLE id (the
/// value `get_monitor_id` / `get_active_monitors` handed it). We persist the
/// list and reconcile the live window set. The reconcile runs on a
/// freshly-spawned OS thread, guaranteed off the main thread regardless of how
/// Tauri's invoke handler dispatches sync commands. See
/// `reconcile_disabled_windows` for the deadlock root cause that motivates this.
#[tauri::command]
fn sync_disabled_monitors(app: tauri::AppHandle, disabled: Vec<String>) -> Result<(), String> {
    // [MI-04] Trust boundary. The frontend is expected to send stable ids, but
    // an un-migrated localStorage from an older build would send `\\.\DISPLAYn`
    // ordinals, and persisting those would let #70 back in through the front
    // door. Normalize with the same rules as the on-disk migration ([MI-03]):
    // stable ids pass through untouched, so the steady-state call is a no-op.
    let monitors = enumerate_monitors();
    let normalized = migrate_disabled_ids(&disabled, &device_name_to_id_map(&monitors));
    let set: std::collections::HashSet<String> = match normalized {
        Some(m) => {
            if !m.dropped.is_empty() {
                log::warn!(
                    target: "displays",
                    "sync_disabled_monitors: dropped {:?}: display ordinals naming no \
                     connected monitor. The frontend should send stable monitor ids (#70).",
                    m.dropped
                );
            }
            m.kept.into_iter().collect()
        }
        None => {
            // No monitors enumerated: nothing to resolve against. Persist what
            // the frontend sent rather than dropping the user's intent; the
            // [MI-03] migration re-keys it once the displays show up.
            log::warn!(
                target: "displays",
                "sync_disabled_monitors: no monitors enumerated, persisting the list as sent"
            );
            disabled.into_iter().collect()
        }
    };
    write_disabled_monitors(&app, &set).map_err(|e| {
        log::error!(target: "displays", "write_disabled_monitors failed: {e}");
        e.to_string()
    })?;
    #[cfg(target_os = "windows")]
    {
        let app_clone = app.clone();
        let set_clone = set.clone();
        // Fresh OS thread → guaranteed off the main thread regardless of
        // how Tauri dispatches sync commands. Required because
        // `WebviewWindowBuilder::build()` in `reconcile_disabled_windows`
        // dispatches `Message::CreateWindow` to the event-loop proxy and
        // blocks until the main pump handles it; calling from the main
        // thread would block the pump and deadlock the channel.
        // Catch panics so a build failure can never poison the runtime.
        std::thread::Builder::new()
            .name("themia-disabled-reconcile".into())
            .spawn(move || {
                if let Err(p) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    reconcile_disabled_windows(&app_clone, &set_clone);
                })) {
                    let msg = p.downcast_ref::<&'static str>().map(|s| (*s).to_string())
                        .or_else(|| p.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "<non-string panic>".to_string());
                    log::error!(target: "displays", "reconcile panicked: {msg}");
                }
            })
            .map_err(|e| {
                log::error!(target: "displays", "spawn reconcile thread failed: {e}");
                e.to_string()
            })?;
    }
    #[cfg(not(target_os = "windows"))]
    let _ = set;
    Ok(())
}

// Runtime-registered TaskbarCreated message id. Broadcast by the shell when
// explorer.exe restarts, invalidating Progman/WorkerW HWNDs.
#[cfg(target_os = "windows")]
static TASKBAR_CREATED_MSG: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);

// ---------------------------------------------------------------------------
// Desktop icon visibility ([DI-NN] in SPEC.md)
// ---------------------------------------------------------------------------

/// [DI-01] `SHELLDLL_DefView`'s "toggle desktop icons" command id — the exact
/// command the desktop context menu's View > Show desktop icons item posts.
/// Undocumented (no MSDN constant) but unchanged since Vista and identical on
/// Windows 10 and 11; 11's rewritten context menu still routes here.
#[cfg(target_os = "windows")]
const IDM_TOGGLE_DESKTOP_ICONS: usize = 0x7402;

/// Walk Progman → SHELLDLL_DefView → SysListView32 (and the WorkerW variants
/// Windows 10/11 use when a wallpaper host sits between Progman and the view).
/// Returns `(defview, listview)`: the [DI-01] command target and the [DI-02]
/// state source. `FindWindowEx` ignores visibility, so both resolve whether the
/// icons are currently shown or hidden.
#[cfg(target_os = "windows")]
fn find_desktop_shell_view() -> Option<(winapi::shared::windef::HWND, winapi::shared::windef::HWND)> {
    use std::ptr;
    use winapi::shared::windef::HWND;
    use winapi::um::winuser::{FindWindowExW, FindWindowW};

    macro_rules! w {
        ($s:literal) => {{
            let v: Vec<u16> = concat!($s, "\0").encode_utf16().collect();
            v
        }};
    }

    unsafe {
        let progman = FindWindowW(w!("Progman").as_ptr(), ptr::null());
        if progman.is_null() {
            return None;
        }

        // Helper: given a parent, look for SHELLDLL_DefView → SysListView32
        let try_parent = |parent: HWND| -> Option<(HWND, HWND)> {
            let defview = FindWindowExW(
                parent, ptr::null_mut(), w!("SHELLDLL_DefView").as_ptr(), ptr::null(),
            );
            if defview.is_null() {
                return None;
            }
            let lv = FindWindowExW(
                defview, ptr::null_mut(), w!("SysListView32").as_ptr(), ptr::null(),
            );
            if lv.is_null() { None } else { Some((defview, lv)) }
        };

        // Direct child of Progman (most common)
        if let Some(found) = try_parent(progman) {
            return Some(found);
        }

        // Windows 10/11: icons may live under a WorkerW instead
        let mut worker = FindWindowExW(
            ptr::null_mut(), ptr::null_mut(), w!("WorkerW").as_ptr(), ptr::null(),
        );
        while !worker.is_null() {
            if let Some(found) = try_parent(worker) {
                return Some(found);
            }
            worker = FindWindowExW(
                ptr::null_mut(), worker, w!("WorkerW").as_ptr(), ptr::null(),
            );
        }

        None
    }
}

/// [DI-05] Resolve + cache both shell HWNDs, revalidating a cached pair with
/// `IsWindow` first. The TaskbarCreated broadcast clears the cache on an
/// explorer.exe restart, but that message can be missed (we were not pumping,
/// the window was being rebuilt); revalidating means a stale handle re-resolves
/// instead of silently addressing a dead window.
#[cfg(target_os = "windows")]
fn resolve_desktop_shell_view() -> Option<(winapi::shared::windef::HWND, winapi::shared::windef::HWND)> {
    use std::sync::atomic::Ordering;
    use winapi::shared::windef::HWND;
    use winapi::um::winuser::IsWindow;

    let defview = DESKTOP_DEFVIEW_HWND.load(Ordering::Relaxed) as HWND;
    let lv = DESKTOP_ICONS_HWND.load(Ordering::Relaxed) as HWND;
    if !defview.is_null()
        && !lv.is_null()
        && unsafe { IsWindow(defview) != 0 && IsWindow(lv) != 0 }
    {
        return Some((defview, lv));
    }

    let (defview, lv) = find_desktop_shell_view()?;
    DESKTOP_DEFVIEW_HWND.store(defview as usize, Ordering::Relaxed);
    DESKTOP_ICONS_HWND.store(lv as usize, Ordering::Relaxed);
    Some((defview, lv))
}

/// [DI-02] Pure decision for the idempotent apply. The shell command is a
/// TOGGLE with no "set" form, so it may only be posted when the live state
/// disagrees with the wanted one. `None` means the state could not be read;
/// posting blind then has a 50% chance of doing the exact opposite of what the
/// user asked, so it must be a no-op. Pure `(Option<bool>, bool) -> bool` so it
/// is unit-tested on every target, not just Windows.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn desktop_icons_need_toggle(current_visible: Option<bool>, wanted_visible: bool) -> bool {
    matches!(current_visible, Some(current) if current != wanted_visible)
}

/// [DI-02] Live desktop icon state. Explorer hides the list-view WINDOW when
/// icons are toggled off, so its visibility IS the state, and it updates the
/// instant the shell handles the command. The `FFlags` shell-bag value Explorer
/// also persists is written lazily, so reading that instead would go stale.
/// `None` when the shell view cannot be resolved.
#[cfg(target_os = "windows")]
fn desktop_icons_visible() -> Option<bool> {
    use winapi::um::winuser::IsWindowVisible;

    let (_, lv) = resolve_desktop_shell_view()?;
    Some(unsafe { IsWindowVisible(lv) != 0 })
}

/// [DI-01] Drive desktop icon visibility through the shell's own command, the
/// same one the desktop context menu posts. Explorer performs the hide AND
/// persists it to its folder-view flags, so the change survives Themia exiting,
/// explorer restarting and reboot — which a bare `ShowWindow(SW_HIDE)` on the
/// list-view (the pre-0.15 approach) did not.
#[cfg(target_os = "windows")]
fn set_desktop_icons_visible(wanted_visible: bool) {
    use winapi::um::winuser::{IsWindowVisible, PostMessageW, WM_COMMAND};

    let Some((defview, lv)) = resolve_desktop_shell_view() else {
        log::warn!(
            target: "desktop-icons",
            "shell desktop view not found; icon visibility not applied"
        );
        return;
    };

    let current = unsafe { IsWindowVisible(lv) != 0 };
    if !desktop_icons_need_toggle(Some(current), wanted_visible) {
        return;
    }

    // PostMessage, not SendMessage: the target window lives in explorer.exe, so
    // a synchronous cross-process call blocks this thread for the shell's whole
    // handling of the command — indefinitely if explorer is wedged.
    let posted = unsafe { PostMessageW(defview, WM_COMMAND, IDM_TOGGLE_DESKTOP_ICONS, 0) };
    if posted == 0 {
        log::warn!(target: "desktop-icons", "PostMessage(toggle desktop icons) failed");
    } else {
        log::info!(
            target: "desktop-icons",
            "toggled desktop icons -> {}",
            if wanted_visible { "shown" } else { "hidden" }
        );
    }
}

/// Set Windows desktop icon visibility. Driven by the Settings toggle only.
/// [DI-03] The result is the shell's OWN persisted setting, exactly as if the
/// user had used the desktop context menu, so Themia never re-asserts or
/// restores it — not on startup, not on shutdown.
#[tauri::command]
fn set_desktop_icons(visible: bool) {
    #[cfg(target_os = "windows")]
    set_desktop_icons_visible(visible);
    #[cfg(not(target_os = "windows"))]
    let _ = visible;
}

/// [DI-04] Read the live desktop icon state so Settings can adopt it. Windows
/// owns this setting now, and the user may have changed it from the desktop
/// context menu while Themia was closed; the stored `desktopIconsVisible` is a
/// first-paint cache, not a second source of truth. `None` on non-Windows or
/// when the shell view cannot be resolved — the caller then keeps what it has.
#[tauri::command]
fn get_desktop_icons() -> Option<bool> {
    #[cfg(target_os = "windows")]
    {
        desktop_icons_visible()
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

/// [DI-06] One-time repair of a pre-0.15 hide. The old path called
/// `ShowWindow(SysListView32, SW_HIDE)` behind Explorer's back, so on every
/// machine that had the setting on, the window is hidden while the shell still
/// believes icons are shown (verified in the field: `IsWindowVisible` false with
/// `HideIcons=0` and `FFlags` NOICONS clear). From that divergent state the
/// [DI-02] apply is correct but useless — asked to SHOW, it sees "hidden", posts
/// the toggle, and the shell flips its own "shown" belief to "hidden", leaving
/// the icons exactly where they were. So the legacy hide is undone first, then
/// re-applied through the shell, which converges the two views without the icons
/// visibly changing state.
///
/// The frontend calls this at most once ever (localStorage migration marker) and
/// only when its stored setting is `false`, i.e. only when Themia is the one that
/// hid them. That guard matters: a hide the USER made from the desktop menu is
/// already the shell's own, indistinguishable from ours by inspection, and must
/// not be touched.
#[tauri::command]
fn repair_legacy_desktop_icon_hide() {
    #[cfg(target_os = "windows")]
    {
        use winapi::um::winuser::{IsWindowVisible, ShowWindow, SW_SHOW};

        let Some((_, lv)) = resolve_desktop_shell_view() else {
            log::warn!(
                target: "desktop-icons",
                "shell desktop view not found; legacy hide not repaired"
            );
            return;
        };
        if unsafe { IsWindowVisible(lv) != 0 } {
            // Already visible: nothing was hidden behind the shell's back
            // (explorer restarted since, or the user showed them by hand).
            return;
        }
        log::info!(target: "desktop-icons", "repairing pre-0.15 SW_HIDE; re-hiding via the shell");
        unsafe { ShowWindow(lv, SW_SHOW) };
        set_desktop_icons_visible(false);
    }
}

#[cfg(test)]
mod desktop_icon_decision_tests {
    use super::desktop_icons_need_toggle;

    /// [DI-02] The shell command is a toggle, so a state that already matches
    /// must post NOTHING. Characterizes the idempotence the old
    /// `ShowWindow(SW_SHOW/SW_HIDE)` got for free and this path has to earn:
    /// without the guard, every re-apply would flip icons back and forth.
    #[test]
    fn di_02_matching_state_posts_nothing() {
        assert!(!desktop_icons_need_toggle(Some(true), true));
        assert!(!desktop_icons_need_toggle(Some(false), false));
    }

    /// [DI-02] A genuine mismatch is the only case that posts the command.
    #[test]
    fn di_02_mismatched_state_toggles() {
        assert!(desktop_icons_need_toggle(Some(true), false));
        assert!(desktop_icons_need_toggle(Some(false), true));
    }

    /// [DI-02] Unreadable state must never post a blind toggle: with no "set"
    /// form, guessing has a 50% chance of hiding the icons of a user who just
    /// asked to show them.
    #[test]
    fn di_02_unknown_state_never_toggles() {
        assert!(!desktop_icons_need_toggle(None, true));
        assert!(!desktop_icons_need_toggle(None, false));
    }
}

/// Deselect every icon in the shell desktop list-view. Called from the
/// frontend on any mousedown inside a Themia widget so clicking into the
/// overlay clears desktop selection (mirrors how clicking the desktop
/// clears Themia selections via the foreground hook). Cross-process:
/// LVITEMW must live in Explorer's address space, so we allocate it
/// remotely with VirtualAllocEx before SendMessage(LVM_SETITEMSTATE).
#[cfg(target_os = "windows")]
fn clear_desktop_selection_impl() {
    use std::ptr;
    use std::mem::size_of;
    use winapi::um::winuser::{SendMessageW, GetWindowThreadProcessId};
    use winapi::um::processthreadsapi::OpenProcess;
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::memoryapi::{VirtualAllocEx, VirtualFreeEx, WriteProcessMemory};
    use winapi::um::winnt::{
        PROCESS_VM_OPERATION, PROCESS_VM_WRITE, MEM_COMMIT, MEM_RESERVE, MEM_RELEASE, PAGE_READWRITE,
    };
    use winapi::um::commctrl::{LVITEMW, LVM_SETITEMSTATE, LVIF_STATE, LVIS_SELECTED};

    // [DI-05] Shared lazy resolve + revalidation with set_desktop_icons_visible.
    let Some((_, lv)) = resolve_desktop_shell_view() else {
        return;
    };

    unsafe {
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(lv, &mut pid);
        if pid == 0 {
            return;
        }

        let proc_handle = OpenProcess(PROCESS_VM_OPERATION | PROCESS_VM_WRITE, 0, pid);
        if proc_handle.is_null() {
            return;
        }

        let remote = VirtualAllocEx(
            proc_handle,
            ptr::null_mut(),
            size_of::<LVITEMW>(),
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        );
        if remote.is_null() {
            CloseHandle(proc_handle);
            return;
        }

        let mut item: LVITEMW = std::mem::zeroed();
        item.mask = LVIF_STATE;
        item.state = 0;
        item.stateMask = LVIS_SELECTED;

        let mut written: usize = 0;
        let ok = WriteProcessMemory(
            proc_handle,
            remote,
            &item as *const _ as *const _,
            size_of::<LVITEMW>(),
            &mut written,
        );
        if ok != 0 {
            // wParam = -1 (usize::MAX): set state for all items.
            SendMessageW(lv, LVM_SETITEMSTATE, usize::MAX, remote as isize);
        }

        VirtualFreeEx(proc_handle, remote, 0, MEM_RELEASE);
        CloseHandle(proc_handle);
    }
}

#[tauri::command]
fn clear_desktop_selection() {
    #[cfg(target_os = "windows")]
    clear_desktop_selection_impl();
}

// ---------------------------------------------------------------------------
// Logging pipeline ([LOG-NN] in SPEC.md)
// ---------------------------------------------------------------------------

/// [LOG-01] Rotated-segment cap. KeepAll once accumulated 628 files / 2.8 GB
/// in %LOCALAPPDATA%\com.themia.desktop\logs; with max_file_size 5 MB this
/// bounds the log dir to ~50 MB.
const LOG_KEEP_FILES: usize = 10;

fn log_rotation_strategy() -> tauri_plugin_log::RotationStrategy {
    tauri_plugin_log::RotationStrategy::KeepSome(LOG_KEEP_FILES)
}

/// [LOG-07] Log directory, resolved the same marker-file way as
/// [`logging_marker_path`] so the sweep can run at the very top of `run()`,
/// before any AppHandle exists.
fn log_dir_path() -> Option<std::path::PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| {
        std::path::PathBuf::from(d)
            .join("com.themia.desktop")
            .join("logs")
    })
}

/// [LOG-07] Is this one of OUR rotated log files, and therefore ours to delete?
///
/// Deliberately narrow: the sweep runs against a real user directory, so
/// anything that is not recognisably a Themia log segment is left alone.
/// Matches the current `themia*.log` naming, the pre-0.13 `Themia_*.log`
/// casing, and the `.log.bak` segments the plugin leaves behind — the three
/// shapes actually observed in the field.
fn is_themia_log_segment(file_name: &str) -> bool {
    let lower = file_name.to_ascii_lowercase();
    lower.starts_with("themia") && (lower.ends_with(".log") || lower.ends_with(".log.bak"))
}

/// [LOG-07] Which segments to delete, newest `keep` retained.
///
/// Pure so the retention rule is testable without touching a real log dir.
/// `entries` is `(file_name, modified)`; ordering is by `modified` descending,
/// with the file name as a stable tiebreak so two segments written in the same
/// clock tick can't swap places between runs.
fn plan_log_sweep(mut entries: Vec<(String, std::time::SystemTime)>, keep: usize) -> Vec<String> {
    entries.retain(|(name, _)| is_themia_log_segment(name));
    entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    entries.into_iter().skip(keep).map(|(name, _)| name).collect()
}

/// [LOG-07] One-time reclaim of pre-[LOG-01] logs.
///
/// `RotationStrategy::KeepSome` only prunes what the CURRENT run rotates, so
/// every machine upgraded from a `KeepAll` build still carries its whole
/// history: 319 files / 1.54 GB measured on a dev machine, 628 files / 2.8 GB
/// in the report that motivated [LOG-01]. Runs before the log plugin is built,
/// so the fresh session's own file is never a candidate. Best-effort
/// throughout: a log directory we cannot read or prune is not worth failing a
/// boot over.
fn sweep_stale_logs(dir: &std::path::Path, keep: usize) {
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) => {
            // NotFound is the fresh-install case, not a fault.
            if e.kind() != std::io::ErrorKind::NotFound {
                log::warn!(target: "logs", "[LOG-07] cannot read log dir {}: {e}", dir.display());
            }
            return;
        }
    };

    let mut entries: Vec<(String, std::time::SystemTime)> = Vec::new();
    for entry in read.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let Ok(modified) = meta.modified() else { continue };
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
        entries.push((name, modified));
    }

    let doomed = plan_log_sweep(entries, keep);
    if doomed.is_empty() {
        return;
    }

    let mut removed = 0usize;
    let mut bytes = 0u64;
    for name in &doomed {
        let path = dir.join(name);
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        match std::fs::remove_file(&path) {
            Ok(()) => {
                removed += 1;
                bytes += size;
            }
            Err(e) => {
                log::warn!(target: "logs", "[LOG-07] could not remove {}: {e}", path.display());
            }
        }
    }
    log::info!(
        target: "logs",
        "[LOG-07] swept {removed} stale log segment(s), {} MB reclaimed, {keep} kept",
        bytes / 1_000_000
    );
}

/// [LOG-02] Number of open Settings > Logs pages across all windows. The
/// Webview target re-broadcasts every record to ALL windows via eval, so
/// INFO/DEBUG only flow while someone is actually looking.
static LOG_STREAM_VIEWERS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// [LOG-02] WARN+ always forwards (rare, and the Logs page must show errors
/// from before it was opened); INFO and below need an open viewer.
fn webview_forward_allowed(level: log::Level, viewers: usize) -> bool {
    level <= log::Level::Warn || viewers > 0
}

/// [LOG-02] Saturating viewer-count step: an unbalanced close (a window
/// killed before its close call lands) must never underflow and wrap, which
/// would pin forwarding on forever.
fn next_viewer_count(current: usize, open: bool) -> usize {
    if open { current.saturating_add(1) } else { current.saturating_sub(1) }
}

/// [LOG-02] Called by the Settings > Logs page on open/close.
#[tauri::command]
fn set_log_stream(open: bool) {
    use std::sync::atomic::Ordering;
    // fetch_update never fails when the closure returns Some.
    let _ = LOG_STREAM_VIEWERS
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| Some(next_viewer_count(v, open)));
    if open {
        // [LOG-06] Emit AFTER the counter rises so the [LOG-02] gate lets this
        // one through: a viewer that opens on a quiet app must see proof the
        // pipeline works rather than an empty list it reads as "broken".
        log::info!(target: "logs", "log viewer attached");
    }
}

/// [LOG-06] Crate roots whose module-path targets are our own code. A bare
/// `log::debug!` (no `target:`) is tagged with the module path, so
/// `widget_netproxy::winhttp` must stay uncapped while `rustls::client::hs`
/// must not.
fn is_own_log_target(target: &str) -> bool {
    // Frontend records arrive as "webview:console.debug@…/foo.ts:10:3" — a
    // flat target, but matched by prefix so a stack frame that happens to
    // contain "::" can never be mistaken for a dependency.
    if target.starts_with("webview") {
        return true;
    }
    match target.split_once("::") {
        // Flat custom target ("drag_drop", "folder:icons", "app"): ours.
        None => true,
        Some((root, _)) => root == "themia" || root == "themia_lib" || root.starts_with("widget_"),
    }
}

/// [LOG-06] Dependency-noise cap. Dependencies stop at INFO; our own code
/// logs down to DEBUG. Before this, release builds capped EVERYTHING at INFO,
/// which silently dropped every `debugLog()`/`log::debug!` the app emits —
/// so a user who enabled logging saw an empty Logs page (issue #85). Lifting
/// the cap wholesale is not an option either: one session produced 8.5k
/// `rustls` DEBUG lines against 2.5k of ours, which would bury the app's own
/// records and churn the [LOG-01] 50 MB budget.
fn log_target_allowed(target: &str, level: log::Level) -> bool {
    level <= log::Level::Info || is_own_log_target(target)
}

/// [LOG-04] Master switch for the whole log pipeline. Logging is opt-in: the
/// gate is seeded from the persisted marker at the top of `run()` (closed on
/// fresh installs) and the frontend re-pushes the dev toggle (Settings >
/// Developer > Logging) on startup and on change. While closed no record
/// reaches any release target, file or webview (the Stdout target exists
/// only in debug builds, which bypass the gate entirely, as do `npm run dev`
/// and the e2e suite).
static LOGGING_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// [LOG-04] Pure truth table for the master gate (testable: `cfg!` is baked
/// into the caller, not this fn). ERROR is the floor and always logs: a
/// failure on a release machine must leave a trace on disk without a prior
/// opt-in, because "turn logging on and reproduce it" is not a diagnosis (and
/// a panic is exactly the thing that does not reproduce on demand). WARN and
/// below still need the toggle or a debug build; that is what keeps the
/// [LOG-01] budget and the [LOG-06] DEBUG verbosity affordable.
fn logging_allowed(level: log::Level, user_enabled: bool, debug_build: bool) -> bool {
    debug_build || user_enabled || level == log::Level::Error
}

/// [LOG-04] Marker-file location: presence = logging opted in. Lives under
/// `%LOCALAPPDATA%\com.themia.desktop` (the identifier pinned in
/// tauri.conf.json) because the gate must be seeded at the very top of
/// `run()`, before any AppHandle exists to resolve paths.
fn logging_marker_path() -> Option<std::path::PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| {
        std::path::PathBuf::from(d)
            .join("com.themia.desktop")
            .join("logging-enabled.marker")
    })
}

/// [LOG-04] Marker presence is the persisted opt-in for the NEXT boot.
fn read_logging_marker(path: &std::path::Path) -> bool {
    path.exists()
}

/// [LOG-04] Create/remove the marker. Best-effort: a failed write only costs
/// the next boot's startup records (the in-process gate is already set), so
/// errors are logged, never surfaced.
fn write_logging_marker(path: &std::path::Path, enabled: bool) {
    if enabled {
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                log::warn!("logging marker dir create failed: {e}");
                return;
            }
        }
        if let Err(e) = std::fs::write(path, b"") {
            log::warn!("logging marker write failed: {e}");
        }
    } else if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            log::warn!("logging marker remove failed: {e}");
        }
    }
}

/// [LOG-04] Flip the in-process gate and (when a marker path is given)
/// persist the opt-in for the next boot. Marker path is injected so tests
/// never touch the real `%LOCALAPPDATA%` install state.
fn apply_logging_enabled(enabled: bool, marker: Option<&std::path::Path>) {
    LOGGING_ENABLED.store(enabled, std::sync::atomic::Ordering::Relaxed);
    if let Some(path) = marker {
        write_logging_marker(path, enabled);
    }
}

/// [LOG-04] Pushed by the frontend from the persisted design store on startup
/// and whenever the user flips Settings > Developer > Logging. Also persists
/// the marker so the gate is already correct at the top of the next boot.
#[tauri::command]
fn set_logging_enabled(enabled: bool) {
    apply_logging_enabled(enabled, logging_marker_path().as_deref());
}

/// [LOG-08] Open the log directory in Explorer, from the Settings > Logs page.
///
/// The page's list is this session's in-memory buffer; the rotated files under
/// `%LOCALAPPDATA%\com.themia.desktop\logs` are what a user actually attaches
/// to a bug report, and they hold the previous sessions the buffer never saw.
/// The directory is created first: with logging off (the [LOG-04] default) the
/// plugin may never have written a file, and Explorer given a missing path
/// silently opens Documents instead of reporting anything.
#[tauri::command]
fn open_log_folder() -> Result<(), String> {
    let dir = log_dir_path().ok_or("LOCALAPPDATA is not set")?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = dir.to_str().ok_or("log directory path is not valid UTF-8")?;
    widget_folder::open_in_explorer(path)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
/// Log every panic through the `log` facade (so tauri-plugin-log captures it to
/// the rotating file + the Settings > Logs panel) before delegating to the
/// default hook, which still prints the backtrace to stderr in debug builds. A
/// panic on any thread, including a detached background worker, is otherwise
/// invisible in a release build. (1.18)
///
/// Emitted at ERROR deliberately: that is the [LOG-04] floor, so the record is
/// persisted on a release machine even with the logging toggle off.
fn install_panic_logger() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown location>".to_string());
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());
        let thread = std::thread::current();
        let tname = thread.name().unwrap_or("<unnamed>");
        log::error!("panic on thread '{tname}' at {location}: {msg}");
        default_hook(info);
    }));
}

pub fn run() {
    // Install the panic logger first so a panic during startup is still logged.
    install_panic_logger();

    // [LOG-04] Seed the master gate from the persisted marker BEFORE the log
    // plugin is built: an opted-in user's startup records, including setup
    // panics (the very thing the panic logger above exists for), must be
    // captured from the first record. Defaults closed when the marker is
    // absent (fresh install / logging off).
    if let Some(path) = logging_marker_path() {
        LOGGING_ENABLED.store(
            read_logging_marker(&path),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    // [LOG-07] Reclaim segments left by pre-[LOG-01] `KeepAll` builds. Before
    // the plugin is built, so this session's own file is not yet a candidate.
    if let Some(dir) = log_dir_path() {
        sweep_stale_logs(&dir, LOG_KEEP_FILES);
    }

    let log_builder = tauri_plugin_log::Builder::new()
        .target(tauri_plugin_log::Target::new(
            tauri_plugin_log::TargetKind::LogDir { file_name: Some("themia".into()) },
        ))
        .target(
            tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Webview)
                // [LOG-02] Release builds gate INFO/DEBUG on an open Logs
                // viewer; debug builds forward everything (DevTools echo and
                // the e2e logs-panel spec rely on it).
                .filter(|meta| {
                    cfg!(debug_assertions)
                        || webview_forward_allowed(
                            meta.level(),
                            LOG_STREAM_VIEWERS.load(std::sync::atomic::Ordering::Relaxed),
                        )
                }),
        )
        .max_file_size(5_000_000)
        // [LOG-01] Never KeepAll: the log dir must stay bounded.
        .rotation_strategy(log_rotation_strategy())
        .timezone_strategy(tauri_plugin_log::TimezoneStrategy::UseLocal)
        // [LOG-06] One level policy for every build: DEBUG, so opting in
        // actually yields the app's diagnostics. The dependency cap below is
        // what keeps that affordable.
        .level(log::LevelFilter::Debug)
        // [LOG-06] Dependency-noise cap, applied before the master gate so a
        // dependency's DEBUG record costs nothing beyond this check.
        .filter(|meta| log_target_allowed(meta.target(), meta.level()))
        // [LOG-04] Dispatch-wide master gate (every target): with the dev
        // toggle off a release build keeps ERROR only — panics included, via
        // the hook below — and drops WARN and below until the toggle opens the
        // gate. WARN-and-below records from before the frontend pushes the
        // persisted value are dropped; that is the contract.
        .filter(|meta| {
            logging_allowed(
                meta.level(),
                LOGGING_ENABLED.load(std::sync::atomic::Ordering::Relaxed),
                cfg!(debug_assertions),
            )
        });

    // [LOG-09] Mirror ERROR (and only ERROR — its own level filter, so this
    // target stays quiet even with the toggle on) into the Windows Application
    // event log. Every other target is a file or a window the user has to know
    // about; Event Viewer is the one place a support request can point at by
    // name, and it is where the OS already files the WER `Application Error`
    // entry for a hard crash, so both halves land together.
    let log_builder = log_builder.target(tauri_plugin_log::Target::new(
        tauri_plugin_log::TargetKind::Dispatch(
            tauri_plugin_log::fern::Dispatch::new()
                .level(log::LevelFilter::Error)
                .chain(tauri_plugin_log::fern::Output::call(|record| {
                    eventlog::report_error(&format!("[{}] {}", record.target(), record.args()));
                })),
        ),
    ));

    // Stdout (VS Code / `npm run dev`) is the one target that stays
    // debug-only; the level policy above is shared by both builds ([LOG-06]).
    #[cfg(debug_assertions)]
    let log_builder = log_builder.target(tauri_plugin_log::Target::new(
        tauri_plugin_log::TargetKind::Stdout,
    ));

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            // A second launch is funneled here, into the already-running
            // instance, with the second process's argv. Autostart only ever
            // launches once (at login, when nothing is running), so this is
            // effectively always a manual launch, but check the flag anyway.
            let from_autostart = args.iter().any(|a| a == AUTOSTART_FLAG);
            if !from_autostart {
                show_config_window(app, "monitor-0", None);
            }
        }))
        .plugin(log_builder.build())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            // Stamp the running version into the one shared User-Agent
            // (netproxy [PX-10]) before anything can issue a request: no window
            // exists yet, so no widget has fetched anything.
            widget_netproxy::set_app_version(&app.package_info().version.to_string());

            // Enable autostart on first run (quoted path for "Program Files"
            // spaces). If a task already exists but predates the --autostart
            // argument, refresh it once so login launches stay distinguishable
            // from manual ones.
            #[cfg(target_os = "windows")]
            {
                // Enable when absent, or refresh an existing task that predates
                // the --autostart arg. Both cases call enable(). This first-run /
                // migration enable is best-effort: log a failure (e.g. Task
                // Scheduler restricted by policy) but never block startup. The
                // user can retry from Settings, where the same error (#69/#79)
                // is surfaced inline instead of being swallowed ([AS-05]).
                //
                // `task_exists`, NOT `is_enabled` ([AS-07]): a task the user
                // switched off in Task Scheduler still exists, and recreating it
                // here would override that choice on every launch.
                if !autostart::task_exists() || autostart::needs_arg_migration() {
                    if let Err(e) = autostart::enable() {
                        log::warn!("autostart: failed to enable on startup: {e}");
                    }
                }
            }

            // Start the display watcher BEFORE the first overlay build
            // ([DW-05]). An --autostart launch can run before explorer has
            // docked the taskbar: rcWork still spans the full monitor, so the
            // first pin overlaps the taskbar's eventual band. The
            // TaskbarCreated / SPI_SETWORKAREA broadcasts that follow only
            // heal it if we are already listening; starting the watcher
            // after the builds left a gap where they were missed and the
            // overlay kept taskbar-overlapping bounds until a manual restart.
            #[cfg(target_os = "windows")]
            {
                WATCHER_APP.set(app.handle().clone()).ok();
                start_display_watcher();
            }

            let monitors = enumerate_monitors();
            if monitors.is_empty() {
                // [DW-07] Build NOTHING. The old code built an unconditional
                // 1920x1080 window at (0,0) — the primary's origin — under a
                // bogus "FALLBACK" device name, and never consulted the
                // disabled list: a fail-open onto the primary, plus a phantom
                // key in the frontend's per-monitor maps. It was reachable in
                // practice, not theoretically: the autostart task's
                // LogonTrigger has no delay ([AS-01]/[AS-02]), so this
                // enumeration runs at the least-settled moment of the boot.
                // The watcher is already running ([DW-05]) and re-enumerates
                // on WM_DISPLAYCHANGE / the work-area sweep ([DW-02]/[DW-04]),
                // which builds the overlays once the displays actually exist
                // ([DW-06]). No wall-clock retry loop, no phantom window.
                log::error!(
                    target: "displays",
                    "startup: no monitors enumerated. Building no overlays; the display \
                     watcher will build them when the displays appear ([DW-07])."
                );
            }

            // [MI-03] One-time re-key of the persisted disable list from
            // `\\.\DISPLAYn` ordinals onto stable ids. Must precede the read
            // below. A no-op once the file carries the current schemaVersion,
            // and a no-op right now if `monitors` is empty (nothing to resolve
            // against), in which case the pass above will run it later.
            migrate_disabled_monitors_file(app.handle(), &monitors);

            // Persisted per-monitor disable list ([MI-01] stable ids):
            // overlays for these monitors are skipped entirely (no HWND, no
            // WebView2 process, no resource cost). Runtime toggle goes through
            // `sync_disabled_monitors` → `reconcile_disabled_windows`, which
            // builds/closes to match changes.
            let disabled = read_disabled_monitors(app.handle());

            // Same decision function as every later reconcile pass: at startup
            // no overlay exists yet, so `window_exists` is constant-false and
            // the plan is exactly "build every enabled monitor" ([MI-05]).
            // [MI-06] Nothing is bound yet at startup, so slots fall out as
            // `0..monitors.len()` in enumeration order — the pre-#93 behavior.
            // From here on they follow the panel, not the enumeration index.
            let slots = assign_monitor_slots(&monitors, &std::collections::HashMap::new());
            for action in plan_reconcile(&monitors, &slots, &disabled, |_| false) {
                if let ReconcileAction::Build { index, monitor_id, x, y, width, height } = action {
                    build_widget_window(
                        app.handle(), index, x, y, width, height, &monitor_id, OverlayLayer::Base,
                    )?;
                }
            }

            #[cfg(target_os = "windows")]
            {
                // Click-through polling intentionally disabled, see comment
                // in pin_to_desktop. Real-world widget interactivity regressed
                // even though unit + Win32 integration tests passed.
                // start_click_through_thread();
                //
                // [DI-03] Desktop icon visibility is deliberately NOT touched at
                // startup. It is the shell's own persisted setting now, so
                // forcing it visible here would undo a hide the user made from
                // the desktop context menu (or from Settings in an earlier run).
            }

            // Anonymous usage analytics (opt-out, on by default) is driven by
            // the frontend, which runs TelemetryDeck's official SDK and asks
            // `analytics_plan` what it may send. Consent and cadence stay in
            // the backend; there is nothing to start here. See docs/ANALYTICS.md.

            // Screen-time tracking: load the persisted store and start the
            // accrual sampler (screentime SPEC [S-22]). Foreground switches
            // arrive via the existing EVENT_SYSTEM_FOREGROUND hook.
            widget_screentime::init(app.handle());

            // Manual launch (Start menu, Explorer, double-click) opens the
            // settings window. An autostart login launch passes --autostart
            // and stays silent. The "already running" case is handled by the
            // single-instance callback above instead of here.
            let from_autostart = std::env::args().any(|a| a == AUTOSTART_FLAG);
            if !from_autostart {
                show_config_window(app.handle(), "monitor-0", None);
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            autostart_is_enabled,
            autostart_can_configure,
            autostart_enable,
            autostart_disable,
            push_windows_to_desktop,
            set_desktop_icons,
            get_desktop_icons,
            repair_legacy_desktop_icon_hide,
            clear_desktop_selection,
            set_log_stream,
            set_logging_enabled,
            open_log_folder,
            open_in_explorer,
            open_file,
            resolve_shortcut,
            open_url,
            quit_app,
            get_system_locale,
            list_system_fonts,
            get_file_icon,
            get_file_icons_batch,
            fs_paste,
            fs_delete,
            fs_delete_permanent,
            fs_exists,
            recycle_bin_status,
            fs_restore_from_trash,
            fs_rename,
            show_file_properties,
            open_with,
            fs_write_file,
            get_folder_item_counts,
            get_image_thumbnail,
            get_image_thumbnails_batch,
            fs_download_url,
            set_ssrf_bypass,
            get_clipboard_url,
            clipboard_set_files,
            clipboard_get_files,
            get_cursor_state,
            activate_widget_keyboard,
            deactivate_widget_keyboard,
            set_text_input_focused,
            set_escape_wanted,
            dismiss_start_menu_if_open,
            dismiss_context_menu_if_open,
            get_cloud_sync_folders,
            get_shell_new_entries,
            fs_create_folder,
            get_shell_menu_entries,
            execute_shell_command,
            get_file_attributes,
            get_file_attributes_batch,
            folder_read_dir,
            get_file_stats_batch,
            set_cloud_pin,
            get_git_status,
            get_git_repo_info_batch,
            email_list_accounts,
            email_start_device_auth,
            email_poll_auth,
            email_start_github_device_auth,
            email_poll_github_device_auth,
            email_start_google_auth,
            email_complete_google_auth,
            email_get_messages,
            email_sign_out_account,
            email_rename_account,
            email_get_folder_status,
            email_mark_message_read,
            email_delete_message,
            email_move_message,
            email_open_url,
            email_open_message,
            email_reply_to_message,
            email_probe_endpoint,
            email_add_imap_account,
            email_add_caldav_account,
            email_add_carddav_account,
            email_add_github_account,
            email_add_rss_feed,
            email_get_github_token,
            email_get_github_api_base_url,
            email_start_claude_auth,
            email_complete_claude_auth,
            email_get_claude_usage,
            show_devtools,
            close_devtools,
            open_config_window,
            open_connect_window,
            open_layout_window,
            open_layout_app_picker_window,
            open_shortcut_add_window,
            open_widget_window,
            open_web_element_picker_window,
            web_element_render_html,
            set_interactive,
            set_pin_layer,
            sync_disabled_monitors,
            update_widget_rects,
            get_system_stats,
            screentime_get_day,
            screentime_get_range,
            list_gpus,
            get_gpu_stats,
            ping_hosts,
            volume_get,
            volume_set,
            volume_set_mute,
            mixer_get_sessions,
            mixer_set_volume,
            mixer_set_mute,
            power_run,
            music_get_now_playing,
            music_play_pause,
            music_next,
            music_previous,
            music_seek,
            music_start_watcher,
            music_start_spectrum,
            music_stop_spectrum,
            clipboard_start,
            clipboard_set,
            calendar_get_events,
            calendar_list_calendars,
            calendar_get_birthdays,
            calendar_open_event,
            todo_get_lists,
            todo_get_tasks,
            todo_update_task,
            todo_rename_task,
            todo_create_task,
            todo_delete_task,
            contacts_list,
            onedrive_recent_files,
            onedrive_list_folder,
            get_battery_stats,
            get_bluetooth_devices,
            list_start_menu_apps,
            launch_app_layout,
            scan_images,
            read_image_base64,
            notes_list,
            notes_read,
            notes_write,
            notes_create,
            notes_rename,
            notes_delete,
            http_get,
            get_monitor_id,
            get_active_monitors,
            license_activate,
            license_validate,
            license_deactivate,
            license_status,
            license_get_key,
            newsletter_subscribe,
            drag_drop::start_native_drag,
            drag_drop::save_virtual_files,
            drag_drop::clipboard_extract_virtual_files,
            drag_drop::set_drag_over_zone,
            game_mode::set_game_mode_watcher,
            diag_window_state,
            diag_send_show_desktop,
            diag_click_and_measure_start_close,
            diag_toggle_desktop,
            diag_z_counters,
            diag_reset_counters,
            diag_compare_zorder,
            diag_window_above,
            diag_close_windows_by_prefix,
            diag_demote_widget,
            diag_cloak_widget,
            diag_set_polling_paused,
            diag_list_widgets,
            diag_force_topmost,
            analytics::analytics_consent,
            analytics::analytics_set_consent,
            analytics::analytics_claim_consent_prompt,
            analytics::analytics_plan,
            analytics::analytics_track,
            analytics::analytics_session_end,
            analytics::analytics_mark_sent,
            analytics::diag_analytics_test,
            tray::tray_env,
            tray::tray_sync,
            tray::tray_flyout_hide,
            tray::tray_flyout_ready,
            tray::tray_flyout_preview,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| match event {
            // [EX-01] The runtime raises this the moment the window map goes
            // empty (tauri-runtime-wry, `TaoWindowEvent::Destroyed`). For an
            // app whose every window is a disposable per-monitor overlay that
            // is a transient, not a shutdown, so refuse it unless `quit_app`
            // armed the flag. Without this arm the default is
            // `ControlFlow::Exit` and the process vanishes silently (#87).
            tauri::RunEvent::ExitRequested { api, .. } => {
                if !exit_request_should_be_honoured(
                    APP_QUITTING.load(std::sync::atomic::Ordering::SeqCst),
                ) {
                    api.prevent_exit();
                    log::error!(
                        target: "displays",
                        "[EX-01] exit request refused: window count reached zero without a \
                         quit_app. Requesting an overlay rebuild pass."
                    );
                    // Nothing is on screen right now and there is no tray icon,
                    // so the rebuild is the only way back. The debounced
                    // display pass re-runs the [DW-06] build for every enabled
                    // monitor.
                    #[cfg(target_os = "windows")]
                    request_display_pass();
                }
            }
            tauri::RunEvent::Exit => {
                // Final screen-time flush so the last open interval survives
                // shutdown (screentime SPEC [S-29]).
                widget_screentime::flush();
                // Tray SPEC [S-18]: hand every notification icon back to the
                // shell. This runs before Tauri's `cleanup_before_exit`, and
                // the tray's own state is a `static` that process exit never
                // drops, so it is the only point at which the last handle can
                // still be released.
                tray::shutdown(app);
                // [DI-03] No desktop-icon restore on exit: the hide is the
                // shell's own persisted setting and must outlive the app,
                // exactly as it does when set from the desktop context menu.
            }
            _ => {}
        });
}

/// Browser arguments applied to EVERY WebView2 window. `additional_browser_args`
/// REPLACES wry's default arg string rather than appending, so we must repeat
/// wry's default `--disable-features=...` here. On top of that:
///   * `--js-flags=--max-old-space-size=192` bounds each renderer's V8 old-space
///     heap at 192 MB, a ceiling on runaway growth / leaks in the long-lived
///     overlay windows (one full renderer process per monitor).
///   * `--autoplay-policy=no-user-gesture-required` lets the opt-in startup sound
///     (src/SPEC.md [S-26]) play at window boot; there is no DOM user gesture
///     yet, so WebView2's default policy would park the AudioContext suspended.
///
/// [WV-02] Nothing here may declare the machine memory-constrained.
/// `--enable-low-end-device-mode` lived in this string until 0.15.2 and is what
/// clipped widget fills, widget blur and the settings-window background to
/// raster-tile rectangles on large / hiDPI displays (themia-releases#124, #43,
/// #108). Renderer memory is bounded by the V8 cap above, never by trading away
/// the raster budget.
const WEBVIEW_BROWSER_ARGS: &str = concat!(
    "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection",
    " --js-flags=--max-old-space-size=192",
    " --autoplay-policy=no-user-gesture-required",
);

fn build_widget_window(
    app: &tauri::AppHandle,
    index: usize,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    // [MI-01] The monitor's STABLE id, never its `\\.\DISPLAYn` ordinal: this
    // is what `get_monitor_id` hands the frontend to key its per-monitor
    // state on.
    monitor_id: &str,
    // [PL-01] Base overlay or the monitor's always-on-top pin overlay. Both
    // carry the SAME monitor id, so the pin layer resolves the same per-monitor
    // view assignment and widget scale as the base overlay it shadows.
    layer: OverlayLayer,
) -> Result<(), Box<dyn std::error::Error>> {
    let label = overlay_label(index, layer);

    #[cfg(target_os = "windows")]
    window_monitor_map().lock().unwrap_or_else(|e| e.into_inner()).insert(label.clone(), monitor_id.to_string());
    #[cfg(not(target_os = "windows"))]
    let _ = monitor_id;

    // Position/size are intentionally omitted from the builder: SetWindowPos
    // below uses raw physical pixels and is the single source of truth.
    let window = WebviewWindowBuilder::new(
        app,
        &label,
        WebviewUrl::App("index.html".into()),
    )
    .additional_browser_args(WEBVIEW_BROWSER_ARGS)
    // Explicit title, without it the OS title defaults to "Tauri App",
    // which flashes in the frame during the gap before the webview's
    // <title> loads (and in any state where the frame isn't fully stripped).
    .title("Themia")
    .decorations(false)
    .transparent(true)
    .resizable(false)
    .shadow(false)
    .skip_taskbar(true)
    .visible(false)
    // [WA-01] Never take the foreground at creation. `.visible(false)` is NOT
    // enough: tauri maps `focused` onto the WEBVIEW too
    // (tauri-runtime-wry `.with_focused(webview_attributes.focus)`), and wry
    // then calls `controller.MoveFocus(PROGRAMMATIC)` while building the
    // WebView2 controller. That SetFocus activates the still-hidden overlay,
    // so every launch yanked the foreground away from whatever the user was
    // typing in — invisibly, since the window itself stays hidden, and before
    // `pin_to_desktop` has had a chance to apply WS_EX_NOACTIVATE.
    .focused(false)
    // Born WS_EX_TOPMOST. Windows allows secondary-monitor windows to be
    // re-promoted to topmost via SetWindowPos only if they were originally
    // created topmost; without this flag, monitor-1+ silently rejects
    // every later HWND_TOPMOST attempt. We immediately drop topmost in
    // pin_to_desktop's finishing pass so widgets sit below normal apps in
    // the steady state; the polling thread re-promotes during Show Desktop.
    .always_on_top(true)
    .build()?;


    #[cfg(target_os = "windows")]
    pin_to_desktop(&window, x, y, width, height, layer)?;

    // Register as OLE drop target for native drag/drop
    #[cfg(target_os = "windows")]
    {
        let hwnd = window.hwnd()?.0 as isize;
        let dpr = window.scale_factor().unwrap_or(1.0);
        if let Err(e) = drag_drop::register_drop_target(app.clone(), &label, hwnd, x, y, dpr) {
            log::error!(target: "drag_drop", "Failed to register drop target for {label}: {e}");
        }
    }

    // [WV-01] Arm crash recovery. Nobody ever closes or reopens an overlay, so
    // a dead renderer would otherwise leave this monitor blank until the user
    // restarts the app.
    #[cfg(target_os = "windows")]
    {
        let recovery_label = label.clone();
        if let Err(e) = window.with_webview(move |pw| {
            register_process_failed_handler(pw, recovery_label)
        }) {
            log::error!(
                target: "displays",
                "[WV-01] {label}: could not reach the platform webview, crash recovery not armed: {e}"
            );
        }
    }

    // Windows: the window is intentionally NOT shown here. It stays hidden
    // (`.visible(false)` on the builder) until it is region-clipped to the
    // widget shapes, otherwise the unclipped OS frame ("grey top bar")
    // flashes during the async gap before the frontend pushes its first
    // rect set. Two paths un-hide it, first wins (guarded by
    // `show_overlay_once`):
    //   1. The first `update_widget_rects` that applies a NON-EMPTY region.
    //   2. This fallback timer, for windows that never get widgets (zero
    //      configured widgets / monitor disabled → only empty rect lists).
    #[cfg(target_os = "windows")]
    {
        let app_for_thread = app.clone();
        let label_for_timer = label.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(1500));
            let app_for_main = app_for_thread.clone();
            let _ = app_for_thread.run_on_main_thread(move || {
                use tauri::Manager;
                if let Some(win) = app_for_main.get_webview_window(&label_for_timer) {
                    if let Ok(h) = win.hwnd() {
                        if show_overlay_once(h.0 as usize) {
                            log::info!(
                                target: "overlay",
                                "fallback timer showed {label_for_timer} (no widget rects pushed)"
                            );
                        }
                    }
                }
            });
        });
    }
    #[cfg(not(target_os = "windows"))]
    window.show()?;

    // Always re-anchor z-order to "just above Progman" when focus arrives,
    // even in interactive mode the widget should sit below every real app
    // window but above the desktop, so Show Desktop never covers it.
    #[cfg(target_os = "windows")]
    {
        let win_clone = window.clone();
        let hwnd_usize = window.hwnd().map(|h| h.0 as usize).unwrap_or(0);
        window.on_window_event(move |event| match event {
            tauri::WindowEvent::Focused(true) => {
                if let Ok(h) = win_clone.hwnd() {
                    let hwnd = h.0 as usize as winapi::shared::windef::HWND;
                    if !hwnd.is_null() {
                        unsafe {
                            winapi::um::winuser::SetWindowPos(
                                hwnd,
                                z_anchor_for(hwnd),
                                0, 0, 0, 0,
                                winapi::um::winuser::SWP_NOMOVE
                                    | winapi::um::winuser::SWP_NOSIZE
                                    | winapi::um::winuser::SWP_NOACTIVATE,
                            );
                            // The overlay activates when the user interacts
                            // with a focusable widget surface (e.g. clicking
                            // into the notes textarea to select text). While
                            // interactive the window region is cleared, so the
                            // region clip is no longer hiding any residual OS
                            // frame, and activation is exactly the moment tao
                            // / WebView2 can re-touch the window's styles.
                            // Re-assert framelessness so the grey "top bar"
                            // can't surface for the duration of the
                            // interaction.
                            reassert_frameless(hwnd);
                        }
                    }
                }
            }
            tauri::WindowEvent::Focused(false) => {
                // Closes an async-ordering race: the very first WM_NCACTIVATE
                // on alt-tab back is delivered to the WndProc before Tauri
                // dispatches Focused(true) on the Rust side. Reasserting on
                // the prior blur guarantees clean style bits are in place by
                // the time that activation message arrives, so even if the
                // WM_NCACTIVATE intercept were ever defeated the frame still
                // can't paint.
                if let Ok(h) = win_clone.hwnd() {
                    let hwnd = h.0 as usize as winapi::shared::windef::HWND;
                    if !hwnd.is_null() {
                        unsafe { reassert_frameless(hwnd); }
                    }
                }
            }
            // Drop the stored original-WndProc entry on close so a future
            // widget window that lands on this same (recycled) HWND re-installs
            // the frameless subclass instead of being skipped as "already set".
            tauri::WindowEvent::Destroyed => {
                if hwnd_usize != 0 {
                    wndproc_map().lock().unwrap_or_else(|e| e.into_inner()).remove(&hwnd_usize);
                    // Drop the show-once latch too, since Windows recycles HWND
                    // values, so a future widget window on this same handle
                    // must be allowed to show again.
                    shown_windows().lock().unwrap_or_else(|e| e.into_inner()).remove(&hwnd_usize);
                    // [HK-01] Keep the hook's lock-free view in step with the map.
                    publish_overlay_snapshot();
                }
                // Drop this window's text-input-focus claim (issue #35): a
                // window destroyed mid-edit (monitor unplug) must not leave
                // the keyboard hook permanently stood down.
                {
                    use std::sync::atomic::Ordering;
                    let mut set =
                        text_input_labels().lock().unwrap_or_else(|e| e.into_inner());
                    set.remove(win_clone.label());
                    TEXT_INPUT_FOCUSED.store(!set.is_empty(), Ordering::Relaxed);
                }
            }
            _ => {}
        });
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Monitor identity + physical work-area coords (excludes taskbar)
// ---------------------------------------------------------------------------
//
// [MI-01] `\\.\DISPLAYn` IS NOT AN IDENTITY. It is an OS-assigned ordinal that
// Windows re-derives on every boot from whatever order the panels happen to
// come up in (DisplayPort link-training order, a monitor still asleep at
// logon, GPU driver init order). The same string designates a DIFFERENT
// physical panel from one boot to the next. Keying persisted per-monitor state
// on it is issue #70: a user pinned widgets to display 3 and every reboot
// moved them back onto display 2, because the saved bytes survived perfectly
// while their meaning did not.
//
// `MonitorInfo::id` is the monitor's device *interface* path
// (`\\?\DISPLAY#DEL4098#5&1a2b3c&0&UID4353#{e6f07b5f-...}`), which carries the
// panel's hardware ids and survives reboots. It is the ONLY key allowed for
// persistence (`disabled_monitors.json`, the frontend's per-monitor maps).
//
// `device_name` stays, but only for the two uses that are correct precisely
// because they are re-resolved from the live topology every time:
//   * the human label ("Display 3") the settings UI shows;
//   * the `EnumDisplayDevicesW` / `tauri::Monitor::name()` lookups below.
// It must NEVER again be written to disk as a key.

#[derive(Clone, Debug)]
struct MonitorInfo {
    /// [MI-01] Stable hardware identity. The persistence key.
    id: String,
    /// [MI-08] Did `id` come from the panel's device-interface path (true), or
    /// is it the [MI-02] ordinal fallback standing in for an identity we could
    /// not resolve (false)? A panel mid-DisplayPort-link-training answers the
    /// second way for a pass or two, and binding state to that answer is what
    /// re-homes a screen's widgets (#123).
    id_stable: bool,
    /// GDI ordinal (`\\.\DISPLAY1`). Ephemeral: label + lookup only.
    device_name: String,
    /// `MONITORINFOF_PRIMARY`: the OS's primary display.
    primary: bool,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

/// Trim a fixed-size UTF-16 Win32 string buffer at the first NUL. Win32 hands
/// back whole arrays (`DISPLAY_DEVICEW.DeviceID` is 128 WCHARs); the units past
/// the terminator are undefined padding and must never reach a persisted key.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn utf16_buf_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// [MI-02] Pick a monitor's persistence id, returning `(id, used_fallback)`.
///
/// `EnumDisplayDevicesW` can succeed yet hand back an empty `DeviceID` (some
/// indirect-display / virtual / RDP adapters). Falling back to the ordinal
/// keeps us running with a usable-if-unstable key instead of an empty string,
/// which would collide across every such monitor and merge their settings.
/// The caller logs the degradation; it is never silent.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn choose_monitor_id(device_id: &str, device_name: &str) -> (String, bool) {
    if device_id.trim().is_empty() {
        (device_name.to_string(), true)
    } else {
        (device_id.to_string(), false)
    }
}

/// Serialized monitor identity + work-area geometry ([MI-01]). The single
/// shape every monitor-related command returns.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct MonitorDescriptor {
    /// Stable hardware id: the key for anything the frontend persists.
    id: String,
    /// [MI-08] False when `id` is the [MI-02] ordinal fallback, i.e. a
    /// placeholder rather than a resolved identity. The frontend refuses to
    /// intern new per-monitor state under one.
    id_stable: bool,
    /// GDI ordinal. Display/label use only, NEVER a storage key ([MI-01]).
    device_name: String,
    primary: bool,
    /// Work-area rect, physical pixels, taskbar excluded.
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl From<MonitorInfo> for MonitorDescriptor {
    fn from(m: MonitorInfo) -> Self {
        MonitorDescriptor {
            id: m.id,
            id_stable: m.id_stable,
            device_name: m.device_name,
            primary: m.primary,
            x: m.x,
            y: m.y,
            width: m.width,
            height: m.height,
        }
    }
}

// ---------------------------------------------------------------------------
// Window label → stable monitor id mapping
// ---------------------------------------------------------------------------
//
// The `monitor-{i}` label is an enumeration-order slot and is just as unstable
// as the ordinal; this map is what turns a window label into the [MI-01]
// identity the frontend keys its per-monitor state on.

#[cfg(target_os = "windows")]
static WINDOW_MONITOR_MAP: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, String>>> =
    std::sync::OnceLock::new();

#[cfg(target_os = "windows")]
fn window_monitor_map() -> &'static std::sync::Mutex<std::collections::HashMap<String, String>> {
    WINDOW_MONITOR_MAP.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// [MI-01] The STABLE id of the monitor a widget window lives on. The
/// frontend keys every per-monitor blob (layouts, views, the disabled flag) on
/// this, so it must never be the `\\.\DISPLAYn` ordinal.
#[tauri::command]
fn get_monitor_id(label: String) -> Option<String> {
    #[cfg(target_os = "windows")]
    { window_monitor_map().lock().unwrap_or_else(|e| e.into_inner()).get(&label).cloned() }
    #[cfg(not(target_os = "windows"))]
    { let _ = label; None }
}

/// Every monitor currently connected to the OS, with its [MI-01] stable id,
/// its ordinal (label use only), whether it is the primary, and its work-area
/// rect (physical pixels, taskbar excluded).
#[tauri::command]
fn get_active_monitors() -> Vec<MonitorDescriptor> {
    enumerate_monitors().into_iter().map(MonitorDescriptor::from).collect()
}

/// One monitor as the Win32 enumeration callback sees it. The `id` lookup
/// needs a second Win32 call, which is deliberately kept OUT of the FFI
/// callback: an `extern "system"` callback must never unwind into Windows.
#[cfg(target_os = "windows")]
struct RawMonitor {
    device_name: String,
    primary: bool,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

/// [MI-02] Resolve a GDI ordinal to the panel's device-interface path.
///
/// `EnumDisplayDevicesW(adapter, 0, .., EDD_GET_DEVICE_INTERFACE_NAME)` yields
/// the monitor on that adapter with `DeviceID` set to the interface path
/// instead of the (non-unique) monitor-driver id. Never panics; every failure
/// degrades to the ordinal with a warning ([MI-02] fallback).
#[cfg(target_os = "windows")]
fn stable_monitor_id(device_name: &str) -> (String, bool) {
    use winapi::um::wingdi::DISPLAY_DEVICEW;
    use winapi::um::winuser::EnumDisplayDevicesW;

    // Not exposed by the winapi crate; from wingdi.h.
    const EDD_GET_DEVICE_INTERFACE_NAME: u32 = 0x0000_0001;

    let mut adapter: Vec<u16> = device_name.encode_utf16().collect();
    adapter.push(0);

    let mut dd: DISPLAY_DEVICEW = unsafe { std::mem::zeroed() };
    // MANDATORY: EnumDisplayDevicesW rejects the call outright without it.
    dd.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;

    let ok = unsafe {
        EnumDisplayDevicesW(adapter.as_ptr(), 0, &mut dd, EDD_GET_DEVICE_INTERFACE_NAME)
    };
    if ok == 0 {
        log::warn!(
            target: "displays",
            "EnumDisplayDevicesW failed for {device_name}; falling back to the \
             ordinal as this monitor's id. Per-monitor settings for it may not \
             survive a reboot (#70)."
        );
        return (device_name.to_string(), false);
    }

    let device_id = utf16_buf_to_string(&dd.DeviceID);
    let (id, used_fallback) = choose_monitor_id(&device_id, device_name);
    if used_fallback {
        log::warn!(
            target: "displays",
            "EnumDisplayDevicesW returned an empty DeviceID for {device_name}; \
             falling back to the ordinal as this monitor's id. Per-monitor \
             settings for it may not survive a reboot (#70)."
        );
    }
    // [MI-08] The second half is what matters downstream: `false` means "this
    // string NAMES nothing" - an ordinal standing in for an identity we could
    // not resolve. The frontend must not intern per-monitor state under it
    // (src/views.ts, `monitorIdentityIsResolved`).
    (id, !used_fallback)
}

#[cfg(target_os = "windows")]
fn enumerate_monitors() -> Vec<MonitorInfo> {
    use winapi::shared::minwindef::{BOOL, LPARAM};
    use winapi::shared::windef::{HDC, HMONITOR, LPRECT};
    use winapi::um::winuser::{
        EnumDisplayMonitors, GetMonitorInfoW, MONITORINFO, MONITORINFOEXW, MONITORINFOF_PRIMARY,
    };

    unsafe extern "system" fn enum_proc(
        hmonitor: HMONITOR,
        _hdc: HDC,
        _rect: LPRECT,
        data: LPARAM,
    ) -> BOOL {
        let monitors = &mut *(data as *mut Vec<RawMonitor>);

        let mut info: MONITORINFOEXW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;

        if GetMonitorInfoW(hmonitor, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO) != 0 {
            // rcWork = work area (full monitor minus taskbar), in physical pixels
            let r = info.rcWork;
            monitors.push(RawMonitor {
                device_name: utf16_buf_to_string(&info.szDevice),
                // Already on hand from GetMonitorInfoW; the pre-#70 code threw
                // it away and left the frontend guessing which panel is primary.
                primary: info.dwFlags & MONITORINFOF_PRIMARY != 0,
                x: r.left,
                y: r.top,
                width: r.right - r.left,
                height: r.bottom - r.top,
            });
        }

        1 // TRUE, continue enumeration
    }

    let mut raw: Vec<RawMonitor> = Vec::new();
    unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(enum_proc),
            &mut raw as *mut _ as LPARAM,
        );
    }

    // Resolve identities outside the FFI callback (see `RawMonitor`).
    raw.into_iter()
        .map(|r| {
            let (id, id_stable) = stable_monitor_id(&r.device_name);
            MonitorInfo {
                id,
                id_stable,
                device_name: r.device_name,
                primary: r.primary,
                x: r.x,
                y: r.y,
                width: r.width,
                height: r.height,
            }
        })
        .collect()
}

#[cfg(not(target_os = "windows"))]
fn enumerate_monitors() -> Vec<MonitorInfo> {
    vec![]
}

// ---------------------------------------------------------------------------
// App Layouts: launch one or more apps and snap each to a region of a chosen
// monitor's work area. Used by the App Layouts widget.
// ---------------------------------------------------------------------------

fn default_true() -> bool { true }

#[derive(serde::Deserialize, Clone, Debug)]
struct LayoutEntry {
    /// "lnk" | "url" | "path" | "explorer"
    kind: String,
    target: String,
    #[serde(default)]
    args: Option<String>,
    #[serde(rename = "xPct")] x_pct: f32,
    #[serde(rename = "yPct")] y_pct: f32,
    #[serde(rename = "wPct")] w_pct: f32,
    #[serde(rename = "hPct")] h_pct: f32,
    /// When true (the default), an already-running instance of this exe is
    /// just repositioned, no new process is spawned. When false, we
    /// reposition the existing window AND additionally try to spawn a fresh
    /// instance (which is then positioned too if it produces a new window).
    #[serde(rename = "reuseIfRunning", default = "default_true")]
    reuse_if_running: bool,
    /// Exe basename used for matching already-running windows (case-insensitive).
    #[serde(rename = "matchHint", default)]
    match_hint: Option<String>,
}

/// Launch a layout. The target monitor is the one the *calling window* is on:
/// the display the user is interacting with, so layouts are always
/// applied to the screen the click happened on, no per-layout monitor picker.
#[tauri::command]
async fn launch_app_layout(window: tauri::Window, entries: Vec<LayoutEntry>) -> Result<(), String> {
    let monitors = enumerate_monitors();
    if monitors.is_empty() {
        return Err("no monitors detected".into());
    }

    // Resolve the caller's monitor into one of our work-area entries. Matching
    // on the ordinal is correct HERE and only here: `tauri::Monitor::name()`
    // and `enumerate_monitors` are both read from the live topology in this
    // same call, so the ordinal cannot have drifted between them. Nothing is
    // persisted ([MI-01]). Falls back to the primary monitor if the lookup
    // fails, then to the first enumerated one.
    let caller_name: Option<String> = window
        .current_monitor()
        .ok()
        .flatten()
        .and_then(|m| m.name().cloned());
    let target_mon: MonitorInfo = caller_name
        .as_ref()
        .and_then(|n| monitors.iter().find(|m| &m.device_name == n).cloned())
        .or_else(|| monitors.iter().find(|m| m.primary).cloned())
        .unwrap_or_else(|| monitors[0].clone());

    // Dedupe entries by (kind, target, args): if the user assigned the same
    // exact app to two panes, launching it twice typically yields one window
    // (single-instance apps) or duplicates that fight over the same target,
    // neither is what they want. Keep the first occurrence's rect; warn-log
    // the dropped entries so a misconfiguration is at least visible.
    let mut seen: std::collections::HashSet<(String, String, Option<String>)> =
        std::collections::HashSet::new();
    let entries: Vec<LayoutEntry> = entries
        .into_iter()
        .filter(|e| {
            let key = (e.kind.clone(), e.target.clone(), e.args.clone());
            let fresh = seen.insert(key);
            if !fresh {
                log::warn!(
                    target: "app_layouts",
                    "duplicate entry skipped: kind={} target={}", e.kind, e.target,
                );
            }
            fresh
        })
        .collect();

    let mut tasks = Vec::new();
    for entry in entries {
        let mon = target_mon.clone();
        tasks.push(tokio::task::spawn_blocking(move || launch_one(&entry, &mon)));
    }
    for t in tasks {
        match t.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => log::warn!(target: "app_layouts", "entry failed: {e}"),
            Err(e)     => log::warn!(target: "app_layouts", "task panicked: {e}"),
        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn launch_one(entry: &LayoutEntry, mon: &MonitorInfo) -> Result<(), String> {
    let rect = compute_rect(mon, entry.x_pct, entry.y_pct, entry.w_pct, entry.h_pct);
    // A folder pane opens File Explorer; its window is matched by class, not exe
    // (explorer.exe also owns the desktop + taskbar), so it has its own path.
    if entry.kind == "explorer" {
        return launch_explorer(&entry.target, rect);
    }
    // A URL pane carries no exe hint, yet the window we need to snap is the
    // default browser's, which ShellExecute won't hand us a usable pid for (it
    // opens a tab in the running browser). Fall back to the default browser exe
    // so the window-find/position machinery below can locate and size it.
    let browser_hint: Option<String> = if entry.match_hint.is_none() && entry.kind == "url" {
        default_browser_exe()
    } else {
        None
    };
    let hint = entry.match_hint.as_deref().or(browser_hint.as_deref());

    // First: always reposition an existing main window for this exe if one
    // exists. Most apps in real layouts are single-instance (Word, Code,
    // Outlook, OneNote, Slack, Teams, etc.) launching them again just brings
    // the existing window forward without producing a new one we could
    // detect, so without this step "click layout while app already open"
    // does nothing visible.
    let did_reuse = if let Some(h) = hint.and_then(find_window_by_exe) {
        position_window(h, rect);
        if entry.reuse_if_running {
            return Ok(()); // satisfied, don't also spawn a duplicate
        }
        true
    } else {
        false
    };

    // Snapshot the pre-launch window set so wait_for_window can distinguish
    // "newly-opened by us" from "already on screen when we started".
    let before: std::collections::HashSet<usize> = match hint {
        Some(h) => snapshot_windows_for_exe(h),
        None => std::collections::HashSet::new(),
    };

    let pid = spawn_entry(entry)?;

    // Up to ~7s to find a real main window. Office (Word/PowerPoint) shows a
    // splash for 1–2s before the document window appears, so accept any
    // qualifying main-window candidate as soon as one shows up.
    let initial_hwnd = match wait_for_window(pid, &before, hint, 7000) {
        Some(h) => h,
        None => {
            // No new window appeared. If we already moved an existing one,
            // count the layout slot as satisfied: the user sees their app
            // in the right pane, which is what they wanted.
            if did_reuse { return Ok(()); }
            return Err("window did not appear within 7s".into());
        }
    };

    // Position now and keep watching for ~3s: if the app swaps a splash for
    // the real window, restores multiple workspace windows, or self-positions
    // right after our first SetWindowPos, the watch loop re-targets / re-applies.
    position_and_watch(initial_hwnd, rect, pid, hint, &before, 3000);
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn launch_one(_entry: &LayoutEntry, _mon: &MonitorInfo) -> Result<(), String> {
    Err("app layouts are only supported on Windows".into())
}

fn compute_rect(mon: &MonitorInfo, xp: f32, yp: f32, wp: f32, hp: f32) -> (i32, i32, i32, i32) {
    let clamp = |v: f32| v.clamp(0.0, 100.0);
    let xp = clamp(xp);
    let yp = clamp(yp);
    let wp = clamp(wp);
    let hp = clamp(hp);
    let x = mon.x + (mon.width as f32 * xp / 100.0).round() as i32;
    let y = mon.y + (mon.height as f32 * yp / 100.0).round() as i32;
    let w = ((mon.width as f32 * wp / 100.0).round() as i32).max(100);
    let h = ((mon.height as f32 * hp / 100.0).round() as i32).max(80);
    (x, y, w, h)
}

/// Heuristic for "this is the app's real main window, not a splash or transient
/// dialog." Office in particular shows a small splash with empty title before
/// the actual document window appears.
#[cfg(target_os = "windows")]
fn is_main_window_candidate(hwnd: winapi::shared::windef::HWND) -> bool {
    use winapi::shared::windef::RECT;
    use winapi::um::winuser::{
        IsWindowVisible, GetWindow, GW_OWNER,
        GetWindowLongW, GWL_EXSTYLE, WS_EX_TOOLWINDOW,
        GetWindowTextLengthW, GetWindowRect,
    };
    unsafe {
        if IsWindowVisible(hwnd) == 0 { return false; }
        if !GetWindow(hwnd, GW_OWNER).is_null() { return false; }
        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        if (ex & WS_EX_TOOLWINDOW) != 0 { return false; }
        // Splash screens typically have no title; the real document window does.
        if GetWindowTextLengthW(hwnd) == 0 { return false; }
        let mut r: RECT = std::mem::zeroed();
        if GetWindowRect(hwnd, &mut r) == 0 { return false; }
        let w = r.right - r.left;
        let h = r.bottom - r.top;
        w >= 400 && h >= 300
    }
}

/// Best top-level window matching the launch context, preferring real main
/// windows over splash/transient windows. `prefer_main` controls whether to
/// return `None` when only non-main candidates are available (use during the
/// early waiting phase) or fall back to any visible top-level (use near the
/// timeout to avoid giving up entirely).
#[cfg(target_os = "windows")]
fn find_best_window(
    pid: u32,
    exe_hint: Option<&str>,
    before: &std::collections::HashSet<usize>,
    prefer_main: bool,
) -> Option<winapi::shared::windef::HWND> {
    use winapi::shared::windef::HWND;
    let hint_lower = exe_hint.map(|s| s.to_lowercase());

    let mut candidates: Vec<HWND> = Vec::new();
    for h in enum_top_level_visible() {
        let owned_by_pid = pid != 0 && hwnd_pid(h) == pid;
        let owned_by_exe = match hint_lower.as_deref() {
            Some(hint) => {
                if before.contains(&(h as usize)) { false }
                else {
                    let pid = hwnd_pid(h);
                    if pid == 0 { false }
                    else { pid_exe_basename(pid).map(|b| b.to_lowercase() == hint).unwrap_or(false) }
                }
            }
            None => false,
        };
        if owned_by_pid || owned_by_exe { candidates.push(h); }
    }
    if candidates.is_empty() { return None; }
    for &h in &candidates {
        if is_main_window_candidate(h) { return Some(h); }
    }
    if prefer_main { None } else { Some(candidates[0]) }
}

/// Invisible-border widths (left, top, right, bottom) for a window: the
/// difference between `GetWindowRect` (which includes the resize handles +
/// drop shadow hit area) and `DWMWA_EXTENDED_FRAME_BOUNDS` (the visible
/// painted frame). On Win 10/11 this is usually ~7–8 px on each side except
/// the title bar. Returns `None` if DWM composition isn't available.
#[cfg(target_os = "windows")]
fn measure_invisible_borders(hwnd: winapi::shared::windef::HWND) -> Option<(i32, i32, i32, i32)> {
    use winapi::shared::windef::RECT;
    use winapi::um::dwmapi::DwmGetWindowAttribute;
    use winapi::um::winuser::GetWindowRect;

    // DWMWA_EXTENDED_FRAME_BOUNDS = 9 (defined in dwmapi.h).
    const DWMWA_EXTENDED_FRAME_BOUNDS: u32 = 9;

    unsafe {
        let mut win: RECT = std::mem::zeroed();
        if GetWindowRect(hwnd, &mut win) == 0 { return None; }
        let mut frame: RECT = std::mem::zeroed();
        let hr = DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut frame as *mut _ as *mut _,
            std::mem::size_of::<RECT>() as u32,
        );
        if hr != 0 { return None; }
        Some((
            frame.left - win.left,
            frame.top - win.top,
            win.right - frame.right,
            win.bottom - frame.bottom,
        ))
    }
}

/// Visible rect (DWM extended frame bounds) as `(x, y, w, h)`: the painted
/// edges of the window, excluding the invisible resize border.
#[cfg(target_os = "windows")]
fn get_visible_rect(hwnd: winapi::shared::windef::HWND) -> Option<(i32, i32, i32, i32)> {
    use winapi::shared::windef::RECT;
    use winapi::um::dwmapi::DwmGetWindowAttribute;

    const DWMWA_EXTENDED_FRAME_BOUNDS: u32 = 9;

    unsafe {
        let mut frame: RECT = std::mem::zeroed();
        let hr = DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut frame as *mut _ as *mut _,
            std::mem::size_of::<RECT>() as u32,
        );
        if hr != 0 { return None; }
        Some((frame.left, frame.top, frame.right - frame.left, frame.bottom - frame.top))
    }
}

/// Place a window so its *visible* edges land on `rect`, compensating for the
/// invisible resize-border outside the painted frame. Without this step,
/// adjacent panes at e.g. 0–50% and 50–100% paint with a visible gap because
/// each window's bounding rect is ~7 px wider than what's painted.
#[cfg(target_os = "windows")]
fn apply_window_pos(hwnd: winapi::shared::windef::HWND, rect: (i32, i32, i32, i32)) {
    use winapi::um::winuser::{
        SetWindowPos, ShowWindow, IsZoomed, IsIconic,
        SW_RESTORE, SWP_NOZORDER, SWP_NOACTIVATE,
    };
    unsafe {
        if IsZoomed(hwnd) != 0 || IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        // First pass: nominal rect. The window has to actually be in place
        // before DWM will report meaningful extended-frame-bounds.
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            rect.0, rect.1, rect.2, rect.3,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );

        // Second pass: expand the rect by the invisible border so visible
        // edges align. Skip if the window has no invisible border (rare,
        // old-style un-DWM-composited windows) or DWM lookup failed.
        if let Some((dl, dt, dr, db)) = measure_invisible_borders(hwnd) {
            if dl != 0 || dt != 0 || dr != 0 || db != 0 {
                SetWindowPos(
                    hwnd,
                    std::ptr::null_mut(),
                    rect.0 - dl, rect.1 - dt,
                    rect.2 + dl + dr, rect.3 + dt + db,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
        }
    }
}

/// Place the window at `rect` and re-apply over the next ~1s to defeat:
///   • apps that restore a saved window position right after we set ours,
///   • windows that initially land on the wrong monitor due to per-monitor DPI
///     virtualization (the first SetWindowPos sometimes ends up on the source
///     monitor; a second call from a settled state moves it correctly).
#[cfg(target_os = "windows")]
fn position_window(hwnd: winapi::shared::windef::HWND, rect: (i32, i32, i32, i32)) {
    use winapi::shared::windef::{HMONITOR, RECT};
    use winapi::um::winuser::{GetWindowRect, MonitorFromRect, MonitorFromWindow, MONITOR_DEFAULTTONEAREST};
    use std::time::Duration;

    let target_rect = RECT {
        left: rect.0,
        top: rect.1,
        right: rect.0 + rect.2,
        bottom: rect.1 + rect.3,
    };
    let target_mon: HMONITOR = unsafe { MonitorFromRect(&target_rect, MONITOR_DEFAULTTONEAREST) };

    apply_window_pos(hwnd, rect);

    for delay_ms in [180u64, 600] {
        std::thread::sleep(Duration::from_millis(delay_ms));
        let drifted = {
            let cur_mon = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
            match get_visible_rect(hwnd) {
                Some((cx, cy, cw, ch)) => {
                    cur_mon != target_mon
                        || (cx - rect.0).abs() > 8
                        || (cy - rect.1).abs() > 8
                        || (cw - rect.2).abs() > 24
                        || (ch - rect.3).abs() > 24
                }
                None => {
                    let mut cur: RECT = unsafe { std::mem::zeroed() };
                    if unsafe { GetWindowRect(hwnd, &mut cur) } == 0 { return; }
                    let bw = cur.right - cur.left;
                    let bh = cur.bottom - cur.top;
                    cur_mon != target_mon
                        || (cur.left - rect.0).abs() > 8
                        || (cur.top - rect.1).abs() > 8
                        || (bw - rect.2).abs() > 24
                        || (bh - rect.3).abs() > 24
                }
            }
        };
        if !drifted { return; }
        apply_window_pos(hwnd, rect);
    }
}

#[cfg(target_os = "windows")]
fn spawn_entry(entry: &LayoutEntry) -> Result<u32, String> {
    use winapi::um::winuser::AllowSetForegroundWindow;
    unsafe { AllowSetForegroundWindow(0xFFFFFFFF); } // ASFW_ANY
    match entry.kind.as_str() {
        "url" | "lnk" | "explorer" => shell_execute_ex(&entry.target, None),
        "path" => shell_execute_ex(&entry.target, entry.args.as_deref()),
        other => Err(format!("unknown entry kind '{other}'")),
    }
}

#[cfg(target_os = "windows")]
fn shell_execute_ex(path: &str, args: Option<&str>) -> Result<u32, String> {
    use std::ptr;
    use winapi::um::shellapi::{ShellExecuteExW, SHELLEXECUTEINFOW, SEE_MASK_NOCLOSEPROCESS};
    use winapi::um::processthreadsapi::GetProcessId;
    use winapi::um::handleapi::CloseHandle;

    let op: Vec<u16> = "open\0".encode_utf16().collect();
    let path_w: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let args_w: Option<Vec<u16>> = args.map(|s| s.encode_utf16().chain(std::iter::once(0)).collect());

    unsafe {
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS;
        info.lpVerb = op.as_ptr();
        info.lpFile = path_w.as_ptr();
        info.lpParameters = args_w.as_ref().map_or(ptr::null(), |v| v.as_ptr());
        info.nShow = 1; // SW_NORMAL
        if ShellExecuteExW(&mut info) == 0 {
            return Err(format!("ShellExecuteExW failed for '{path}'"));
        }
        let pid = if info.hProcess.is_null() {
            0
        } else {
            let p = GetProcessId(info.hProcess);
            CloseHandle(info.hProcess);
            p
        };
        Ok(pid)
    }
}

#[cfg(target_os = "windows")]
fn wait_for_window(
    pid: u32,
    before: &std::collections::HashSet<usize>,
    exe_hint: Option<&str>,
    timeout_ms: u64,
) -> Option<winapi::shared::windef::HWND> {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    // Spend the first 70% of the budget waiting for a real main window.
    // After that, accept any visible top-level so we don't time out entirely
    // on apps that have no title or are unusually small.
    let main_only_until = Instant::now() + Duration::from_millis(timeout_ms * 70 / 100);

    loop {
        let prefer_main = Instant::now() < main_only_until;
        if let Some(h) = find_best_window(pid, exe_hint, before, prefer_main) {
            return Some(h);
        }
        if Instant::now() >= deadline { return None; }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Apply the position to `initial_hwnd`, then for ~`watch_ms` ms keep snapping
/// every additional new main window owned by the same PID/exe to the same
/// rect, and re-apply the initial window if it drifts. This handles three
/// distinct annoyances at once:
///   1. Splash → real-window swap (Office). The splash gets skipped by the
///      main-window filter, and the real window we pick up here.
///   2. Workspace-restoring apps (VS Code, JetBrains, Discord) that bring
///      back *several* windows from the previous session. Each new window
///      gets snapped to the target pane instead of staying at its remembered
///      position.
///   3. Apps that re-position themselves after our first SetWindowPos
///      (Office, some browsers). The drift check re-applies if the rect or
///      monitor changes.
#[cfg(target_os = "windows")]
fn position_and_watch(
    initial_hwnd: winapi::shared::windef::HWND,
    rect: (i32, i32, i32, i32),
    pid: u32,
    exe_hint: Option<&str>,
    before: &std::collections::HashSet<usize>,
    watch_ms: u64,
) {
    use winapi::shared::windef::{HMONITOR, RECT};
    use winapi::um::winuser::{GetWindowRect, MonitorFromRect, MonitorFromWindow, MONITOR_DEFAULTTONEAREST};
    use std::time::{Duration, Instant};

    let target_rect = RECT {
        left: rect.0,
        top: rect.1,
        right: rect.0 + rect.2,
        bottom: rect.1 + rect.3,
    };
    let target_mon: HMONITOR = unsafe { MonitorFromRect(&target_rect, MONITOR_DEFAULTTONEAREST) };

    let mut positioned: std::collections::HashSet<usize> = std::collections::HashSet::new();
    positioned.insert(initial_hwnd as usize);
    apply_window_pos(initial_hwnd, rect);

    let deadline = Instant::now() + Duration::from_millis(watch_ms);
    let tick = Duration::from_millis(180);

    while Instant::now() < deadline {
        std::thread::sleep(tick);

        // Snap any *additional* new main windows that have appeared for the
        // same PID/exe. Workspace-restoring apps reopen several windows over
        // a couple of seconds; we want every one of them on our pane.
        for h in find_all_main_candidates(pid, exe_hint, before) {
            if !positioned.contains(&(h as usize)) {
                positioned.insert(h as usize);
                apply_window_pos(h, rect);
            }
        }

        // Has the initial window drifted off-target? Compare against the DWM
        // *visible* rect: apply_window_pos intentionally pushes the window's
        // bounding rect outside `rect` by the invisible-border width, so a
        // raw GetWindowRect comparison would always look "drifted" and loop.
        let drifted = {
            let cur_mon = unsafe { MonitorFromWindow(initial_hwnd, MONITOR_DEFAULTTONEAREST) };
            match get_visible_rect(initial_hwnd) {
                Some((cx, cy, cw, ch)) => {
                    cur_mon != target_mon
                        || (cx - rect.0).abs() > 8
                        || (cy - rect.1).abs() > 8
                        || (cw - rect.2).abs() > 24
                        || (ch - rect.3).abs() > 24
                }
                None => {
                    // Fallback: window without DWM composition. Use raw rect.
                    let mut cur: RECT = unsafe { std::mem::zeroed() };
                    if unsafe { GetWindowRect(initial_hwnd, &mut cur) } == 0 { break; }
                    let bw = cur.right - cur.left;
                    let bh = cur.bottom - cur.top;
                    cur_mon != target_mon
                        || (cur.left - rect.0).abs() > 8
                        || (cur.top - rect.1).abs() > 8
                        || (bw - rect.2).abs() > 24
                        || (bh - rect.3).abs() > 24
                }
            }
        };
        if drifted {
            apply_window_pos(initial_hwnd, rect);
        }
    }
}

/// Every visible top-level *main* window owned by the given PID OR by a new
/// instance of the given exe (i.e. not in the pre-launch snapshot).
#[cfg(target_os = "windows")]
fn find_all_main_candidates(
    pid: u32,
    exe_hint: Option<&str>,
    before: &std::collections::HashSet<usize>,
) -> Vec<winapi::shared::windef::HWND> {
    let hint_lower = exe_hint.map(|s| s.to_lowercase());
    let mut out = Vec::new();
    for h in enum_top_level_visible() {
        if !is_main_window_candidate(h) { continue; }
        let owned_by_pid = pid != 0 && hwnd_pid(h) == pid;
        let owned_by_exe = match hint_lower.as_deref() {
            Some(hint) => {
                if before.contains(&(h as usize)) { false }
                else {
                    let p = hwnd_pid(h);
                    if p == 0 { false }
                    else { pid_exe_basename(p).map(|b| b.to_lowercase() == hint).unwrap_or(false) }
                }
            }
            None => false,
        };
        if owned_by_pid || owned_by_exe { out.push(h); }
    }
    out
}

#[cfg(target_os = "windows")]
fn enum_top_level_visible() -> Vec<winapi::shared::windef::HWND> {
    use winapi::shared::windef::HWND;
    use winapi::shared::minwindef::{BOOL, LPARAM};
    use winapi::um::winuser::{
        EnumWindows, IsWindowVisible, GetWindow, GW_OWNER,
        GetWindowLongW, GWL_EXSTYLE, WS_EX_TOOLWINDOW,
    };
    unsafe extern "system" fn cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let list = &mut *(lparam as *mut Vec<HWND>);
        if IsWindowVisible(hwnd) == 0 { return 1; }
        if !GetWindow(hwnd, GW_OWNER).is_null() { return 1; }
        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        if (ex & WS_EX_TOOLWINDOW) != 0 { return 1; }
        list.push(hwnd);
        1
    }
    let mut list: Vec<HWND> = Vec::new();
    unsafe { EnumWindows(Some(cb), &mut list as *mut _ as LPARAM); }
    list
}

#[cfg(target_os = "windows")]
fn hwnd_pid(hwnd: winapi::shared::windef::HWND) -> u32 {
    use winapi::um::winuser::GetWindowThreadProcessId;
    let mut pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid); }
    pid
}

#[cfg(target_os = "windows")]
fn pid_exe_basename(pid: u32) -> Option<String> {
    use winapi::um::processthreadsapi::OpenProcess;
    use winapi::um::winbase::QueryFullProcessImageNameW;
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::winnt::PROCESS_QUERY_LIMITED_INFORMATION;

    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() { return None; }
        let mut buf = [0u16; 520];
        let mut sz: u32 = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut sz);
        CloseHandle(h);
        if ok == 0 { return None; }
        let path = String::from_utf16_lossy(&buf[..sz as usize]);
        std::path::Path::new(&path)
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
    }
}

/// Used by the reuse-if-running path: find an existing main window for the
/// given exe (anywhere on screen, we want to re-target it, not skip it).
#[cfg(target_os = "windows")]
fn find_window_by_exe(exe_basename: &str) -> Option<winapi::shared::windef::HWND> {
    let hint = exe_basename.to_lowercase();
    let mut fallback: Option<winapi::shared::windef::HWND> = None;
    for h in enum_top_level_visible() {
        let pid = hwnd_pid(h);
        if pid == 0 { continue; }
        let matches = pid_exe_basename(pid).map(|b| b.to_lowercase() == hint).unwrap_or(false);
        if !matches { continue; }
        if is_main_window_candidate(h) { return Some(h); }
        fallback.get_or_insert(h);
    }
    fallback
}

/// Snapshot of HWNDs that already belong to this exe before we launch. Used as
/// a "from this snapshot forward, only consider new windows" baseline so we
/// don't re-target a pre-existing window when re-use is off.
#[cfg(target_os = "windows")]
fn snapshot_windows_for_exe(exe_basename: &str) -> std::collections::HashSet<usize> {
    let hint = exe_basename.to_lowercase();
    let mut set = std::collections::HashSet::new();
    for h in enum_top_level_visible() {
        let pid = hwnd_pid(h);
        if pid == 0 { continue; }
        if let Some(base) = pid_exe_basename(pid) {
            if base.to_lowercase() == hint { set.insert(h as usize); }
        }
    }
    set
}

/// Window class name (e.g. `CabinetWClass` for a File Explorer folder window).
#[cfg(target_os = "windows")]
fn window_class(hwnd: winapi::shared::windef::HWND) -> String {
    use winapi::um::winuser::GetClassNameW;
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    if n <= 0 { return String::new(); }
    String::from_utf16_lossy(&buf[..n as usize])
}

/// Snapshot of visible top-level HWNDs of a given window class. Used as a
/// "from here forward, only consider new windows" baseline for class-matched
/// launches (File Explorer), where matching by exe is unsafe: `explorer.exe`
/// also owns the desktop (`Progman`) and taskbar, which must never be moved.
#[cfg(target_os = "windows")]
fn snapshot_windows_by_class(class: &str) -> std::collections::HashSet<usize> {
    let mut set = std::collections::HashSet::new();
    for h in enum_top_level_visible() {
        if window_class(h) == class { set.insert(h as usize); }
    }
    set
}

/// Find a window of `class` not present in `before`. `prefer_main` returns
/// `None` (rather than a non-main fallback) while we still have time to wait for
/// a real main window.
#[cfg(target_os = "windows")]
fn find_new_class_window(
    class: &str,
    before: &std::collections::HashSet<usize>,
    prefer_main: bool,
) -> Option<winapi::shared::windef::HWND> {
    let mut fallback: Option<winapi::shared::windef::HWND> = None;
    for h in enum_top_level_visible() {
        if before.contains(&(h as usize)) { continue; }
        if window_class(h) != class { continue; }
        if is_main_window_candidate(h) { return Some(h); }
        fallback.get_or_insert(h);
    }
    if prefer_main { None } else { fallback }
}

/// Poll up to `timeout_ms` for a new window of `class` (mirrors
/// `wait_for_window`, but matched by class instead of pid/exe).
#[cfg(target_os = "windows")]
fn wait_for_new_class_window(
    class: &str,
    before: &std::collections::HashSet<usize>,
    timeout_ms: u64,
) -> Option<winapi::shared::windef::HWND> {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let main_only_until = Instant::now() + Duration::from_millis(timeout_ms * 70 / 100);
    loop {
        let prefer_main = Instant::now() < main_only_until;
        if let Some(h) = find_new_class_window(class, before, prefer_main) {
            return Some(h);
        }
        if Instant::now() >= deadline { return None; }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Open a folder in File Explorer and snap that window onto the pane. Explorer
/// is matched by window class (`CabinetWClass`), never by exe, since
/// `explorer.exe` also owns the desktop + taskbar. Opening a folder usually
/// spawns a new Explorer window; if Explorer instead reused an existing one
/// (no new window appeared) we fall back to the foreground Explorer window.
#[cfg(target_os = "windows")]
fn launch_explorer(folder: &str, rect: (i32, i32, i32, i32)) -> Result<(), String> {
    use winapi::um::winuser::{AllowSetForegroundWindow, GetForegroundWindow};
    const CLASS: &str = "CabinetWClass";

    let before = snapshot_windows_by_class(CLASS);
    unsafe { AllowSetForegroundWindow(0xFFFFFFFF); } // ASFW_ANY
    let _ = shell_execute_ex(folder, None)?;

    let hwnd = wait_for_new_class_window(CLASS, &before, 7000).or_else(|| {
        let fg = unsafe { GetForegroundWindow() };
        if !fg.is_null() && window_class(fg) == CLASS && is_main_window_candidate(fg) {
            Some(fg)
        } else {
            None
        }
    });

    match hwnd {
        Some(h) => {
            // pid=0 / hint=None: the watch loop only re-asserts THIS window's
            // position (no pid/exe re-targeting), so it can't grab the desktop.
            position_and_watch(h, rect, 0, None, &std::collections::HashSet::new(), 3000);
            Ok(())
        }
        None => Err("File Explorer window did not appear within 7s".into()),
    }
}

/// Read a REG_SZ value. `value_name = None` reads the key's default value.
/// Returns the string with its trailing NUL stripped, or `None` on any failure.
#[cfg(target_os = "windows")]
fn read_reg_sz(
    root: winapi::shared::minwindef::HKEY,
    subkey: &str,
    value_name: Option<&str>,
) -> Option<String> {
    use winapi::um::winreg::{RegOpenKeyExW, RegQueryValueExW, RegCloseKey};
    use winapi::um::winnt::KEY_READ;
    use std::ptr;
    unsafe {
        let mut hkey = ptr::null_mut();
        let sub: Vec<u16> = subkey.encode_utf16().chain(std::iter::once(0)).collect();
        if RegOpenKeyExW(root, sub.as_ptr(), 0, KEY_READ, &mut hkey) != 0 {
            return None;
        }
        let name_w: Option<Vec<u16>> =
            value_name.map(|v| v.encode_utf16().chain(std::iter::once(0)).collect());
        let name_ptr = name_w.as_ref().map_or(ptr::null(), |v| v.as_ptr());

        // First call with a null buffer reports the required byte size.
        let mut size: u32 = 0;
        if RegQueryValueExW(hkey, name_ptr, ptr::null_mut(), ptr::null_mut(), ptr::null_mut(), &mut size) != 0
            || size == 0
        {
            RegCloseKey(hkey);
            return None;
        }
        let mut buf: Vec<u16> = vec![0u16; (size as usize / 2) + 1];
        let mut size2: u32 = (buf.len() * 2) as u32;
        let rc = RegQueryValueExW(
            hkey,
            name_ptr,
            ptr::null_mut(),
            ptr::null_mut(),
            buf.as_mut_ptr() as *mut u8,
            &mut size2,
        );
        RegCloseKey(hkey);
        if rc != 0 {
            return None;
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }
}

/// Resolve the default `https` handler's executable basename (e.g. `chrome.exe`,
/// `msedge.exe`, `firefox.exe`) from the per-user UrlAssociations UserChoice and
/// the ProgId's `shell\open\command`. Used to locate the browser window when a
/// layout pane launches a URL: ShellExecute on a URL returns a transient
/// launcher pid, and an already-running browser opens the URL as a tab in an
/// existing window, so without the exe basename there is no window to snap onto
/// the pane (the browser just keeps its previous size).
#[cfg(target_os = "windows")]
fn default_browser_exe() -> Option<String> {
    use winapi::um::winreg::{HKEY_CURRENT_USER, HKEY_CLASSES_ROOT};

    let progid = read_reg_sz(
        HKEY_CURRENT_USER,
        "Software\\Microsoft\\Windows\\Shell\\Associations\\UrlAssociations\\https\\UserChoice",
        Some("ProgId"),
    )?;
    let cmd = read_reg_sz(
        HKEY_CLASSES_ROOT,
        &format!("{progid}\\shell\\open\\command"),
        None,
    )?;
    // `cmd` is e.g. `"C:\...\chrome.exe" --single-argument %1` (quoted) or
    // `C:\...\firefox.exe -osint -url %1` (unquoted). Take the exe token.
    let exe_path = if let Some(rest) = cmd.strip_prefix('"') {
        rest.split('"').next().unwrap_or("").to_string()
    } else {
        cmd.split_whitespace().next().unwrap_or("").to_string()
    };
    let base = exe_path.rsplit(['\\', '/']).next().unwrap_or("");
    if base.is_empty() { None } else { Some(base.to_string()) }
}

// ---------------------------------------------------------------------------
// Windows: z-order + taskbar/Alt-Tab exclusion
// ---------------------------------------------------------------------------

/// Re-assert the frameless invariant on an overlay window: strip every
/// non-client frame style and make sure our `no_minimize_wnd_proc` subclass
/// (which forces a zero non-client area via WM_NCCALCSIZE) is the live proc,
/// then route a frame recalc through it. Idempotent and cheap.
///
/// `pin_to_desktop` does this once at window creation, but tao / WebView2
/// finish webview init asynchronously and can reset the window's styles or
/// proc afterwards. While the window is region-clipped to the widget shapes
/// the residual frame is clipped away invisibly, but the moment the region
/// is expanded to the full window (a context-menu backdrop pushed via
/// `update_widget_rects`, or `set_interactive(true)` clearing the region for
/// edit mode / a modal) that frame flashes back as a grey "top bar". Calling
/// this from `apply_widget_region` re-asserts the invariant exactly when the
/// visible shape changes, so the frame can never reappear.
///
/// Deliberately does NOT touch the extended styles: `set_interactive`
/// toggles WS_EX_NOACTIVATE and this must not fight that.
#[cfg(target_os = "windows")]
unsafe fn reassert_frameless(hwnd: winapi::shared::windef::HWND) {
    use winapi::um::winuser::{
        GetWindowLongW, SetWindowLongW, SetWindowPos,
        GetWindowLongPtrW, SetWindowLongPtrW,
        GWL_STYLE, GWLP_WNDPROC,
        SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
        WS_BORDER, WS_CAPTION, WS_DLGFRAME, WS_SYSMENU, WS_THICKFRAME,
    };

    // Strip every non-client frame style. `decorations(false)` removes most
    // of it, but a residual caption / sizing border can survive, and once
    // the window is no longer region-clipped to the widget shapes that frame
    // becomes visible.
    let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
    SetWindowLongW(
        hwnd,
        GWL_STYLE,
        (style & !(WS_CAPTION | WS_THICKFRAME | WS_BORDER | WS_DLGFRAME | WS_SYSMENU)) as i32,
    );

    // Ensure our no-minimize WndProc subclass is the live proc. "Already
    // installed?" is decided from the window's *live* WndProc, never from a
    // map lookup: a closed widget window leaves a stale `wndproc_map` entry
    // behind and Windows freely recycles HWND values, so a map-only check
    // would treat a brand-new window on a recycled HWND as already-subclassed
    // and let the default "Tauri App" frame survive. Comparing the live proc
    // is self-correcting and also prevents the infinite-recursion crash from
    // re-capturing our own proc as `old_proc`.
    let our_proc = no_minimize_wnd_proc as *const () as isize;
    let current_proc = GetWindowLongPtrW(hwnd, GWLP_WNDPROC);
    if current_proc != our_proc {
        wndproc_map().lock().unwrap_or_else(|e| e.into_inner()).insert(hwnd as usize, current_proc);
        SetWindowLongPtrW(hwnd, GWLP_WNDPROC, our_proc);
        // [HK-01] Keep the hook's lock-free view in step with the map.
        publish_overlay_snapshot();
    }

    // Route a frame recalc through the subclass: WM_NCCALCSIZE returns 0 ->
    // zero non-client area → the "top bar" can never render, regardless of
    // the current style bits.
    SetWindowPos(hwnd, std::ptr::null_mut(), 0, 0, 0, 0,
        SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER);
}

/// [BD-01] The DWM window attributes that must read "off" on every overlay,
/// as `(attribute, value)` pairs.
///
/// Split out from the FFI purely so the values can be unit-pinned: flipping
/// `DWMSBT_NONE` back to `DWMSBT_AUTO` (0) would silently re-open the overlay
/// to a system- or third-party-chosen backdrop, and that regression is
/// invisible in every environment CI can reach.
///
/// All three are pushed unconditionally because which one a given build
/// honors is a runtime property: `DWMWA_MICA` is the undocumented 21H2-only
/// spelling, `DWMWA_SYSTEMBACKDROP_TYPE` its 22523+ replacement.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn backdrop_denial_attrs() -> [(u32, u32); 3] {
    const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
    const DWMWA_SYSTEMBACKDROP_TYPE: u32 = 38;
    // Undocumented; superseded by DWMWA_SYSTEMBACKDROP_TYPE on 22523+.
    const DWMWA_MICA: u32 = 1029;
    const DWMSBT_NONE: u32 = 1;
    const DWMWCP_DONOTROUND: u32 = 1;

    [
        (DWMWA_SYSTEMBACKDROP_TYPE, DWMSBT_NONE),
        (DWMWA_MICA, 0),
        (DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND),
    ]
}

/// [BD-02] Pin every DWM backdrop knob on an overlay HWND to "off".
///
/// The overlay is a full-work-area transparent window sitting directly above
/// the wallpaper, so anything DWM composites into it covers the whole desktop:
/// a Mica/Acrylic backdrop here reads to the user as "my wallpaper is
/// permanently blurred" (themia-releases#74).
///
/// Themia never asks for a backdrop, but third-party injectors
/// (MicaForEveryone and friends) sweep every top-level window and push
/// backdrop type, corner preference, `DwmExtendFrameIntoClientArea(-1)` and
/// `DwmEnableBlurBehindWindow`. Our overlays already fail MicaForEveryone's
/// own eligibility filter twice over (`WS_EX_NOACTIVATE`, and
/// `WS_EX_TOOLWINDOW` with no title bar), so this is not a fix for a
/// reproduced application of it — it states the invariant "our overlays carry
/// no backdrop, ever" directly on our own window instead of depending on
/// another process's filter continuing to exclude us.
///
/// Best-effort by construction: the attributes are version-gated and DWM
/// returns `E_INVALIDARG` for the ones a given build doesn't implement, which
/// is the expected outcome for at least one of them on every Windows version.
#[cfg(target_os = "windows")]
unsafe fn deny_dwm_backdrop(hwnd: winapi::shared::windef::HWND) {
    use winapi::shared::minwindef::{FALSE, TRUE};
    use winapi::um::dwmapi::{
        DwmEnableBlurBehindWindow, DwmExtendFrameIntoClientArea, DwmSetWindowAttribute,
        DWM_BB_BLURREGION, DWM_BB_ENABLE, DWM_BLURBEHIND,
    };
    use winapi::um::uxtheme::MARGINS;
    use winapi::um::wingdi::{CreateRectRgn, DeleteObject};

    for (attr, value) in backdrop_denial_attrs() {
        // ignore: version-gated attribute. DWM answers E_INVALIDARG on builds
        // that don't implement it and there is nothing to recover — the other
        // spellings in the table cover those builds.
        let _ = DwmSetWindowAttribute(
            hwnd,
            attr,
            &value as *const u32 as *const winapi::ctypes::c_void,
            std::mem::size_of::<u32>() as u32,
        );
    }

    // Undo an "extend frame into client area" push. All-zero MARGINS is the
    // documented "no glass frame" state; an injector uses -1 ("whole window").
    let margins = MARGINS {
        cxLeftWidth: 0,
        cxRightWidth: 0,
        cyTopHeight: 0,
        cyBottomHeight: 0,
    };
    // ignore: best-effort reset. On failure the frame stays as it was, and the
    // overlay has no non-client area anyway (WM_NCCALCSIZE returns 0).
    let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);

    // [BD-04] Blur-behind is NOT a knob that can simply be switched off here:
    // tao implements `transparent: true` with it. At window creation it pushes
    // `DWM_BB_ENABLE | DWM_BB_BLURREGION`, `fEnable = TRUE` and an EMPTY blur
    // region (`CreateRectRgn(0, 0, -1, -1)`) — the documented way to ask DWM for
    // per-pixel alpha with no actual blur, since an empty region has no pixels
    // to blur. Disabling it makes the overlay OPAQUE. That was invisible while
    // the window stayed region-clipped to the widget shapes (SetWindowRgn keeps
    // the rest of the surface out of the window entirely), and appeared the
    // moment the region covered the whole window — a context-menu backdrop
    // ([S-17] tags it `data-themia-interactive`), edit mode, a modal — as a
    // black screen where the wallpaper had been.
    //
    // So re-assert tao's exact call instead of clearing it: an injector's real
    // blur is overwritten either way, and the overlay keeps its transparency,
    // including after a DWM restart ([BD-03]), which resets this attribute too.
    let region = CreateRectRgn(0, 0, -1, -1);
    if !region.is_null() {
        let blur = DWM_BLURBEHIND {
            dwFlags: DWM_BB_ENABLE | DWM_BB_BLURREGION,
            fEnable: TRUE,
            hRgnBlur: region,
            fTransitionOnMaximized: FALSE,
        };
        // ignore: best-effort reset, same reasoning as the frame reset above.
        let _ = DwmEnableBlurBehindWindow(hwnd, &blur);
        // DWM copies the region, so this HRGN is ours to free on every path.
        DeleteObject(region as winapi::shared::windef::HGDIOBJ);
    }
    // A null region is the one case where the call is skipped rather than made:
    // with DWM_BB_BLURREGION set, a NULL `hRgnBlur` means "the whole window",
    // i.e. exactly the wallpaper-wide blur this function exists to prevent.
}

// ---------------------------------------------------------------------------
// Always-on-top pin layer [PL-NN]
//
// A monitor's widgets all live in ONE overlay window, and z-order belongs to
// the window, so "keep THIS widget above other applications" needs a second
// window: `monitor-{i}-pin`, identical to `monitor-{i}` in bounds and content
// pipeline, but anchored `HWND_TOPMOST` instead of just-above-Progman. The
// frontend routes pinned widgets to it (src/pinLayer.ts) and everything else
// to the base overlay, so exactly one window paints each widget.
// ---------------------------------------------------------------------------

/// Which layer an overlay window is. Decides its z-anchor and its label.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OverlayLayer {
    /// `monitor-{i}`: below every real app window, above the wallpaper.
    Base,
    /// `monitor-{i}-pin`: permanently topmost, holds the pinned widgets.
    Pin,
}

/// Suffix that marks a pin overlay. Mirrored by `PIN_LAYER_SUFFIX` in
/// src/pinLayer.ts, which is what routes the webview to `PinLayer.tsx`.
const PIN_LAYER_SUFFIX: &str = "-pin";

/// The window label for one monitor slot on one layer.
fn overlay_label(slot: usize, layer: OverlayLayer) -> String {
    match layer {
        OverlayLayer::Base => format!("monitor-{slot}"),
        OverlayLayer::Pin => format!("monitor-{slot}{PIN_LAYER_SUFFIX}"),
    }
}

/// True for a pin overlay's label. Deliberately narrower than "ends with -pin":
/// every overlay label starts with `monitor-`, and no detached window does.
fn label_is_pin_layer(label: &str) -> bool {
    label.starts_with("monitor-") && label.ends_with(PIN_LAYER_SUFFIX)
}

/// The base overlay a pin overlay shadows. Returns the input unchanged for a
/// label that is already a base label, so callers need no second branch.
fn base_label_of(label: &str) -> String {
    if label_is_pin_layer(label) {
        label[..label.len() - PIN_LAYER_SUFFIX.len()].to_string()
    } else {
        label.to_string()
    }
}

/// Does the current layout hold at least one pinned widget? Written by the
/// `set_pin_layer` command from the base overlays, read by `sync_pin_overlays`.
/// Global rather than per-monitor because a view's widgets are mirrored onto
/// every monitor, so "something is pinned" is true everywhere or nowhere.
static PIN_LAYER_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// HWNDs of the live pin overlays. The z-order machinery reads it to keep them
/// topmost while it pushes every other overlay back down to the desktop band;
/// pruned in the `WM_NCDESTROY` arm alongside the other HWND-keyed maps.
#[cfg(target_os = "windows")]
static PIN_LAYER_HWNDS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashSet<usize>>,
> = std::sync::OnceLock::new();

#[cfg(target_os = "windows")]
fn pin_layer_hwnds() -> &'static std::sync::Mutex<std::collections::HashSet<usize>> {
    PIN_LAYER_HWNDS.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

#[cfg(target_os = "windows")]
fn hwnd_is_pin_layer(hwnd: winapi::shared::windef::HWND) -> bool {
    pin_layer_hwnds()
        .lock()
        .map(|s| s.contains(&(hwnd as usize)))
        .unwrap_or(false)
}

/// The z-order anchor THIS overlay wants.
///
/// A pin overlay is topmost by definition; every other overlay sits just above
/// Progman ([ZS-02]). Routing both through one function is what keeps a new
/// restack site from silently demoting the pin layer: there is no correct call
/// to `z_above_desktop()` on an overlay HWND that does not go through here.
#[cfg(target_os = "windows")]
fn z_anchor_for(hwnd: winapi::shared::windef::HWND) -> winapi::shared::windef::HWND {
    if hwnd_is_pin_layer(hwnd) {
        winapi::um::winuser::HWND_TOPMOST
    } else {
        z_above_desktop()
    }
}

/// Frontend reports whether any widget is pinned. Flipping it requests the
/// debounced display pass ([DW-02]) rather than building windows here: that
/// pass is the single writer for overlay creation and geometry, and reusing it
/// keeps the pin overlays on the same code path (and the same work-area
/// bounds) as the base ones. Idempotent, so every monitor's base overlay can
/// report the same answer without churn.
#[tauri::command]
fn set_pin_layer(enabled: bool) {
    use std::sync::atomic::Ordering;
    if PIN_LAYER_ENABLED.swap(enabled, Ordering::Relaxed) == enabled {
        return;
    }
    log::info!(target: "overlay", "[PL-01] pin layer {}", if enabled { "enabled" } else { "disabled" });
    #[cfg(target_os = "windows")]
    request_display_pass();
}

/// Make the live pin overlays match `PIN_LAYER_ENABLED` and the live BASE
/// overlays: one pin window per base window while enabled, none while not.
///
/// Geometry is read back from the base overlay's own window rect instead of
/// being recomputed from the monitor list, so the two layers cannot disagree
/// about the work area ([DW-01] keeps exactly one writer for those bounds).
///
/// Same threading contract as `reconcile_disabled_windows`: worker thread only,
/// never the main thread (`WebviewWindowBuilder::build` needs the pump).
#[cfg(target_os = "windows")]
fn sync_pin_overlays(app: &tauri::AppHandle) {
    use std::sync::atomic::Ordering;
    use tauri::Manager;

    let enabled = PIN_LAYER_ENABLED.load(Ordering::Relaxed);
    let labels: Vec<String> = app.webview_windows().into_keys().collect();

    // Close every pin overlay that should not be up: the feature is off, or
    // the base overlay it shadows is gone (monitor unplugged or disabled).
    for label in labels.iter().filter(|l| label_is_pin_layer(l)) {
        let base = base_label_of(label);
        if enabled && labels.iter().any(|l| *l == base) {
            continue;
        }
        drag_drop::revoke_drop_target(label);
        if let Some(win) = app.get_webview_window(label) {
            if let Err(e) = win.close() {
                log::error!(target: "overlay", "[PL-02] close failed for {label}: {e:?}");
            }
        }
        if let Ok(mut map) = window_monitor_map().lock() {
            map.remove(label);
        }
    }
    if !enabled {
        return;
    }

    // One pin overlay per live base overlay, at the base overlay's bounds.
    // `parse::<usize>()` is what filters the pin labels back out: they end in
    // the suffix, so they never parse.
    for base_label in labels.iter() {
        let Some(slot) = base_label
            .strip_prefix("monitor-")
            .and_then(|s| s.parse::<usize>().ok())
        else {
            continue;
        };
        let Some(base) = app.get_webview_window(base_label) else { continue };
        let Some(rect) = window_screen_rect(&base) else {
            log::error!(target: "overlay", "[PL-02] {base_label} has no live HWND, skipping its pin layer");
            continue;
        };
        let (x, y, width, height) = rect;
        let monitor_id = window_monitor_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(base_label)
            .cloned()
            .unwrap_or_default();
        let pin_label = overlay_label(slot, OverlayLayer::Pin);

        match app.get_webview_window(&pin_label) {
            Some(win) if win.hwnd().is_ok() => {
                if let Err(e) = pin_to_desktop(&win, x, y, width, height, OverlayLayer::Pin) {
                    log::error!(target: "overlay", "[PL-02] re-pin failed for {pin_label}: {e:?}");
                } else {
                    let dpr = win.scale_factor().unwrap_or(1.0);
                    drag_drop::update_drop_target_position(&pin_label, x, y, dpr);
                }
                // Same notification the base overlay gets: re-resolve the
                // monitor (and with it the view assignment + widget scale)
                // without a hard reload. [MI-01].
                if let Err(e) = win.eval("window.dispatchEvent(new CustomEvent('themia:display-changed'))") {
                    log::error!(target: "overlay", "[PL-02] display-changed eval failed for {pin_label}: {e:?}");
                }
            }
            Some(win) => {
                // Husk with a dead HWND: same [DW-06] treatment as a base
                // overlay, destroy and rebuild rather than leaving it blank.
                log::error!(target: "overlay", "[PL-02] {pin_label} has no live HWND, rebuilding");
                drag_drop::revoke_drop_target(&pin_label);
                if let Err(e) = win.destroy() {
                    log::error!(target: "overlay", "[PL-02] destroy failed for {pin_label}: {e:?}");
                }
                if let Err(e) = build_widget_window(
                    app, slot, x, y, width, height, &monitor_id, OverlayLayer::Pin,
                ) {
                    log::error!(target: "overlay", "[PL-02] rebuild failed for {pin_label}: {e:?}");
                }
            }
            None => {
                if let Err(e) = build_widget_window(
                    app, slot, x, y, width, height, &monitor_id, OverlayLayer::Pin,
                ) {
                    log::error!(target: "overlay", "[PL-02] build failed for {pin_label}: {e:?}");
                }
            }
        }
    }
}

#[cfg(test)]
mod pin_layer_tests {
    use super::{base_label_of, label_is_pin_layer, overlay_label, OverlayLayer};

    #[test]
    fn pl01_labels_are_the_base_label_plus_a_suffix() {
        assert_eq!(overlay_label(0, OverlayLayer::Base), "monitor-0");
        assert_eq!(overlay_label(0, OverlayLayer::Pin), "monitor-0-pin");
        assert_eq!(overlay_label(12, OverlayLayer::Pin), "monitor-12-pin");
    }

    #[test]
    fn pl01_pin_labels_are_recognised_and_nothing_else_is() {
        assert!(label_is_pin_layer("monitor-0-pin"));
        assert!(label_is_pin_layer("monitor-3-pin"));
        assert!(!label_is_pin_layer("monitor-0"));
        // Detached windows never look like an overlay, whatever they are named.
        for l in ["config", "connect", "widget-window", "tray-flyout", "layout", "pin"] {
            assert!(!label_is_pin_layer(l), "{l} must not read as a pin overlay");
        }
    }

    #[test]
    fn pl02_base_label_round_trips_and_is_identity_on_a_base_label() {
        assert_eq!(base_label_of("monitor-2-pin"), "monitor-2");
        assert_eq!(base_label_of("monitor-2"), "monitor-2");
        for slot in [0usize, 1, 7, 40] {
            let base = overlay_label(slot, OverlayLayer::Base);
            assert_eq!(base_label_of(&overlay_label(slot, OverlayLayer::Pin)), base);
        }
    }

    /// The pin label must never parse as a monitor SLOT. Three call sites rely
    /// on that to tell the layers apart with nothing but a `parse::<usize>()`:
    /// `bound_monitor_slots` ([MI-06] slot binding), the disabled-window
    /// reconcile, and `sync_pin_overlays`' "one pin per live base" loop. If it
    /// ever parsed, a pin overlay would be mistaken for its own base and the
    /// sync would build a pin overlay for the pin overlay.
    #[test]
    fn pl02_a_pin_label_never_parses_as_a_slot() {
        for slot in [0usize, 1, 9, 15] {
            let pin = overlay_label(slot, OverlayLayer::Pin);
            let parsed = pin.strip_prefix("monitor-").and_then(|s| s.parse::<usize>().ok());
            assert_eq!(parsed, None, "{pin} must not parse as a slot");
            let base = overlay_label(slot, OverlayLayer::Base);
            assert_eq!(
                base.strip_prefix("monitor-").and_then(|s| s.parse::<usize>().ok()),
                Some(slot),
            );
        }
    }
}

/// A live window's screen rect in physical pixels, or None if its HWND is gone.
#[cfg(target_os = "windows")]
fn window_screen_rect(window: &tauri::WebviewWindow) -> Option<(i32, i32, i32, i32)> {
    use winapi::shared::windef::{HWND, RECT};
    use winapi::um::winuser::{GetWindowRect, IsWindow};

    let hwnd = window.hwnd().ok()?.0 as usize as HWND;
    if hwnd.is_null() {
        return None;
    }
    unsafe {
        if IsWindow(hwnd) == 0 {
            return None;
        }
        let mut r: RECT = std::mem::zeroed();
        if GetWindowRect(hwnd, &mut r) == 0 {
            return None;
        }
        Some((r.left, r.top, r.right - r.left, r.bottom - r.top))
    }
}

#[cfg(target_os = "windows")]
fn pin_to_desktop(
    window: &tauri::WebviewWindow,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    layer: OverlayLayer,
) -> Result<(), Box<dyn std::error::Error>> {
    use winapi::shared::windef::HWND;
    use winapi::um::winuser::{
        GetWindowLongW, SetWindowLongW, SetWindowPos,
        GWL_EXSTYLE, SWP_NOACTIVATE,
        WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    };

    let hwnd = window.hwnd()?.0 as usize as HWND;

    // Register BEFORE any restack below, so `z_anchor_for` (and the subclass
    // installed by `reassert_frameless`, which starts rewriting
    // WM_WINDOWPOSCHANGING immediately) already sees this HWND as topmost.
    if layer == OverlayLayer::Pin {
        pin_layer_hwnds()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(hwnd as usize);
    }

    unsafe {
        // Frame styles + subclass + frame recalc. Extracted into
        // `reassert_frameless` so the same invariant can be re-applied later
        // (from `apply_widget_region`) whenever the visible shape changes.
        reassert_frameless(hwnd);

        // Extended style: hide from taskbar/Alt-Tab, prevent activation.
        // Click-through (WS_EX_LAYERED | WS_EX_TRANSPARENT) is intentionally
        // NOT set here. Earlier iterations toggled it via a polling thread
        // to forward empty-area clicks to the Windows desktop, but in
        // practice that scheme broke widget interactivity in real Tauri /
        // WebView2 runs (the unit + Win32 integration tests passed, but
        // live clicks didn't reach buttons / right-click menus). Widgets
        // now capture all input over their bounds; desktop pass-through
        // is disabled until a less invasive mechanism is in place.
        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        SetWindowLongW(
            hwnd,
            GWL_EXSTYLE,
            ((ex | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE) & !WS_EX_APPWINDOW) as i32,
        );

        // [BD-02] No system or third-party backdrop on the overlay: it spans
        // the whole work area, so any backdrop reads as a permanently blurred
        // wallpaper. Re-asserted from the WndProc on DWM composition changes
        // ([BD-03]); nothing polls for it.
        deny_dwm_backdrop(hwnd);

        // Set exact physical-pixel work-area bounds and pin z-order: the base
        // overlay to "just above Progman", surviving Show Desktop while still
        // sitting below every real top-level app window; a pin overlay to the
        // topmost band, which is the whole point of it ([PL-01]).
        SetWindowPos(hwnd, z_anchor_for(hwnd), x, y, width, height, SWP_NOACTIVATE);

        // Final recalc through the subclass, after the extended-style and
        // positioning passes, so the window settles frameless regardless of
        // anything those touched.
        reassert_frameless(hwnd);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Windows: background thread listening for display / taskbar changes
// ---------------------------------------------------------------------------

/// `WM_WTSSESSION_CHANGE` wParam → is this the "screen unlocked" transition?
/// Only `WTS_SESSION_UNLOCK` replays the startup animation; every other
/// session event (lock, logon, logoff, console/remote connect-disconnect) is
/// ignored so the warp only fires when the user actually returns to the
/// desktop. Pure so it can be unit-pinned without a session.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn session_change_is_unlock(wparam: usize) -> bool {
    const WTS_SESSION_UNLOCK: usize = 0x8;
    wparam == WTS_SESSION_UNLOCK
}

/// `WM_WTSSESSION_CHANGE` wParam → is this the "session locked" transition?
/// Screen-time accrual pauses on lock (screentime SPEC [S-23]). Pure so it can
/// be unit-pinned without a session.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn session_change_is_lock(wparam: usize) -> bool {
    const WTS_SESSION_LOCK: usize = 0x7;
    wparam == WTS_SESSION_LOCK
}

#[cfg(target_os = "windows")]
fn start_display_watcher() {
    std::thread::spawn(|| {
        use std::ptr;
        use winapi::shared::minwindef::{BOOL, DWORD, LPARAM, LRESULT, UINT, WPARAM};
        use winapi::shared::windef::HWND;
        use winapi::um::libloaderapi::GetModuleHandleW;

        // wtsapi32.dll session-notification registration: the winapi crate ships
        // the WTS_SESSION_* / WM_WTSSESSION_CHANGE constants but not these two
        // entry points, so declare + link them here.
        #[link(name = "wtsapi32")]
        extern "system" {
            fn WTSRegisterSessionNotification(hwnd: HWND, flags: DWORD) -> BOOL;
            fn WTSUnRegisterSessionNotification(hwnd: HWND) -> BOOL;
        }
        use winapi::um::winuser::{
            CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW,
            RegisterClassW, TranslateMessage, WS_EX_TOOLWINDOW, MSG, WNDCLASSW,
        };

        const WM_DISPLAYCHANGE: UINT = 0x007E;
        const WM_SETTINGCHANGE: UINT = 0x001A;
        // Session lock/unlock, delivered after WTSRegisterSessionNotification.
        const WM_WTSSESSION_CHANGE: UINT = 0x02B1;
        const WM_DESTROY: UINT = 0x0002;

        unsafe extern "system" fn wnd_proc(
            hwnd: HWND,
            msg: UINT,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            use std::sync::atomic::Ordering;

            // Screen unlocked → broadcast so every widget window replays its
            // startup animation (the warp greets the user on return). Only the
            // UNLOCK transition qualifies; session_change_is_unlock filters out
            // lock / logon / logoff / console+remote connect-disconnect.
            if msg == WM_WTSSESSION_CHANGE && session_change_is_unlock(wparam) {
                if let Some(app) = WATCHER_APP.get() {
                    use tauri::Emitter;
                    if let Err(e) = app.emit("screen-unlocked", ()) {
                        log::error!(target: "screen-unlocked", "emit failed: {e:?}");
                    }
                }
                // Screen-time accrual resumes on unlock (screentime SPEC [S-23]).
                widget_screentime::note_unlock();
            }

            // Screen-time accrual pauses while the session is locked; the lock
            // transition also flushes the store (screentime SPEC [S-23]/[S-29]).
            if msg == WM_WTSSESSION_CHANGE && session_change_is_lock(wparam) {
                widget_screentime::note_lock();
            }

            // RAII: drop the session-notification registration on the destroy
            // path so it is freed on every teardown route, not leaked.
            if msg == WM_DESTROY {
                WTSUnRegisterSessionNotification(hwnd);
            }

            let taskbar_created = TASKBAR_CREATED_MSG.load(Ordering::Relaxed);
            let is_taskbar_created = taskbar_created != 0 && msg == taskbar_created;

            if msg == WM_DISPLAYCHANGE || msg == WM_SETTINGCHANGE || is_taskbar_created {
                // explorer.exe restart invalidates the cached shell HWNDs
                // ([DI-05]; resolve_desktop_shell_view also revalidates, so a
                // missed broadcast degrades to a re-resolve, not a dead handle).
                if is_taskbar_created {
                    DESKTOP_ICONS_HWND.store(0, Ordering::Relaxed);
                    DESKTOP_DEFVIEW_HWND.store(0, Ordering::Relaxed);
                    PROGMAN_HWND.store(0, Ordering::Relaxed);
                }

                // Record the event; the trailing-debounce thread will fire
                // on_display_change once events stop arriving. on_display_change
                // calls pin_to_desktop for each monitor window, which re-attaches
                // to the (newly-resolved) wallpaper WorkerW. Monotonic counter,
                // not a timestamp ([DW-03]).
                request_display_pass();
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }

        unsafe {
            let hinstance = GetModuleHandleW(ptr::null());
            let class: Vec<u16> = "WidgetWatcher\0".encode_utf16().collect();

            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(wnd_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinstance,
                hIcon: ptr::null_mut(),
                hCursor: ptr::null_mut(),
                hbrBackground: ptr::null_mut(),
                lpszMenuName: ptr::null(),
                lpszClassName: class.as_ptr(),
            };
            RegisterClassW(&wc);

            // Invisible top-level window, receives broadcast messages
            // (WM_DISPLAYCHANGE, WM_SETTINGCHANGE). HWND_MESSAGE windows do NOT.
            let watcher_hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                class.as_ptr(),
                ptr::null(),
                0, // no WS_VISIBLE
                0, 0, 1, 1,
                ptr::null_mut(), // desktop-parent = top-level = receives broadcasts
                ptr::null_mut(),
                hinstance,
                ptr::null_mut(),
            );

            // Ask the session manager to deliver WM_WTSSESSION_CHANGE here, so
            // wnd_proc sees lock/unlock. NOTIFY_FOR_THIS_SESSION (0) scopes it to
            // the user's own session. Unregistered on WM_DESTROY (wnd_proc).
            // winapi exposes the WTS_SESSION_* constants but not these two
            // wtsapi32.dll entry points, so declare them ourselves.
            if !watcher_hwnd.is_null() {
                const NOTIFY_FOR_THIS_SESSION: DWORD = 0;
                WTSRegisterSessionNotification(watcher_hwnd, NOTIFY_FOR_THIS_SESSION);
            }

            // Register the shell's "TaskbarCreated" broadcast so we can detect
            // explorer.exe restarts and re-resolve Progman/WorkerW handles.
            use std::sync::atomic::Ordering;
            use winapi::um::winuser::RegisterWindowMessageW;
            let tb_msg_name: Vec<u16> = "TaskbarCreated\0".encode_utf16().collect();
            let tb_msg = RegisterWindowMessageW(tb_msg_name.as_ptr());
            if tb_msg != 0 {
                TASKBAR_CREATED_MSG.store(tb_msg, Ordering::Relaxed);
            }

            // [HK-01] Start the off-hook emitter before the hooks that feed it,
            // so the very first click or keystroke already has somewhere to
            // hand its work.
            start_hook_emitter();

            // Install the two global low-level input hooks:
            //   * WH_KEYBOARD_LL intercepts Ctrl+C/V/X/A, Delete and Escape for
            //     the active widget without bringing any window to the
            //     foreground.
            //   * WH_MOUSE_LL dismisses Start/Search the instant a real OS click
            //     lands on a widget, skipping the JS → IPC round-trip.
            // [HK-02] Handles are retained so `rearm_input_hooks_if_dead` can
            // reinstall them if Windows evicts them for a timeout.
            // [HK-03] They live on their own thread, not this one: this thread's
            // pump also services `wnd_proc`, which emits to Tauri on session
            // unlock, and a callback that waits behind that is a callback the OS
            // evicts.
            use winapi::um::winuser::SetWinEventHook;
            start_input_hook_thread();

            // Clear WIDGET_KB_ACTIVE whenever any other app's window becomes
            // foreground, so we stop intercepting keys outside our widgets.
            const EVENT_SYSTEM_FOREGROUND: u32 = 0x0003;
            const EVENT_OBJECT_CLOAKED:    u32 = 0x8017;
            const WINEVENT_OUTOFCONTEXT:   u32 = 0x0000;
            const WINEVENT_SKIPOWNPROCESS: u32 = 0x0002;
            SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND,
                ptr::null_mut(),
                Some(foreground_change_proc),
                0, 0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            );

            // Win+D / Show Desktop on Win10/11 cloaks windows via DWM rather
            // than minimizing them. Uncloak any of our widget HWNDs the moment
            // the shell cloaks them. We must NOT skip own process, since the
            // cloak event fires for windows in our own process.
            SetWinEventHook(
                EVENT_OBJECT_CLOAKED, EVENT_OBJECT_CLOAKED,
                ptr::null_mut(),
                Some(cloaked_event_proc),
                0, 0,
                WINEVENT_OUTOFCONTEXT,
            );

            // Trailing-debounce thread: waits for display-change events to
            // settle, then fires on_display_change once after a quiet period.
            std::thread::spawn(|| {
                use std::sync::atomic::Ordering;
                const QUIET_MS: u64 = 600;
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                    let seq = DISPLAY_CHANGE_SEQ.load(Ordering::Relaxed);
                    let handled = DISPLAY_HANDLED_SEQ.load(Ordering::Relaxed);
                    if display_pass_due(seq, handled) {
                        // Event pending, wait for a quiet period with no new events
                        loop {
                            let snapshot = DISPLAY_CHANGE_SEQ.load(Ordering::Relaxed);
                            std::thread::sleep(std::time::Duration::from_millis(QUIET_MS));
                            if display_events_settled(
                                snapshot,
                                DISPLAY_CHANGE_SEQ.load(Ordering::Relaxed),
                            ) {
                                DISPLAY_HANDLED_SEQ.store(snapshot, Ordering::Relaxed);
                                if let Some(app) = WATCHER_APP.get() {
                                    on_display_change(app);
                                }
                                break;
                            }
                        }
                    }
                }
            });

            // Z-order reconciliation poll. The EVENT_SYSTEM_FOREGROUND hook
            // catches most Show Desktop transitions, but on multi-monitor
            // setups (per-monitor WorkerW activations) and under load the
            // hook can miss events. The shell can re-elevate its WorkerW
            // very quickly on secondary monitors, so the poll runs at 80ms
            // (roughly one frame at 60Hz) whenever anything is happening.
            //
            // [ZP-01] It does NOT run at 80ms forever. That is 12.5 timer
            // wakeups a second for the entire uptime of the app, which keeps
            // the CPU package out of its deep C-states and shows up as a
            // permanent idle draw on battery. The fast rate only has to cover
            // the window around a shell transition, so the loop drops to 500ms
            // once nothing has happened for `ZORDER_FAST_TICKS` ticks and
            // snaps back the moment `SHELL_ACTIVITY_SEQ` moves or the shell
            // state is one where a race is possible at all.
            std::thread::spawn(|| {
                let mut fast_ticks: u32 = ZORDER_FAST_TICKS;
                let mut seen_seq: u64 = 0;
                let mut since_hook_check_ms: u64 = 0;
                let mut since_cloak_check_ms: u64 = 0;

                loop {
                    let sleep_ms = zorder_poll_interval_ms(fast_ticks);
                    std::thread::sleep(std::time::Duration::from_millis(sleep_ms));

                    ensure_widget_zorder_tick();
                    // Work-area bounds backstop ([DW-04]), internally divided
                    // to ~1s; requests the debounced re-pin pass on divergence.
                    ensure_work_area_tick();

                    // [ZC-01] Cloak backstop on its own coarse cadence: the
                    // EVENT_OBJECT_CLOAKED hook is lossy under load (a
                    // fullscreen game), and a missed uncloak is permanent.
                    since_cloak_check_ms += sleep_ms;
                    if since_cloak_check_ms >= CLOAK_CHECK_INTERVAL_MS {
                        since_cloak_check_ms = 0;
                        ensure_widget_cloak_tick();
                    }

                    let seq = SHELL_ACTIVITY_SEQ.load(std::sync::atomic::Ordering::Relaxed);
                    let moved = seq != seen_seq;
                    seen_seq = seq;
                    fast_ticks = next_fast_ticks(fast_ticks, moved || zorder_race_possible());

                    // [HK-02] Hook liveness, on its own coarse cadence. This
                    // thread only decides; the hook thread does the installing.
                    since_hook_check_ms += sleep_ms;
                    if since_hook_check_ms >= HOOK_HEALTH_INTERVAL_MS {
                        rearm_input_hooks_if_dead(since_hook_check_ms);
                        since_hook_check_ms = 0;
                    }
                }
            });

            let mut msg: MSG = std::mem::zeroed();
            loop {
                let r = GetMessageW(&mut msg, ptr::null_mut(), 0, 0);
                if r <= 0 { break; }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    });
}

#[cfg(target_os = "windows")]
fn on_display_change(app: &tauri::AppHandle) {
    // Marshal onto the main thread: Tauri WebviewWindow operations and Win32
    // calls touching the message loop can panic when invoked from the display
    // watcher background thread.
    let app_clone = app.clone();
    let _ = app.run_on_main_thread(move || {
        // Catch any panic so the watcher thread keeps running and the app
        // does not crash if a window/HWND becomes invalid mid-operation.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            on_display_change_impl(&app_clone);
        }));
    });
}

#[cfg(target_os = "windows")]
fn on_display_change_impl(app: &tauri::AppHandle) {
    let monitors = enumerate_monitors();
    if monitors.is_empty() {
        return; // Nothing to do; avoid destroying the last window.
    }
    // [MI-03] Idempotent + run-once. Called here as well as at startup because
    // the startup enumeration can legitimately be empty ([DW-07]), which defers
    // the migration to the first pass that sees a real topology — this one.
    migrate_disabled_monitors_file(app, &monitors);
    let disabled = read_disabled_monitors(app);

    // [MI-06] Bind each monitor to the slot its own overlay already occupies,
    // NOT to its enumeration index. Must be read before the map is mutated
    // below, and drives the label used by every branch of this pass (#93).
    let slots = assign_monitor_slots(&monitors, &bound_monitor_slots(app));

    // Every label this pass has already decided on (kept, built, or closed
    // because its monitor is disabled). Anything else under the `monitor-`
    // prefix is an orphan and gets closed below.
    let mut handled: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Update map and reposition/create windows for current monitors
    for (i, m) in monitors.iter().enumerate() {
        let slot = slots.get(i).copied().unwrap_or(i);
        let label = format!("monitor-{slot}");
        if let Ok(mut map) = window_monitor_map().lock() {
            map.insert(label.clone(), m.id.clone());
        }

        // Monitor disabled in Display Settings: make sure no overlay
        // exists for it (close one inherited from a different monitor that
        // previously owned this label slot) and skip the create path.
        if !monitor_should_have_window(&m.id, &disabled) {
            if let Some(win) = app.get_webview_window(&label) {
                drag_drop::revoke_drop_target(&label);
                if let Err(e) = win.close() {
                    log::error!(target: "displays", "close failed for disabled {label}: {e:?}");
                }
                if let Ok(mut map) = window_monitor_map().lock() {
                    map.remove(&label);
                }
            }
            // Already decided: the orphan sweep must not close it a second time
            // while Tauri is still tearing it down.
            handled.insert(label);
            continue;
        }

        if let Some(window) = app.get_webview_window(&label) {
            // Validate the window still has a live HWND before touching it.
            if window.hwnd().is_err() {
                // Tauri still tracks the window but its HWND is gone (webview
                // process crash / teardown race). Drop the husk and rebuild
                // fresh, otherwise the slot stays dead until restart [DW-06].
                log::error!(target: "displays", "{label} has no live HWND, rebuilding");
                drag_drop::revoke_drop_target(&label);
                if let Err(e) = window.destroy() {
                    log::error!(target: "displays", "destroy failed for dead {label}: {e:?}");
                }
                if let Err(e) = build_widget_window(
                    app, slot, m.x, m.y, m.width, m.height, &m.id, OverlayLayer::Base,
                ) {
                    log::error!(target: "displays", "rebuild failed for {label}: {e:?}");
                }
            } else {
                if let Err(e) = pin_to_desktop(&window, m.x, m.y, m.width, m.height, OverlayLayer::Base) {
                    log::error!(target: "displays", "pin_to_desktop failed for {label}: {e:?}");
                } else {
                    // The overlay moved to this monitor's new origin/DPI. Refresh
                    // the OLE drop target's screen→client mapping so native
                    // drag/drop coords and widget-rect routing stay correct.
                    // Mirror the build path's geometry: monitor origin + the
                    // window's scale factor.
                    let dpr = window.scale_factor().unwrap_or(1.0);
                    drag_drop::update_drop_target_position(&label, m.x, m.y, dpr);
                }
                // Notify JS so it can re-init for the new monitor without a
                // hard reload (which races with the next display event).
                if let Err(e) = window.eval("window.dispatchEvent(new CustomEvent('themia:display-changed'))") {
                    log::error!(target: "displays", "display-changed eval failed for {label}: {e:?}");
                }
            }
        } else {
            // [DW-07] The build path that covers "no overlays exist yet",
            // including the startup enumeration having come back empty.
            if let Err(e) = build_widget_window(
                app, slot, m.x, m.y, m.width, m.height, &m.id, OverlayLayer::Base,
            ) {
                log::error!(target: "displays", "build_widget_window failed for {label}: {e:?}");
            }
        }
        handled.insert(label);
    }

    // Close orphaned windows: any `monitor-*` overlay no longer claimed by a
    // connected, enabled monitor (a panel that was unplugged or powered off).
    // Enumerate the live window set rather than counting up from
    // `monitors.len()`: [MI-06] slots follow the panel, so they are not
    // guaranteed to be `0..len` and a gap must not stop the scan (#93). Use
    // close() rather than destroy() so Tauri can run its teardown gracefully.
    let orphans: Vec<String> = app
        .webview_windows()
        .into_keys()
        // [PL-02] Pin overlays are not orphans to this sweep: they are keyed off
        // the LIVE base overlays, which this pass is still rebuilding, so
        // `sync_pin_overlays` below decides their fate once the bases settle.
        .filter(|label| {
            label.starts_with("monitor-") && !label_is_pin_layer(label) && !handled.contains(label)
        })
        .collect();
    for label in orphans {
        let Some(window) = app.get_webview_window(&label) else { continue };
        if let Ok(mut map) = window_monitor_map().lock() {
            map.remove(&label);
        }
        // Revoke OLE drop target before closing
        drag_drop::revoke_drop_target(&label);
        if let Err(e) = window.close() {
            log::error!(target: "displays", "window.close failed for {label}: {e:?}");
        }
    }

    // [PL-02] Base overlays have settled; mirror them onto the pin layer.
    sync_pin_overlays(app);

    // Keep the config window's Displays tree in sync: it isn't a `monitor-*`
    // window so it doesn't get the per-monitor eval above.
    if let Some(config) = app.get_webview_window("config") {
        if let Err(e) = config.eval("window.dispatchEvent(new CustomEvent('themia:display-changed'))") {
            log::error!(target: "displays", "display-changed eval failed for config: {e:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// Work-area self-heal sweep [DW-04]
// ---------------------------------------------------------------------------
//
// The event path (WM_DISPLAYCHANGE / WM_SETTINGCHANGE / TaskbarCreated)
// covers the documented ways the work area changes, but real desktops drop
// or re-order those broadcasts: a login race where the taskbar docks after
// the overlay was sized, rcWork still stale when the debounce fires, RDP and
// dock/undock transitions. The user-visible failure was an overlay
// overlapping the taskbar until the app was restarted. This sweep is the
// backstop: every ~1s it compares each overlay's actual bounds against the
// live rcWork and, when they diverge on two consecutive sweeps, requests the
// SAME debounced re-pin pass the event path uses: one heal path, never a
// second positioning implementation.

const WORK_AREA_STREAK_THRESHOLD: u32 = 2;

/// [DW-04] Streak accumulator: divergence must hold for two consecutive
/// sweeps before a heal fires, so a sweep landing mid-transition (taskbar
/// still animating / docking) can't trigger a spurious pass. Returns
/// `(new_streak, fire_now)`; firing resets the streak so healing re-arms.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn mismatch_streak_advance(streak: u32, diverged: bool) -> (u32, bool) {
    if !diverged {
        return (0, false);
    }
    let s = streak.saturating_add(1);
    if s >= WORK_AREA_STREAK_THRESHOLD {
        (0, true)
    } else {
        (s, false)
    }
}

/// [DW-01] An overlay is correctly pinned when its window rect equals its
/// monitor's work area exactly: physical pixels, GetWindowRect
/// (left, top, right, bottom) vs the work area's origin + size. Anything
/// else (full-monitor height spanning the taskbar band, origin drift after
/// the taskbar re-docks to another edge) is divergence.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn overlay_rect_matches_work_area(
    rect: (i32, i32, i32, i32),
    mon_x: i32,
    mon_y: i32,
    mon_w: i32,
    mon_h: i32,
) -> bool {
    rect == (mon_x, mon_y, mon_x + mon_w, mon_y + mon_h)
}

#[cfg(target_os = "windows")]
static WORK_AREA_SWEEP_TICK: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
#[cfg(target_os = "windows")]
static WORK_AREA_MISMATCH_STREAK: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);

/// One sweep over every connected monitor: does any overlay's live state
/// differ from what `on_display_change` would produce? Missing overlay for
/// an enabled monitor and a dead HWND both count ([DW-06]); the re-pin pass
/// rebuilds those. The disabled-monitor set is read lazily so the steady
/// state (all overlays present and correctly pinned) never touches the disk.
#[cfg(target_os = "windows")]
fn overlay_work_area_diverged(app: &tauri::AppHandle, monitors: &[MonitorInfo]) -> bool {
    use winapi::shared::windef::{HWND, RECT};
    use winapi::um::winuser::{GetWindowRect, IsWindow};

    let mut disabled: Option<std::collections::HashSet<String>> = None;
    // [MI-06] Same slot assignment the re-pin pass will use, so the sweep
    // compares each overlay against ITS OWN monitor. Deriving the label from
    // the enumeration index here would report permanent divergence (and fire a
    // pass every ~2s) on any topology whose slots are not `0..len` (#93).
    let slots = assign_monitor_slots(monitors, &bound_monitor_slots(app));

    for (i, m) in monitors.iter().enumerate() {
        let label = format!("monitor-{}", slots.get(i).copied().unwrap_or(i));
        let Some(win) = app.get_webview_window(&label) else {
            // Missing overlay is divergence only when the monitor is enabled
            // ([DW-06]); a user-disabled monitor legitimately has none. Keyed
            // on the stable id ([MI-01]/[MI-05]), like every other gate.
            let d = disabled.get_or_insert_with(|| read_disabled_monitors(app));
            if monitor_should_have_window(&m.id, d) {
                return true;
            }
            continue;
        };
        let Ok(h) = win.hwnd() else {
            return true; // Tauri tracks the window but its HWND is gone [DW-06]
        };
        let hwnd = h.0 as usize as HWND;
        unsafe {
            if IsWindow(hwnd) == 0 {
                return true;
            }
            let mut r: RECT = std::mem::zeroed();
            if GetWindowRect(hwnd, &mut r) == 0 {
                return true;
            }
            if !overlay_rect_matches_work_area(
                (r.left, r.top, r.right, r.bottom),
                m.x,
                m.y,
                m.width,
                m.height,
            ) {
                return true;
            }
        }
    }
    false
}

/// [DW-04] Called from the 80ms reconciliation thread; internally divided to
/// ~1s. Stands down while polling is paused (fullscreen/game mode) and while
/// a re-pin pass is already queued or debouncing, since the sweep would only
/// observe the transitional state that pass is about to fix.
#[cfg(target_os = "windows")]
fn ensure_work_area_tick() {
    use std::sync::atomic::Ordering;

    // ~80ms tick × 12 ≈ 1s between sweeps. GetWindowRect + GetMonitorInfoW
    // are microsecond-cheap, but there is no need to run them at frame rate.
    if !WORK_AREA_SWEEP_TICK
        .fetch_add(1, Ordering::Relaxed)
        .is_multiple_of(12)
    {
        return;
    }
    if POLLING_PAUSED.load(Ordering::Relaxed) {
        return;
    }
    if display_pass_due(
        DISPLAY_CHANGE_SEQ.load(Ordering::Relaxed),
        DISPLAY_HANDLED_SEQ.load(Ordering::Relaxed),
    ) {
        WORK_AREA_MISMATCH_STREAK.store(0, Ordering::Relaxed);
        return;
    }
    let Some(app) = WATCHER_APP.get() else { return };
    let monitors = enumerate_monitors();
    if monitors.is_empty() {
        return; // mirror on_display_change: never act on an empty topology
    }
    let diverged = overlay_work_area_diverged(app, &monitors);
    let (streak, fire) = mismatch_streak_advance(
        WORK_AREA_MISMATCH_STREAK.load(Ordering::Relaxed),
        diverged,
    );
    WORK_AREA_MISMATCH_STREAK.store(streak, Ordering::Relaxed);
    if fire {
        log::warn!(
            target: "displays",
            "overlay bounds diverged from monitor work area, requesting re-pin pass"
        );
        request_display_pass();
    }
}

// ---------------------------------------------------------------------------
// Tests for the SetWindowRgn-based desktop pass-through
// ---------------------------------------------------------------------------
//
// The overlay clips itself to the union of all widget + chrome rectangles via
// SetWindowRgn. Areas outside the region are not part of the window at all,
// so the Windows desktop (Progman / SysListView32) receives clicks and
// right-clicks naturally through the gaps between widgets.
//
// The tests cover:
//   - `build_widget_region` produces a region whose bounding box matches the
//     union of inputs (the user-visible shape).
//   - Multiple disjoint widgets are all part of the region simultaneously
//     (this is the property the user asked about explicitly: "multiple
//     squares unified").
//   - Empty input produces a null HRGN (caller must pass null to
//     SetWindowRgn to clear the clip).
//   - Real-Win32 integration: applying the region to a live window updates
//     `GetWindowRgnBox` to match the expected union.

#[cfg(test)]
mod tests {
    use super::*;

    fn sr(left: i32, top: i32, right: i32, bottom: i32, radius: i32) -> ShapedRect {
        ShapedRect { left, top, right, bottom, radius }
    }

    // -----------------------------------------------------------------
    // [MI-01]: the config window's target survives the URL
    // -----------------------------------------------------------------
    //
    // Regression: a real Windows monitor id is a device path full of `#`.
    // Concatenated into the query raw, the first one started the URL fragment
    // and the id arrived truncated to `\\?\DISPLAY`, so the Display page read
    // and wrote a phantom monitor's settings and the display it was opened for
    // never changed.

    /// What `ConfigWindow` gets back out of the query, i.e. what
    /// `URLSearchParams.get('target')` yields for this URL.
    fn target_param_of(url: &str) -> Option<String> {
        let no_fragment = url.split('#').next().unwrap_or("");
        let query = no_fragment.split('?').nth(1)?;
        for pair in query.split('&') {
            let (k, v) = pair.split_once('=')?;
            if k != "target" {
                continue;
            }
            // URLSearchParams: `+` is a space, then percent-decode.
            let decoded = percent_encoding::percent_decode_str(&v.replace('+', " "))
                .decode_utf8_lossy()
                .into_owned();
            return Some(decoded);
        }
        None
    }

    #[test]
    fn mi01_a_windows_monitor_id_survives_the_config_window_url() {
        let id = r"\\?\DISPLAY#SAM1015#5&2c7ec5a2&0&UID4353#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}";
        let target = format!("display:{id}");
        let url = config_window_url(Some(&target));

        assert!(!url.contains('#'), "a raw `#` would cut the id off: {url}");
        assert_eq!(target_param_of(&url).as_deref(), Some(target.as_str()));
    }

    #[test]
    fn mi01_the_plain_targets_round_trip_too() {
        for target in ["app:general", "add", "widget:clock-1", "view:default"] {
            let url = config_window_url(Some(target));
            assert_eq!(target_param_of(&url).as_deref(), Some(target), "{target}");
        }
    }

    #[test]
    fn mi01_no_target_means_no_target_param() {
        assert_eq!(config_window_url(None), "index.html?config=1");
        assert_eq!(config_window_url(Some("")), "index.html?config=1");
    }

    // -----------------------------------------------------------------
    // #79 / [AS-05]: a failed autostart toggle must say WHAT to do
    // -----------------------------------------------------------------
    //
    // Two users hit the same wall: the "Launch on startup" toggle refused to
    // stay on and the app said nothing actionable, so it read as a dead
    // control. Both were fixed by relaunching Themia as administrator.
    // `classify_autostart_failure` is what turns a raw schtasks failure into
    // that advice — and, crucially, withholds it when the user is ALREADY
    // elevated, where elevating again cannot help.

    #[test]
    fn s79_non_elevated_failure_asks_for_administrator() {
        let e = classify_autostart_failure(
            "schtasks /Create failed (exit code 1): ERROR: Access is denied.".to_string(),
            false,
        );
        assert_eq!(e.kind, AutostartErrorKind::ElevationRequired);
        // The raw diagnostic survives classification, for logs / bug reports.
        assert!(e.message.contains("Access is denied"), "message lost: {}", e.message);
    }

    #[test]
    fn s79_elevated_failure_does_not_ask_for_administrator() {
        // Already admin: the cause is policy or a disabled scheduler service.
        // Repeating "run as administrator" would loop the user forever.
        let e = classify_autostart_failure(
            "schtasks /Create failed (exit code 1): ERROR: Access is denied.".to_string(),
            true,
        );
        assert_eq!(e.kind, AutostartErrorKind::Other);
        assert!(e.message.contains("Access is denied"));
    }

    #[test]
    fn s79_classification_ignores_localized_stderr_text() {
        // Same failure, German Windows. The decision comes from the process
        // token, never from matching English words in schtasks' output.
        let de = "schtasks /Create failed (exit code 1): FEHLER: Zugriff verweigert.".to_string();
        assert_eq!(
            classify_autostart_failure(de.clone(), false).kind,
            AutostartErrorKind::ElevationRequired
        );
        assert_eq!(classify_autostart_failure(de, true).kind, AutostartErrorKind::Other);
    }

    #[test]
    fn s79_error_serializes_as_camel_case_kind_plus_message() {
        // The frontend parses `kind` against a closed union; the wire spelling
        // is part of that contract (see `autostartError.ts`).
        let json = serde_json::to_string(&classify_autostart_failure("boom".to_string(), false))
            .expect("AutostartError must serialize");
        assert!(json.contains(r#""kind":"elevationRequired""#), "got: {json}");
        assert!(json.contains(r#""message":"boom""#), "got: {json}");
        let json = serde_json::to_string(&classify_autostart_failure("boom".to_string(), true))
            .expect("AutostartError must serialize");
        assert!(json.contains(r#""kind":"other""#), "got: {json}");
    }

    // -----------------------------------------------------------------
    // [HK-01]: the keyboard callback must not touch Tauri
    // -----------------------------------------------------------------
    //
    // `kb_hook_proc_impl` used to call `app.webview_windows()` and
    // `Emitter::emit` inline — the exact hazard [HK-01] was written for, fixed
    // in the mouse path and left in the keyboard one. Both take the manager
    // lock, which the main thread holds across window creation and teardown, so
    // a Ctrl+Z or a routed Ctrl+C arriving during a display-change storm could
    // block the callback past `LowLevelHooksTimeout` and cost the app both
    // hooks. Delivery now goes through `request_hook_emit`.
    //
    // A source scan, because the property is "this code is not here" and no
    // runtime test can pin that: the callback needs a live hook, a live Tauri
    // app and a contended main thread to misbehave.

    #[cfg(target_os = "windows")]
    fn kb_hook_body_without_comments() -> String {
        let src = include_str!("lib.rs");
        let start = src
            .find("unsafe fn kb_hook_proc_impl")
            .expect("kb_hook_proc_impl must exist");
        let end = start
            + src[start..]
                .find("fn mouse_hook_proc")
                .expect("mouse_hook_proc must follow it");
        // Comments name the forbidden calls in order to forbid them.
        src[start..end]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn hk01_keyboard_callback_takes_no_manager_lock() {
        let body = kb_hook_body_without_comments();
        for needle in [
            "WATCHER_APP",
            "app.emit",
            "emit_to",
            "webview_windows()",
            "get_webview_window",
        ] {
            assert!(
                !body.contains(needle),
                "[HK-01] `{needle}` is back in the WH_KEYBOARD_LL callback. It takes Tauri's \
                 manager lock, which the main thread holds across window creation and teardown; \
                 one keystroke landing there overruns LowLevelHooksTimeout and Windows silently \
                 removes both input hooks. Hand the delivery to request_hook_emit instead."
            );
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn hk01_emit_queue_is_bounded() {
        // A wedged emitter must cost bounded memory, not a growing backlog of
        // every key the user has pressed since.
        hook_emits().lock().unwrap_or_else(|e| e.into_inner()).clear();
        for i in 0..(HOOK_EMIT_CAP + 8) {
            request_hook_emit(HookEmit::AppKeyBroadcast(format!("{i}")));
        }
        let queued = hook_emits().lock().unwrap_or_else(|e| e.into_inner()).len();
        assert_eq!(queued, HOOK_EMIT_CAP, "queue grew past its cap");
        hook_emits().lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    // -----------------------------------------------------------------
    // [HK-02] / [HK-03]: a dead hook must not become system-wide input lag
    // -----------------------------------------------------------------
    //
    // The watchdog used to call `install_input_hooks` straight from the z-order
    // poll thread, which does nothing but sleep. Windows delivers a low-level
    // hook callback through the message queue of the thread that installed it,
    // so those registrations could never fire: the heartbeat stayed flat, the
    // check re-armed every four seconds forever, and each fresh registration
    // made the OS wait out `LowLevelHooksTimeout` on every keystroke and mouse
    // move in the whole session. The log filled with [HK-02] on repeat while
    // the desktop's input went sluggish. Installing now happens only on the
    // hook thread ([HK-03]), and a re-arm that does not take backs off.

    #[test]
    fn hk02_rearm_backs_off_instead_of_retrying_every_check() {
        // First failure retries at the normal cadence; each further one waits
        // twice as long, so a registration that cannot be serviced costs the
        // user's input latency once a minute, not once every four seconds.
        assert_eq!(rearm_backoff_checks(0), 1);
        assert_eq!(rearm_backoff_checks(1), 2);
        assert_eq!(rearm_backoff_checks(2), 4);
        assert_eq!(rearm_backoff_checks(3), 8);
        assert_eq!(rearm_backoff_checks(4), 16);
        // Capped: no shift overflow, and no hour-long silence either.
        assert_eq!(rearm_backoff_checks(5), 16);
        assert_eq!(rearm_backoff_checks(u32::MAX), 16);
    }

    #[test]
    fn hk02_dead_only_when_the_os_saw_input_our_callbacks_missed() {
        // Input inside the interval we watched, no callback ran: dead.
        assert!(hooks_look_dead(true, false, Some(10), 4_000));
        // A callback ran: alive, whatever the idle time says.
        assert!(!hooks_look_dead(true, true, Some(10), 4_000));
        // User was away from the machine; silence proves nothing.
        assert!(!hooks_look_dead(true, false, Some(60_000), 4_000));
        // Never armed in the first place.
        assert!(!hooks_look_dead(false, false, Some(10), 4_000));
        // No GetLastInputInfo reading = no decision; never re-arm on a guess.
        assert!(!hooks_look_dead(true, false, None, 4_000));
    }

    // -----------------------------------------------------------------
    // decide_refuse_activation: "overlay jumps in front on right-click"
    // -----------------------------------------------------------------

    /// Idle state (no focused input, not in edit mode): a click on a widget
    /// (including a right-click that opens the context menu) must NOT
    /// activate the overlay. Reproduces the user's report: "on right click
    /// the window still goes into the front of other windows."
    #[test]
    fn refuse_activation_when_idle() {
        assert!(
            decide_refuse_activation(false),
            "idle clicks must refuse activation (return MA_NOACTIVATE)"
        );
    }

    /// Interactive state (focused text input / edit mode / picker open):
    /// the user genuinely needs keyboard focus, so activation must proceed.
    #[test]
    fn allow_activation_when_interactive() {
        assert!(
            !decide_refuse_activation(true),
            "interactive clicks must allow activation"
        );
    }

    // -----------------------------------------------------------------
    // session_change_is_unlock: replay the startup animation on unlock
    // -----------------------------------------------------------------

    /// WM_WTSSESSION_CHANGE fires for many session transitions; only an
    /// UNLOCK (the user came back to their desktop) replays the startup
    /// animation. WTS_SESSION_LOCK / LOGON / LOGOFF / remote connect /
    /// disconnect must NOT trigger it.
    #[test]
    fn session_unlock_triggers_replay_others_do_not() {
        const WTS_CONSOLE_CONNECT: usize = 0x1;
        const WTS_CONSOLE_DISCONNECT: usize = 0x2;
        const WTS_REMOTE_CONNECT: usize = 0x3;
        const WTS_REMOTE_DISCONNECT: usize = 0x4;
        const WTS_SESSION_LOGON: usize = 0x5;
        const WTS_SESSION_LOGOFF: usize = 0x6;
        const WTS_SESSION_LOCK: usize = 0x7;
        const WTS_SESSION_UNLOCK: usize = 0x8;

        assert!(session_change_is_unlock(WTS_SESSION_UNLOCK), "unlock must replay");
        for wparam in [
            WTS_CONSOLE_CONNECT,
            WTS_CONSOLE_DISCONNECT,
            WTS_REMOTE_CONNECT,
            WTS_REMOTE_DISCONNECT,
            WTS_SESSION_LOGON,
            WTS_SESSION_LOGOFF,
            WTS_SESSION_LOCK,
        ] {
            assert!(!session_change_is_unlock(wparam), "wparam {wparam:#x} must not replay");
        }
    }

    /// Screen-time accrual pauses exactly on the LOCK transition (screentime
    /// SPEC [S-23]); every other session event must not pause it.
    #[test]
    fn s23_session_lock_pauses_screentime_others_do_not() {
        const WTS_SESSION_LOCK: usize = 0x7;
        assert!(session_change_is_lock(WTS_SESSION_LOCK), "lock must pause");
        for wparam in [0x1usize, 0x2, 0x3, 0x4, 0x5, 0x6, 0x8] {
            assert!(!session_change_is_lock(wparam), "wparam {wparam:#x} must not pause");
        }
    }

    // -----------------------------------------------------------------
    // Display debounce sequencing [DW-02]/[DW-03]: "overlay overlaps the
    // taskbar until restart" heal pipeline (see src-tauri/SPEC.md).
    // -----------------------------------------------------------------

    /// [DW-03] The debounce decision is pure sequence equality on a
    /// monotonic counter. The previous implementation compared wall-clock
    /// millis: after a backwards clock step (NTP correction), fresh events
    /// were stamped BEFORE the last-handled time, `last > handled` stayed
    /// false, and every later display change was silently dropped; the
    /// overlay kept taskbar-overlapping bounds until an app restart.
    #[test]
    fn dw03_display_pass_due_is_pure_sequence_comparison() {
        assert!(!display_pass_due(0, 0), "no events yet, idle");
        assert!(display_pass_due(1, 0), "one unhandled event must fire a pass");
        assert!(!display_pass_due(5, 5), "fully handled, idle");
        // Inequality in EITHER direction reports due. A "handled ahead of
        // seq" state cannot occur with a monotonic counter; this pins that
        // no desync can ever silently strand events (the wall-clock bug).
        assert!(display_pass_due(3, 7), "desync must trigger a pass, never a stall");
    }

    /// [DW-02] The debounce quiet-window only closes when no new event
    /// arrived while the thread slept (sequence unchanged vs the snapshot).
    #[test]
    fn dw02_display_events_settled_requires_unchanged_seq() {
        assert!(display_events_settled(4, 4), "unchanged seq, burst settled");
        assert!(!display_events_settled(4, 6), "burst still in progress");
    }

    // -----------------------------------------------------------------
    // Work-area self-heal sweep [DW-01]/[DW-04]
    // -----------------------------------------------------------------

    /// [DW-01] The overlay rect must equal its monitor's work area exactly
    /// (physical px). Reproduces the report: a stale full-monitor pin whose
    /// bottom edge spans the taskbar band must register as a mismatch.
    #[test]
    fn dw01_overlay_rect_must_equal_work_area_exactly() {
        // Correct pin: 2560×1392 work area (48px taskbar at the bottom).
        assert!(overlay_rect_matches_work_area((0, 0, 2560, 1392), 0, 0, 2560, 1392));
        // The reported bug: pinned to the full 2560×1440 monitor while the
        // work area excludes the taskbar; overlaps it by 48px.
        assert!(!overlay_rect_matches_work_area((0, 0, 2560, 1440), 0, 0, 2560, 1392));
        // Secondary monitor at a negative origin pins correctly.
        assert!(overlay_rect_matches_work_area((-1920, 0, 0, 1040), -1920, 0, 1920, 1040));
        // Origin drift (taskbar docked LEFT moves the work-area origin).
        assert!(!overlay_rect_matches_work_area((0, 0, 2496, 1440), 64, 0, 2496, 1440));
    }

    // -----------------------------------------------------------------
    // Overlay backdrop denial [BD-01]/[BD-02]/[BD-03]
    // -----------------------------------------------------------------

    /// [BD-01] The denial table must hold the OFF values. `DWMSBT_NONE` is 1;
    /// 0 is `DWMSBT_AUTO`, which hands the choice back to DWM (and to anyone
    /// pushing a backdrop at us), so a 1→0 slip here would silently restore
    /// the exact wallpaper-blur symptom from themia-releases#74 with every
    /// other test still green.
    #[test]
    fn bd01_backdrop_denial_table_is_all_off_values() {
        let attrs = backdrop_denial_attrs();
        assert_eq!(
            attrs,
            [(38, 1), (1029, 0), (33, 1)],
            "DWMWA_SYSTEMBACKDROP_TYPE=DWMSBT_NONE, DWMWA_MICA=off, \
             DWMWA_WINDOW_CORNER_PREFERENCE=DWMWCP_DONOTROUND"
        );
        for &(attr, value) in &attrs {
            // DWMWA_MICA is a bool where 0 IS the denial; every other entry is
            // an enum whose 0 means "DWM decides".
            if attr != 1029 {
                assert_ne!(
                    value, 0,
                    "attr {attr}: 0 is the 'let DWM decide' default, not a denial"
                );
            }
        }
    }

    /// [BD-02] `pin_to_desktop` must deny the backdrop on every overlay it
    /// pins. A source scan: the effect is a DWM attribute on a live HWND,
    /// observable only with a compositor and a real window.
    #[test]
    fn bd02_pin_to_desktop_denies_backdrop() {
        let src = include_str!("lib.rs");
        let fn_start = src
            .find("fn pin_to_desktop(")
            .expect("pin_to_desktop must exist");
        let fn_end = src[fn_start..]
            .find("\n}\n")
            .map(|i| fn_start + i)
            .expect("pin_to_desktop must terminate");
        assert!(
            src[fn_start..fn_end].contains("deny_dwm_backdrop(hwnd)"),
            "pin_to_desktop must call deny_dwm_backdrop, otherwise a freshly \
             built overlay can carry an injected Mica/Acrylic backdrop"
        );
    }

    /// [BD-03] The overlay WndProc must re-assert the denial on
    /// `WM_DWMCOMPOSITIONCHANGED` (0x031E) — a DWM restart resets every window
    /// attribute. Source scan for the same reason as [BD-02], and it also
    /// pins "event-driven, not polled".
    #[test]
    fn bd03_wndproc_reasserts_denial_on_composition_change() {
        let src = include_str!("lib.rs");
        let fn_start = src
            .find("fn no_minimize_wnd_proc_impl(")
            .expect("no_minimize_wnd_proc_impl must exist");
        let body = &src[fn_start..];
        let hook = body
            .find("if msg == WM_DWMCOMPOSITIONCHANGED {")
            .expect("WndProc must handle WM_DWMCOMPOSITIONCHANGED");
        assert!(
            body[hook..hook + 200].contains("deny_dwm_backdrop(hwnd)"),
            "the WM_DWMCOMPOSITIONCHANGED branch must re-assert the denial"
        );
        assert!(
            body[..hook].contains("const WM_DWMCOMPOSITIONCHANGED: u32 = 0x031E;"),
            "WM_DWMCOMPOSITIONCHANGED must be 0x031E"
        );
    }

    /// [BD-04] The denial must RE-ASSERT tao's empty-region blur-behind, never
    /// disable blur-behind. tao implements `transparent: true` with exactly that
    /// call, so `fEnable = FALSE` here turns the overlay opaque — invisible
    /// while the window is region-clipped to the widget shapes, and a black
    /// screen the moment the region covers the whole window (context menu, edit
    /// mode, modal). Source scan: the effect is a DWM attribute on a live HWND.
    #[test]
    fn bd04_denial_reasserts_transparency_blur_behind() {
        let src = include_str!("lib.rs");
        let fn_start = src
            .find("unsafe fn deny_dwm_backdrop(")
            .expect("deny_dwm_backdrop must exist");
        let fn_end = src[fn_start..]
            .find("\n}\n")
            .map(|i| fn_start + i)
            .expect("deny_dwm_backdrop must terminate");
        let body = &src[fn_start..fn_end];
        assert!(
            !body.contains("fEnable: FALSE"),
            "deny_dwm_backdrop must not disable blur-behind: that is tao's \
             per-pixel-alpha mechanism for `transparent: true`, and turning it \
             off makes the whole overlay opaque"
        );
        assert!(
            body.contains("DWM_BB_ENABLE | DWM_BB_BLURREGION")
                && body.contains("fEnable: TRUE")
                && body.contains("CreateRectRgn(0, 0, -1, -1)"),
            "the blur-behind re-assert must be tao's exact empty-region call: \
             DWM_BB_ENABLE | DWM_BB_BLURREGION, fEnable TRUE, an EMPTY region"
        );
        assert!(
            body.contains("DeleteObject(region"),
            "the HRGN handed to DwmEnableBlurBehindWindow is copied by DWM and \
             must be freed on every path"
        );
    }

    /// [WA-01] The overlay builder must pass `.focused(false)`. Regression
    /// guard for the invisible foreground steal: without it, wry's
    /// `MoveFocus(PROGRAMMATIC)` during WebView2 controller creation activated
    /// the (still hidden, not yet `WS_EX_NOACTIVATE`) overlay, so every launch
    /// pulled the foreground off the user's app once per monitor.
    ///
    /// A source scan, because the property lives in a builder chain whose
    /// configuration Tauri exposes no way to read back, and the runtime
    /// behavior needs a live desktop + an `EVENT_SYSTEM_FOREGROUND` hook to
    /// observe. Scoped to `build_widget_window`'s chain: the detached windows
    /// (config, connect, widget popouts) are user-invoked and SHOULD focus,
    /// except under the [WA-02] harness flag.
    #[test]
    fn wa01_overlay_builder_is_created_unfocused() {
        let src = include_str!("lib.rs");
        let fn_start = src
            .find("fn build_widget_window(")
            .expect("build_widget_window must exist");
        let chain_start = src[fn_start..]
            .find("WebviewWindowBuilder::new(")
            .map(|i| fn_start + i)
            .expect("build_widget_window must build a webview window");
        let chain_end = src[chain_start..]
            .find(".build()?;")
            .map(|i| chain_start + i)
            .expect("builder chain must terminate in .build()?;");
        let chain = &src[chain_start..chain_end];
        assert!(
            chain.contains(".focused(false)"),
            "[WA-01] overlay builder must pass .focused(false); \
             .visible(false) alone still lets wry's MoveFocus activate the window"
        );
    }

    /// [WA-02] The harness flag is matched whole, positionally free, and never
    /// by prefix: `--no-activate-something` or a bare `--no` must not switch
    /// the app into a non-activating mode a user never asked for.
    #[test]
    fn wa02_no_activate_flag_is_an_exact_argv_match() {
        assert!(args_request_no_activate(["themia.exe", "--autostart", "--no-activate"]));
        assert!(args_request_no_activate(["themia.exe", "--no-activate", "--autostart"]));
        assert!(!args_request_no_activate(["themia.exe", "--autostart"]));
        assert!(!args_request_no_activate(["themia.exe"]));
        assert!(!args_request_no_activate(["themia.exe", "--no-activate-widgets"]));
        assert!(!args_request_no_activate(["themia.exe", "no-activate"]));
    }

    /// [WA-03] A same-DPI monitor centers exactly, and the primary's origin
    /// contributes nothing beyond its own offset.
    #[test]
    fn wa03_centers_on_a_100_percent_monitor() {
        // 2560x1440 primary at the origin, 960x720 logical window.
        let (x, y) = detached_window_logical_position(0, 0, 2560, 1440, 1.0, 960.0, 720.0);
        assert_eq!((x, y), (800.0, 360.0));
    }

    /// [WA-03] The reporter's shape (themia-releases#111): a SECOND display,
    /// left of the primary, so its physical origin is negative. The window must
    /// be created there, not on the primary and then dragged across.
    #[test]
    fn wa03_centers_on_a_monitor_left_of_the_primary() {
        let (x, y) = detached_window_logical_position(-2560, 0, 2560, 1440, 1.0, 960.0, 720.0);
        assert_eq!((x, y), (-1760.0, 360.0));
        // Converted back with that monitor's own scale, the point is inside the
        // monitor's physical rect, which is exactly the containment test tao
        // runs to pick the creation monitor.
        assert!((-2560.0..0.0).contains(&x));
    }

    /// [WA-03] Mixed DPI is the case the old build-then-move path got wrong: it
    /// sized the move with the PARENT window's scale factor and then dragged a
    /// live WebView2 across a DPI boundary. The logical position must round-trip
    /// through the TARGET monitor's scale back onto the target monitor.
    #[test]
    fn wa03_mixed_dpi_position_round_trips_onto_the_target_monitor() {
        // Secondary at 150%, right of a 2560-wide primary: 3840x2160 physical
        // = 2560x1440 logical.
        let scale = 1.5;
        let (x, y) = detached_window_logical_position(2560, 0, 3840, 2160, scale, 960.0, 720.0);
        let (px, py) = ((x * scale).round() as i32, (y * scale).round() as i32);
        assert!(
            (2560..2560 + 3840).contains(&px),
            "physical x {px} must land inside the target monitor"
        );
        assert!((0..2160).contains(&py), "physical y {py} must land inside the target monitor");
        // Centered: equal physical margins either side of the 960*1.5 window.
        assert_eq!(px, 2560 + (3840 - 1440) / 2);
        assert_eq!(py, (2160 - 1080) / 2);
    }

    /// [WA-03] A scale factor that crossed FFI as garbage must degrade to 1.0,
    /// never hand the builder NaN/inf coordinates.
    #[test]
    fn wa03_degenerate_scale_factors_never_produce_nan() {
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let (x, y) = detached_window_logical_position(0, 0, 2560, 1440, bad, 960.0, 720.0);
            assert!(x.is_finite() && y.is_finite(), "scale {bad} produced ({x},{y})");
            assert_eq!((x, y), (800.0, 360.0), "scale {bad} must fall back to 1.0");
        }
    }

    /// [WA-03] Regression guard for themia-releases#111: the detached-window
    /// builders must take their position as a BUILDER INPUT and must not move
    /// the finished window onto another monitor. A source scan, because the
    /// difference is only observable on a live multi-monitor, mixed-DPI desktop
    /// — the very configuration CI does not have.
    #[test]
    fn wa03_detached_builders_place_at_creation_not_after() {
        let src = include_str!("lib.rs");
        for f in ["fn build_detached_window(", "async fn open_web_element_picker_window("] {
            let fn_start = src.find(f).unwrap_or_else(|| panic!("{f} must exist"));
            let fn_end = src[fn_start..]
                .find("\n}\n")
                .map(|i| fn_start + i)
                .unwrap_or_else(|| panic!("{f} must terminate"));
            let body = &src[fn_start..fn_end];
            assert!(
                body.contains("detached_window_placement("),
                "[WA-03] {f} must resolve its placement before building"
            );
            assert!(
                body.contains(".position(x, y)"),
                "[WA-03] {f} must hand the position to the builder"
            );
            assert!(
                !body.contains("set_position("),
                "[WA-03] {f} must not move the window after it is built: that drags a \
                 live WebView2 across a per-monitor DPI boundary"
            );
        }
    }

    /// [DW-04] Divergence must hold for two consecutive sweeps before the
    /// heal fires (one sweep can land mid-transition while the taskbar is
    /// still animating/docking); any clean sweep resets the streak, and a
    /// fire resets it too so healing re-arms from zero.
    #[test]
    fn dw04_mismatch_streak_fires_after_two_consecutive_sweeps() {
        assert_eq!(mismatch_streak_advance(0, true), (1, false), "first sweep arms only");
        assert_eq!(mismatch_streak_advance(1, true), (0, true), "second consecutive fires + resets");
        assert_eq!(mismatch_streak_advance(1, false), (0, false), "clean sweep disarms");
        assert_eq!(mismatch_streak_advance(0, false), (0, false), "idle stays idle");
    }

    // -----------------------------------------------------------------
    // Per-window interactive state: multi-monitor isolation
    // -----------------------------------------------------------------

    /// Each overlay window's interactive flag is independent. Reproduces the
    /// multi-monitor bug: marking monitor-0's window interactive must NOT
    /// make monitor-1's window interactive, otherwise monitor-1 would skip
    /// region updates and stop refusing activation (flicker + front-jump on
    /// the secondary display).
    #[cfg(target_os = "windows")]
    #[test]
    fn interactive_state_is_per_window() {
        // Two distinct fake HWND values. We never dereference them; the
        // interactive set is keyed purely by the usize handle value.
        let monitor_0: usize = 0x1111_0000;
        let monitor_1: usize = 0x2222_0000;

        // Clean slate (other tests may have touched the set).
        set_window_interactive(monitor_0, false);
        set_window_interactive(monitor_1, false);
        assert!(!is_window_interactive(monitor_0));
        assert!(!is_window_interactive(monitor_1));

        // Monitor 0 goes interactive (edit mode / focused input).
        set_window_interactive(monitor_0, true);
        assert!(is_window_interactive(monitor_0), "monitor-0 must be interactive");
        assert!(
            !is_window_interactive(monitor_1),
            "monitor-1 must stay non-interactive, no global leak"
        );

        // Monitor 0 goes idle again; monitor 1 still untouched.
        set_window_interactive(monitor_0, false);
        assert!(!is_window_interactive(monitor_0));
        assert!(!is_window_interactive(monitor_1));

        // Cleanup so we don't leak test handles into the shared set.
        set_window_interactive(monitor_0, false);
        set_window_interactive(monitor_1, false);
    }

    // -----------------------------------------------------------------
    // Per-window show-once guard: no grey-titlebar flash at startup
    // -----------------------------------------------------------------

    /// The overlay is shown exactly once per HWND, the first time it is
    /// region-clipped (or by the fallback timer). Reproduces the startup
    /// bug's fix: a second show attempt must be a no-op, the guard is
    /// per-window (multi-monitor), and a recycled HWND, whose entry the
    /// `Destroyed` handler removed, must be allowed to show again.
    #[cfg(target_os = "windows")]
    #[test]
    fn show_overlay_guard_fires_once_per_window() {
        let monitor_0: usize = 0x3333_0000;
        let monitor_1: usize = 0x4444_0000;

        // Clean slate (other tests may have touched the set).
        shown_windows().lock().unwrap().remove(&monitor_0);
        shown_windows().lock().unwrap().remove(&monitor_1);

        assert!(mark_shown_once(monitor_0), "first call must report show");
        assert!(!mark_shown_once(monitor_0), "second call must be a no-op");

        // Independent per-HWND: monitor-1 still shows once of its own.
        assert!(mark_shown_once(monitor_1), "different window still shows once");
        assert!(!mark_shown_once(monitor_0), "monitor-0 still latched");

        // Destroyed handler removes the entry → recycled HWND shows again.
        shown_windows().lock().unwrap().remove(&monitor_0);
        assert!(mark_shown_once(monitor_0), "recycled HWND must show once more");

        // Cleanup so we don't leak test handles into the shared set.
        shown_windows().lock().unwrap().remove(&monitor_0);
        shown_windows().lock().unwrap().remove(&monitor_1);
    }

    // -----------------------------------------------------------------
    // build_widget_region: pure logic via Win32 GDI handles
    // -----------------------------------------------------------------

    /// Smallest enclosing box of an HRGN, in client coords. Returns
    /// (left, top, right, bottom).
    #[cfg(target_os = "windows")]
    unsafe fn rgn_box(rgn: winapi::shared::minwindef::HRGN) -> (i32, i32, i32, i32) {
        use winapi::shared::windef::RECT;
        use winapi::um::wingdi::GetRgnBox;
        let mut r: RECT = std::mem::zeroed();
        GetRgnBox(rgn, &mut r);
        (r.left, r.top, r.right, r.bottom)
    }

    #[cfg(target_os = "windows")]
    unsafe fn rgn_contains(rgn: winapi::shared::minwindef::HRGN, x: i32, y: i32) -> bool {
        use winapi::um::wingdi::PtInRegion;
        PtInRegion(rgn, x, y) != 0
    }

    #[cfg(target_os = "windows")]
    unsafe fn delete_rgn(rgn: winapi::shared::minwindef::HRGN) {
        use winapi::um::wingdi::DeleteObject;
        DeleteObject(rgn as *mut _);
    }

    /// Empty input must yield a null HRGN. Caller passes null to
    /// SetWindowRgn to clear any existing clip: that's the "interactive
    /// mode / no widgets pushed yet" state.
    #[cfg(target_os = "windows")]
    #[test]
    fn region_empty_input_yields_null() {
        unsafe {
            let rgn = build_widget_region(&[]);
            assert!(rgn.is_null(), "empty input must yield null HRGN");
        }
    }

    /// Single rect → region whose bounding box exactly matches the rect.
    #[cfg(target_os = "windows")]
    #[test]
    fn region_single_rect_box_matches() {
        unsafe {
            let rects = [sr(10, 20, 110, 220, 0)];
            let rgn = build_widget_region(&rects);
            assert!(!rgn.is_null());
            let (l, t, r, b) = rgn_box(rgn);
            assert_eq!((l, t, r, b), (10, 20, 110, 220));
            delete_rgn(rgn);
        }
    }

    /// Two disjoint widgets: both belong to the region, and the gap
    /// between them does NOT. Verifies the user's question literally:
    /// "the region can be multiple squares unified".
    #[cfg(target_os = "windows")]
    #[test]
    fn region_disjoint_rects_are_both_inside() {
        unsafe {
            // (0,0)-(100,100) top-left widget; (500,500)-(600,600) bottom-right.
            let rects = [sr(0, 0, 100, 100, 0), sr(500, 500, 600, 600, 0)];
            let rgn = build_widget_region(&rects);
            assert!(!rgn.is_null());

            assert!(rgn_contains(rgn, 50, 50),    "top-left widget must be inside region");
            assert!(rgn_contains(rgn, 550, 550),  "bottom-right widget must be inside region");
            assert!(!rgn_contains(rgn, 300, 300), "gap between widgets must NOT be inside region");
            assert!(!rgn_contains(rgn, 200, 200), "above-gap must NOT be inside region");

            // Bounding box of the union covers (0, 0) - (600, 600).
            let (l, t, r, b) = rgn_box(rgn);
            assert_eq!((l, t, r, b), (0, 0, 600, 600));
            delete_rgn(rgn);
        }
    }

    /// Three widgets in an L-shape, each independently inside.
    #[cfg(target_os = "windows")]
    #[test]
    fn region_three_widgets_each_inside() {
        unsafe {
            let rects = [
                sr(0,   0,   100, 100, 0),
                sr(200, 0,   300, 100, 0),
                sr(0,   200, 100, 300, 0),
            ];
            let rgn = build_widget_region(&rects);
            assert!(!rgn.is_null());
            assert!(rgn_contains(rgn, 50, 50));
            assert!(rgn_contains(rgn, 250, 50));
            assert!(rgn_contains(rgn, 50, 250));
            // Center of the empty "L corner", not in region.
            assert!(!rgn_contains(rgn, 250, 250));
            delete_rgn(rgn);
        }
    }

    /// Rounded-corner widget: center inside, outer corner (well outside
    /// the visible curve) NOT inside.
    #[cfg(target_os = "windows")]
    #[test]
    fn region_rounded_rect_excludes_outer_corner() {
        unsafe {
            let rects = [sr(0, 0, 100, 100, 20)];
            let rgn = build_widget_region(&rects);
            assert!(!rgn.is_null());
            assert!(rgn_contains(rgn, 50, 50),  "center of rounded widget must be inside");
            assert!(!rgn_contains(rgn, 0, 0),   "outer corner of rounded widget must NOT be inside");
            delete_rgn(rgn);
        }
    }

    /// Degenerate (zero-area) rects must be skipped; the surviving valid
    /// rect determines the region.
    #[cfg(target_os = "windows")]
    #[test]
    fn region_skips_degenerate_rects() {
        unsafe {
            let rects = [
                sr(10, 10, 10, 10, 0),    // zero width AND height
                sr(20, 20, 20, 100, 0),   // zero width
                sr(50, 50, 150, 150, 0),  // valid
            ];
            let rgn = build_widget_region(&rects);
            assert!(!rgn.is_null());
            let (l, t, r, b) = rgn_box(rgn);
            assert_eq!((l, t, r, b), (50, 50, 150, 150));
            delete_rgn(rgn);
        }
    }

    // -----------------------------------------------------------------
    // Win32 integration: apply region to a real window, read it back.
    // -----------------------------------------------------------------

    use std::sync::Mutex;
    static WIN32_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn should_skip_win32() -> bool {
        std::env::var("THEMIA_SKIP_WIN32").is_ok()
    }

    #[cfg(target_os = "windows")]
    fn make_test_window(x: i32, y: i32, w: i32, h: i32) -> winapi::shared::windef::HWND {
        use std::ptr;
        use winapi::shared::windef::HWND;
        use winapi::um::libloaderapi::GetModuleHandleW;
        use winapi::um::winuser::{
            CreateWindowExW, RegisterClassW, WNDCLASSW, WS_EX_NOACTIVATE,
            WS_EX_TOOLWINDOW, WS_POPUP,
        };
        unsafe {
            let class: Vec<u16> = "ThemiaRegionTest\0".encode_utf16().collect();
            let hinst = GetModuleHandleW(ptr::null());
            let mut wc: WNDCLASSW = std::mem::zeroed();
            wc.lpfnWndProc = Some(winapi::um::winuser::DefWindowProcW);
            wc.hInstance = hinst;
            wc.lpszClassName = class.as_ptr();
            RegisterClassW(&wc);
            let title: Vec<u16> = "ThemiaRgnTest\0".encode_utf16().collect();
            CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                class.as_ptr(),
                title.as_ptr(),
                WS_POPUP,
                x, y, w, h,
                ptr::null_mut(),
                ptr::null_mut(),
                hinst,
                ptr::null_mut(),
            ) as HWND
        }
    }

    #[cfg(target_os = "windows")]
    fn destroy(hwnd: winapi::shared::windef::HWND) {
        use winapi::um::winuser::DestroyWindow;
        unsafe { DestroyWindow(hwnd); }
    }

    /// End-to-end: push rects → apply region → window's actual region box
    /// matches the union of inputs. Mirrors the production flow.
    #[cfg(target_os = "windows")]
    #[test]
    fn integration_apply_region_to_real_window() {
        if should_skip_win32() { return; }
        let _g = WIN32_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        use winapi::shared::windef::RECT;
        use winapi::um::winuser::GetWindowRgnBox;

        let hwnd = make_test_window(0, 0, 1000, 1000);
        assert!(!hwnd.is_null());
        let key = hwnd as usize;

        widget_rects_map().lock().unwrap().insert(key, vec![
            sr(10,  10,  210, 110,  8),  // widget A
            sr(400, 300, 600, 500,  8),  // widget B
            sr(900, 950, 980, 990,  0),  // mode pill (sharp rect)
        ]);
        unsafe { apply_widget_region(hwnd, true); }

        let mut box_rect: RECT = unsafe { std::mem::zeroed() };
        let kind = unsafe { GetWindowRgnBox(hwnd, &mut box_rect) };
        widget_rects_map().lock().unwrap().remove(&key);
        destroy(hwnd);

        assert!(kind != 0, "GetWindowRgnBox must succeed (kind != NULLREGION/ERROR)");
        assert!(
            box_rect.left <= 10 && box_rect.top <= 10
                && box_rect.right >= 980 && box_rect.bottom >= 990,
            "region box ({},{},{},{}) must enclose union of pushed rects",
            box_rect.left, box_rect.top, box_rect.right, box_rect.bottom
        );
    }

    /// Clearing the region (interactive=true path) removes the clip: the
    /// window again accepts input across its full bounds.
    #[cfg(target_os = "windows")]
    #[test]
    fn integration_clear_region_removes_clip() {
        if should_skip_win32() { return; }
        let _g = WIN32_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        use winapi::shared::windef::RECT;
        use winapi::um::winuser::GetWindowRgnBox;

        let hwnd = make_test_window(0, 0, 800, 600);
        assert!(!hwnd.is_null());
        let key = hwnd as usize;
        widget_rects_map().lock().unwrap().insert(key, vec![sr(100, 100, 300, 300, 0)]);

        unsafe { apply_widget_region(hwnd, true); }
        unsafe { apply_widget_region(hwnd, false); }

        let mut box_rect: RECT = unsafe { std::mem::zeroed() };
        let kind = unsafe { GetWindowRgnBox(hwnd, &mut box_rect) };
        widget_rects_map().lock().unwrap().remove(&key);
        destroy(hwnd);

        // After clearing, GetWindowRgnBox returns ERROR (= 0) because there
        // is no region installed on the window.
        assert_eq!(kind, 0, "after clear, window must have no region installed");
    }

    /// [S-18] Regression (nwalser/themia-releases#34): removing the last
    /// widget pushes an EXPLICITLY EMPTY rect list. The overlay must then
    /// get an EMPTY region (zero hit-test area), NOT a cleared clip, which
    /// turns the whole transparent overlay into a hit-test target and
    /// swallows desktop right-clicks until Show Widgets is toggled off.
    #[cfg(target_os = "windows")]
    #[test]
    fn s18_explicit_empty_rects_install_empty_region() {
        if should_skip_win32() { return; }
        let _g = WIN32_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        use winapi::shared::windef::RECT;
        use winapi::um::winuser::GetWindowRgnBox;

        let hwnd = make_test_window(0, 0, 800, 600);
        assert!(!hwnd.is_null());
        let key = hwnd as usize;
        // The frontend pushed an explicit zero-rect list (zero-widget view,
        // no visible chrome).
        widget_rects_map().lock().unwrap().insert(key, Vec::new());

        unsafe { apply_widget_region(hwnd, true); }

        let mut box_rect: RECT = unsafe { std::mem::zeroed() };
        let kind = unsafe { GetWindowRgnBox(hwnd, &mut box_rect) };
        widget_rects_map().lock().unwrap().remove(&key);
        destroy(hwnd);

        const NULLREGION: i32 = 1;
        assert_eq!(
            kind, NULLREGION,
            "explicit empty rect push must install an EMPTY region \
             (zero hit-test area), not clear the clip (kind 0 = no region)"
        );
    }

    /// [S-17] Characterization: before the frontend's first rect push (no
    /// map entry at all) the window stays un-clipped: a brief full
    /// transparent cover at startup beats an invisible window.
    #[cfg(target_os = "windows")]
    #[test]
    fn s17_no_push_yet_leaves_window_unclipped() {
        if should_skip_win32() { return; }
        let _g = WIN32_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        use winapi::shared::windef::RECT;
        use winapi::um::winuser::GetWindowRgnBox;

        let hwnd = make_test_window(0, 0, 800, 600);
        assert!(!hwnd.is_null());
        let key = hwnd as usize;
        widget_rects_map().lock().unwrap().remove(&key); // ensure no entry

        unsafe { apply_widget_region(hwnd, true); }

        let mut box_rect: RECT = unsafe { std::mem::zeroed() };
        let kind = unsafe { GetWindowRgnBox(hwnd, &mut box_rect) };
        destroy(hwnd);

        assert_eq!(kind, 0, "no rect push yet: window must have no region installed");
    }

    // -----------------------------------------------------------------
    // decide_region_clip: pure decision behind apply_widget_region
    // -----------------------------------------------------------------

    /// [S-17] No push yet → leave un-clipped (startup full-cover).
    #[cfg(target_os = "windows")]
    #[test]
    fn s17_region_clip_no_push_is_full() {
        assert_eq!(decide_region_clip(None), RegionClip::Full);
    }

    /// [S-18] Explicit empty push → empty region (zero hit-test area).
    #[cfg(target_os = "windows")]
    #[test]
    fn s18_region_clip_explicit_empty_is_empty() {
        assert_eq!(decide_region_clip(Some(&[])), RegionClip::Empty);
    }

    /// [S-18] A push of only degenerate (zero-area) rects builds no region
    /// piece, treat it as an empty push, not as a clear.
    #[cfg(target_os = "windows")]
    #[test]
    fn s18_region_clip_degenerate_only_is_empty() {
        let degenerate = [sr(10, 10, 10, 50, 0), sr(5, 5, 100, 5, 8)];
        assert_eq!(decide_region_clip(Some(&degenerate)), RegionClip::Empty);
    }

    /// [S-17] Any positive-area rect → clip to the rect union.
    #[cfg(target_os = "windows")]
    #[test]
    fn s17_region_clip_rects_when_any_positive_area() {
        let rects = [sr(10, 10, 10, 50, 0), sr(20, 20, 120, 90, 8)];
        assert_eq!(decide_region_clip(Some(&rects)), RegionClip::Rects);
    }

    // -----------------------------------------------------------------
    // Per-monitor disable: persistence + reconcile decision tests
    // -----------------------------------------------------------------
    //
    // Reproduces the user's bug: disabling a monitor in Display Settings
    // worked (window closed → desktop right-click recovered), but
    // re-enabling did not bring the overlay back. Root cause traced to
    // `WebviewWindowBuilder::build` being called off the main thread; the
    // unit tests below lock down the pure layers (persistence + the
    // reconcile decision matrix) so that whatever path the Tauri integration
    // takes, the inputs feeding into build/close are exactly what we expect.

    /// A monitor whose stable id happens to equal its ordinal (the
    /// [MI-02] `EnumDisplayDevicesW`-failed fallback). Keeps the pre-#70
    /// tests below expressing exactly what they always did.
    fn mi(name: &str, x: i32, y: i32, w: i32, h: i32) -> MonitorInfo {
        // id == ordinal IS the [MI-02] fallback shape, so it is not resolved.
        MonitorInfo { id_stable: false, ..mi_id(name, name, x, y, w, h) }
    }

    /// `plan_reconcile` with NOTHING bound yet ([MI-06]): slots fall out as
    /// `0..monitors.len()` in enumeration order, which is the startup case and
    /// what every pre-#93 test assumed. Tests that care about slot stability
    /// pass a real binding instead.
    fn plan_fresh(
        monitors: &[MonitorInfo],
        disabled: &std::collections::HashSet<String>,
        window_exists: impl Fn(&str) -> bool,
    ) -> Vec<ReconcileAction> {
        let slots = assign_monitor_slots(monitors, &std::collections::HashMap::new());
        plan_reconcile(monitors, &slots, disabled, window_exists)
    }

    /// A `stable id → slot` binding, as `bound_monitor_slots` would report it.
    fn bound(pairs: &[(&str, usize)]) -> std::collections::HashMap<String, usize> {
        pairs.iter().map(|(id, s)| ((*id).to_string(), *s)).collect()
    }

    /// A monitor with a stable id ([MI-01]) that is DISTINCT from its GDI
    /// ordinal, i.e. every real monitor. Any gate that still keys on the
    /// ordinal is visible immediately.
    fn mi_id(id: &str, name: &str, x: i32, y: i32, w: i32, h: i32) -> MonitorInfo {
        MonitorInfo {
            id: id.to_string(),
            id_stable: true,
            device_name: name.to_string(),
            primary: false,
            x, y, width: w, height: h,
        }
    }

    /// Device-interface paths as Windows really hands them back.
    const DELL_ID: &str =
        "\\\\?\\DISPLAY#DEL4098#5&1a2b3c&0&UID4353#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}";
    const LG_ID: &str =
        "\\\\?\\DISPLAY#GSM5B09#5&1a2b3c&0&UID4355#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}";

    fn id_map(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        pairs.iter().map(|(n, i)| ((*n).to_string(), (*i).to_string())).collect()
    }

    fn set_of(items: &[&str]) -> std::collections::HashSet<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    fn temp_disabled_path(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "themia_test_disabled_{}_{}_{}.json",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        p
    }

    #[test]
    fn read_disabled_monitors_missing_file_returns_empty() {
        let p = temp_disabled_path("missing");
        let _ = std::fs::remove_file(&p);
        let set = read_disabled_monitors_at(&p);
        assert!(set.is_empty(), "missing file must yield empty set");
    }

    #[test]
    fn read_disabled_monitors_bad_json_returns_empty() {
        let p = temp_disabled_path("badjson");
        std::fs::write(&p, "not valid json").expect("seed");
        let set = read_disabled_monitors_at(&p);
        let _ = std::fs::remove_file(&p);
        assert!(set.is_empty(), "bad JSON must yield empty set, not crash");
    }

    /// Well-formed JSON of the WRONG shape (an object, not the expected
    /// `Vec<String>`) must hit the parse-error branch and fall back to an
    /// empty set without panicking; corruption is logged, never crashes the
    /// startup path that reads this file.
    #[test]
    fn read_disabled_monitors_wrong_shape_returns_empty() {
        let p = temp_disabled_path("wrongshape");
        std::fs::write(&p, r#"{"unexpected":"object"}"#).expect("seed");
        let set = read_disabled_monitors_at(&p);
        let _ = std::fs::remove_file(&p);
        assert!(
            set.is_empty(),
            "valid-JSON-but-wrong-shape must yield empty set, not crash"
        );
    }

    #[test]
    fn write_then_read_disabled_monitors_roundtrip() {
        let p = temp_disabled_path("roundtrip");
        let mut set = std::collections::HashSet::new();
        set.insert("\\\\.\\DISPLAY1".to_string());
        set.insert("\\\\.\\DISPLAY2".to_string());

        write_disabled_monitors_at(&p, &set).expect("write");
        let read_back = read_disabled_monitors_at(&p);
        let _ = std::fs::remove_file(&p);

        assert_eq!(read_back, set, "roundtrip must preserve the disabled set");
    }

    #[test]
    fn write_empty_disabled_monitors_yields_empty_read() {
        let p = temp_disabled_path("empty");
        let empty = std::collections::HashSet::new();
        write_disabled_monitors_at(&p, &empty).expect("write");
        let read_back = read_disabled_monitors_at(&p);
        let _ = std::fs::remove_file(&p);
        assert!(read_back.is_empty(), "empty write → empty read");
    }

    #[test]
    fn write_disabled_monitors_overwrites_previous() {
        let p = temp_disabled_path("overwrite");
        let mut first = std::collections::HashSet::new();
        first.insert("\\\\.\\DISPLAY1".to_string());
        first.insert("\\\\.\\DISPLAY2".to_string());
        write_disabled_monitors_at(&p, &first).expect("write 1");

        let mut second = std::collections::HashSet::new();
        second.insert("\\\\.\\DISPLAY3".to_string());
        write_disabled_monitors_at(&p, &second).expect("write 2");

        let read_back = read_disabled_monitors_at(&p);
        let _ = std::fs::remove_file(&p);

        // Critical for the re-enable case: write({}) after write({A}) must
        // result in an empty file, not A leaking through.
        assert_eq!(read_back, second, "second write must fully overwrite the first");
        assert!(!read_back.contains("\\\\.\\DISPLAY1"), "old entries must be gone");
    }

    /// Re-enable scenario at the persistence layer: the first toggle marks
    /// DISPLAY2 disabled, the second toggle clears it. After the clear, the
    /// file must report DISPLAY2 as enabled, otherwise the next startup
    /// (and the next reconcile) would still skip building DISPLAY2's
    /// overlay.
    #[test]
    fn reenable_clears_the_disabled_entry_on_disk() {
        let p = temp_disabled_path("reenable");

        let mut disabled = std::collections::HashSet::new();
        disabled.insert("\\\\.\\DISPLAY2".to_string());
        write_disabled_monitors_at(&p, &disabled).expect("disable");
        let after_disable = read_disabled_monitors_at(&p);
        assert!(after_disable.contains("\\\\.\\DISPLAY2"), "disable must persist");

        // Re-enable: frontend now sends an empty list.
        let cleared: std::collections::HashSet<String> = std::collections::HashSet::new();
        write_disabled_monitors_at(&p, &cleared).expect("re-enable");
        let after_enable = read_disabled_monitors_at(&p);
        let _ = std::fs::remove_file(&p);

        assert!(
            after_enable.is_empty(),
            "re-enable must leave the on-disk set empty; got {:?}",
            after_enable
        );
    }

    // -----------------------------------------------------------------
    // plan_reconcile: destroy/build decision matrix
    // -----------------------------------------------------------------

    #[test]
    fn plan_reconcile_disable_with_existing_window_closes() {
        let monitors = vec![
            mi("\\\\.\\DISPLAY1", 0, 0, 1920, 1080),
            mi("\\\\.\\DISPLAY2", 1920, 0, 1920, 1080),
        ];
        let mut disabled = std::collections::HashSet::new();
        disabled.insert("\\\\.\\DISPLAY2".to_string());

        let actions = plan_fresh(&monitors, &disabled, |_| true);

        assert_eq!(actions.len(), 2);
        assert!(matches!(actions[0], ReconcileAction::Noop { .. }), "DISPLAY1 stays");
        assert_eq!(
            actions[1],
            ReconcileAction::Close { label: "monitor-1".to_string() },
            "DISPLAY2 must be closed (zero-resource)"
        );
    }

    /// The exact scenario the user reported: monitor disabled (window closed),
    /// they re-enable → reconcile must Build for that monitor.
    #[test]
    fn plan_reconcile_reenable_with_absent_window_builds() {
        let monitors = vec![
            mi("\\\\.\\DISPLAY1", 0, 0, 1920, 1080),
            mi("\\\\.\\DISPLAY2", 1920, 0, 1920, 1080),
        ];
        let disabled: std::collections::HashSet<String> = std::collections::HashSet::new();

        let actions = plan_fresh(&monitors, &disabled, |label| label == "monitor-0");

        assert_eq!(actions.len(), 2);
        assert!(matches!(actions[0], ReconcileAction::Noop { .. }), "DISPLAY1 stays");
        assert_eq!(
            actions[1],
            ReconcileAction::Build {
                index: 1,
                monitor_id: "\\\\.\\DISPLAY2".to_string(),
                x: 1920, y: 0, width: 1920, height: 1080,
            },
            "DISPLAY2 must be rebuilt after re-enable"
        );
    }

    /// Startup-with-persisted-disable: file on disk says DISPLAY2 is
    /// disabled; the startup loop skipped its build. Reconcile (called from
    /// the migration sync invoked by App.tsx) sees no window for DISPLAY2
    /// + DISPLAY2 still disabled → Noop, no rebuild attempt.
    #[test]
    fn plan_reconcile_startup_skips_disabled_monitor() {
        let monitors = vec![
            mi("\\\\.\\DISPLAY1", 0, 0, 1920, 1080),
            mi("\\\\.\\DISPLAY2", 1920, 0, 1920, 1080),
        ];
        let mut disabled = std::collections::HashSet::new();
        disabled.insert("\\\\.\\DISPLAY2".to_string());

        // monitor-0 built at startup, monitor-1 skipped.
        let actions = plan_fresh(&monitors, &disabled, |label| label == "monitor-0");

        assert_eq!(actions.len(), 2);
        assert!(matches!(actions[0], ReconcileAction::Noop { reason: "enabled+present", .. }));
        assert!(
            matches!(actions[1], ReconcileAction::Noop { reason: "disabled+absent", .. }),
            "DISPLAY2 stays absent (already-disabled)"
        );
    }

    #[test]
    fn plan_reconcile_steady_state_is_all_noops() {
        let monitors = vec![
            mi("\\\\.\\DISPLAY1", 0, 0, 1920, 1080),
            mi("\\\\.\\DISPLAY2", 1920, 0, 1920, 1080),
        ];
        let disabled: std::collections::HashSet<String> = std::collections::HashSet::new();

        let actions = plan_fresh(&monitors, &disabled, |_| true);

        assert_eq!(actions.len(), 2);
        for a in &actions {
            assert!(matches!(a, ReconcileAction::Noop { .. }), "steady state must be noop: {a:?}");
        }
    }

    /// Rapid-toggle race protection: if `exists` returns true because Tauri
    /// hasn't finished closing yet, the action must be Noop, since building a
    /// label that's still mid-close errors with "label already exists".
    #[test]
    fn plan_reconcile_enabled_and_present_is_noop_not_build() {
        let monitors = vec![mi("\\\\.\\DISPLAY1", 0, 0, 1920, 1080)];
        let disabled: std::collections::HashSet<String> = std::collections::HashSet::new();
        let actions = plan_fresh(&monitors, &disabled, |_| true);
        assert_eq!(actions.len(), 1);
        assert!(
            matches!(actions[0], ReconcileAction::Noop { reason: "enabled+present", .. }),
            "enabled+present must be noop: {:?}", actions[0]
        );
    }

    // -----------------------------------------------------------------
    // #70: stable monitor identity ([MI-01]..[MI-05], [DW-07])
    // -----------------------------------------------------------------
    //
    // Reproduces the reported bug: widgets pinned to display 3 reappear on
    // display 2 after every reboot. `\\.\DISPLAYn` is an OS-assigned ORDINAL,
    // reshuffled across reboots, so the persisted disabled set kept naming a
    // different physical panel each boot. These tests pin the fix: the key is
    // the panel's stable device-interface path, legacy files are re-keyed once,
    // and an empty enumeration never rewrites (or fails open on) config.

    // --- [MI-02] Win32 buffer decoding -------------------------------------

    /// `DISPLAY_DEVICEW.DeviceID` is a fixed 128-WCHAR array; everything past
    /// the first NUL is undefined padding and must never reach a persisted key.
    #[test]
    fn s70_utf16_buffer_trims_at_first_nul() {
        let mut buf = [0u16; 8];
        for (i, c) in "AB".encode_utf16().enumerate() {
            buf[i] = c;
        }
        buf[3] = b'X' as u16; // garbage past the terminator
        assert_eq!(utf16_buf_to_string(&buf), "AB");
    }

    /// A buffer with no NUL at all (full-length string) must decode whole
    /// rather than panic on the missing terminator.
    #[test]
    fn s70_utf16_buffer_without_nul_decodes_whole() {
        let buf: Vec<u16> = "ABCD".encode_utf16().collect();
        assert_eq!(utf16_buf_to_string(&buf), "ABCD");
    }

    /// [MI-02] `EnumDisplayDevicesW` succeeding with an EMPTY `DeviceID`
    /// (indirect/virtual displays) must fall back to the ordinal, never to an
    /// empty key: empty keys would collide across every such monitor.
    #[test]
    fn s70_empty_device_id_falls_back_to_device_name() {
        let (id, fell_back) = choose_monitor_id("", "\\\\.\\DISPLAY2");
        assert_eq!(id, "\\\\.\\DISPLAY2");
        assert!(fell_back, "empty DeviceID must report the degradation");

        let (id, fell_back) = choose_monitor_id("   ", "\\\\.\\DISPLAY2");
        assert_eq!(id, "\\\\.\\DISPLAY2", "whitespace-only DeviceID is empty too");
        assert!(fell_back);
    }

    #[test]
    fn s70_present_device_id_is_used_as_the_id() {
        let (id, fell_back) = choose_monitor_id(DELL_ID, "\\\\.\\DISPLAY2");
        assert_eq!(id, DELL_ID);
        assert!(!fell_back, "a real DeviceID is not a fallback");
    }

    // --- [MI-03] migration -------------------------------------------------

    /// The core re-key: a legacy file naming ordinals becomes a file naming
    /// the panels those ordinals resolve to right now.
    #[test]
    fn s70_migrates_device_name_keys_to_stable_ids() {
        let map = id_map(&[("\\\\.\\DISPLAY1", DELL_ID), ("\\\\.\\DISPLAY2", LG_ID)]);
        let out = migrate_disabled_ids(&["\\\\.\\DISPLAY2".to_string()], &map)
            .expect("non-empty enumeration must migrate");
        assert_eq!(out.kept, vec![LG_ID.to_string()]);
        assert!(out.dropped.is_empty());
    }

    /// Idempotence: entries that already look like stable ids pass through
    /// byte-for-byte, so re-running the migration is a no-op.
    #[test]
    fn s70_migration_is_idempotent_for_stable_ids() {
        let map = id_map(&[("\\\\.\\DISPLAY1", DELL_ID), ("\\\\.\\DISPLAY2", LG_ID)]);
        let saved = vec![LG_ID.to_string()];
        let once = migrate_disabled_ids(&saved, &map).expect("migrate");
        let twice = migrate_disabled_ids(&once.kept, &map).expect("migrate again");
        assert_eq!(once.kept, saved, "stable ids pass through untouched");
        assert_eq!(twice.kept, once.kept, "migration must be idempotent");
        assert!(twice.dropped.is_empty());
    }

    /// An ordinal that resolves to no connected monitor is DROPPED, not kept:
    /// a bare ordinal is not an identity, and keeping it is the #70 bug (it
    /// would disable whichever panel inherits that ordinal next boot).
    #[test]
    fn s70_migration_drops_unresolvable_device_names() {
        let map = id_map(&[("\\\\.\\DISPLAY1", DELL_ID)]);
        let out = migrate_disabled_ids(
            &["\\\\.\\DISPLAY1".to_string(), "\\\\.\\DISPLAY3".to_string()],
            &map,
        )
        .expect("migrate");
        assert_eq!(out.kept, vec![DELL_ID.to_string()]);
        assert_eq!(
            out.dropped,
            vec!["\\\\.\\DISPLAY3".to_string()],
            "the unresolvable ordinal must be reported so the drop is loud"
        );
    }

    /// The mild cost of the drop rule, bounded: a STABLE id whose panel is
    /// unplugged right now is still meaningful, so it survives. Only ordinals
    /// are dropped.
    #[test]
    fn s70_migration_keeps_stable_ids_of_disconnected_monitors() {
        let map = id_map(&[("\\\\.\\DISPLAY1", DELL_ID)]);
        let out = migrate_disabled_ids(&[LG_ID.to_string()], &map).expect("migrate");
        assert_eq!(out.kept, vec![LG_ID.to_string()]);
        assert!(out.dropped.is_empty(), "an offline panel's id is not garbage");
    }

    /// Mixed legacy + already-stable entries: each is handled on its own terms.
    #[test]
    fn s70_migration_handles_mixed_legacy_and_stable_entries() {
        let map = id_map(&[("\\\\.\\DISPLAY1", DELL_ID), ("\\\\.\\DISPLAY2", LG_ID)]);
        let out = migrate_disabled_ids(
            &[
                "\\\\.\\DISPLAY1".to_string(), // legacy → DELL
                LG_ID.to_string(),             // already stable → kept
                "\\\\.\\DISPLAY7".to_string(), // unresolvable → dropped
            ],
            &map,
        )
        .expect("migrate");
        let mut want = vec![DELL_ID.to_string(), LG_ID.to_string()];
        want.sort();
        assert_eq!(out.kept, want);
        assert_eq!(out.dropped, vec!["\\\\.\\DISPLAY7".to_string()]);
    }

    /// A legacy entry that resolves AND its own resolved id both present:
    /// the result must not contain the id twice.
    #[test]
    fn s70_migration_dedupes_when_legacy_and_stable_collide() {
        let map = id_map(&[("\\\\.\\DISPLAY1", DELL_ID)]);
        let out = migrate_disabled_ids(
            &["\\\\.\\DISPLAY1".to_string(), DELL_ID.to_string()],
            &map,
        )
        .expect("migrate");
        assert_eq!(out.kept, vec![DELL_ID.to_string()], "no duplicate ids");
    }

    /// THE safety rule. An empty enumeration (autostart at logon, all panels
    /// asleep, a driver still initializing) resolves nothing: migrating then
    /// would drop the user's entire config. Signal "do not write" instead.
    #[test]
    fn s70_empty_enumeration_does_not_migrate() {
        let empty = std::collections::HashMap::new();
        assert_eq!(
            migrate_disabled_ids(&["\\\\.\\DISPLAY2".to_string()], &empty),
            None,
            "empty enumeration must signal do-not-write, never a lossy rewrite"
        );
    }

    // --- [MI-03] file-level migration --------------------------------------

    #[test]
    fn s70_migrate_file_rewrites_legacy_and_stamps_schema_version() {
        let p = temp_disabled_path("mig_legacy");
        // Pre-#70 on-disk shape: a bare array of ordinals.
        std::fs::write(&p, r#"["\\\\.\\DISPLAY2"]"#).expect("seed");

        let monitors = vec![
            mi_id(DELL_ID, "\\\\.\\DISPLAY1", 0, 0, 1920, 1080),
            mi_id(LG_ID, "\\\\.\\DISPLAY2", 1920, 0, 1920, 1080),
        ];
        migrate_disabled_monitors_file_at(&p, &monitors);

        let raw = std::fs::read_to_string(&p).expect("read back");
        let set = read_disabled_monitors_at(&p);
        let _ = std::fs::remove_file(&p);

        assert!(
            raw.contains(&format!("\"schemaVersion\":{DISABLED_MONITORS_SCHEMA_VERSION}")),
            "migrated file must carry the run-once marker; got {raw}"
        );
        assert_eq!(set, set_of(&[LG_ID]), "the LG panel stays disabled by identity");
    }

    /// Run-once: a file already stamped with the current schemaVersion is left
    /// byte-for-byte alone, even if its ids happen to look unusual.
    #[test]
    fn s70_migrate_file_leaves_already_migrated_file_untouched() {
        let p = temp_disabled_path("mig_current");
        let before = format!(
            r#"{{"schemaVersion":{DISABLED_MONITORS_SCHEMA_VERSION},"disabled":["{LG_ID}"]}}"#
        )
        .replace('\\', "\\\\"); // JSON-escape the backslashes in the path
        std::fs::write(&p, &before).expect("seed");

        // Enumeration where DISPLAY2 now resolves to the DELL panel: an
        // ordinal-keyed re-run would corrupt this file.
        let monitors = vec![mi_id(DELL_ID, "\\\\.\\DISPLAY2", 0, 0, 1920, 1080)];
        migrate_disabled_monitors_file_at(&p, &monitors);

        let after = std::fs::read_to_string(&p).expect("read back");
        let _ = std::fs::remove_file(&p);
        assert_eq!(after, before, "an already-migrated file must not be rewritten");
    }

    /// Empty enumeration → the legacy file survives EXACTLY as it was, so the
    /// migration can still run correctly once the panels show up.
    #[test]
    fn s70_migrate_file_does_not_touch_file_when_enumeration_empty() {
        let p = temp_disabled_path("mig_empty");
        let before = r#"["\\\\.\\DISPLAY2"]"#;
        std::fs::write(&p, before).expect("seed");

        migrate_disabled_monitors_file_at(&p, &[]);

        let after = std::fs::read_to_string(&p).expect("read back");
        let _ = std::fs::remove_file(&p);
        assert_eq!(after, before, "empty enumeration must never rewrite config");
    }

    /// "Corrupt" is not "absent": an unparseable file may hold state the user
    /// set, so it is left alone rather than replaced with an empty list.
    #[test]
    fn s70_migrate_file_never_rewrites_a_corrupt_file() {
        let p = temp_disabled_path("mig_corrupt");
        let before = "{not json at all";
        std::fs::write(&p, before).expect("seed");

        let monitors = vec![mi_id(DELL_ID, "\\\\.\\DISPLAY1", 0, 0, 1920, 1080)];
        migrate_disabled_monitors_file_at(&p, &monitors);

        let after = std::fs::read_to_string(&p).expect("read back");
        let _ = std::fs::remove_file(&p);
        assert_eq!(after, before, "a corrupt file must never be silently replaced");
    }

    /// Absent file: migration must not conjure one (first run stays clean).
    #[test]
    fn s70_migrate_file_creates_nothing_when_absent() {
        let p = temp_disabled_path("mig_absent");
        let _ = std::fs::remove_file(&p);
        let monitors = vec![mi_id(DELL_ID, "\\\\.\\DISPLAY1", 0, 0, 1920, 1080)];
        migrate_disabled_monitors_file_at(&p, &monitors);
        let exists = p.exists();
        let _ = std::fs::remove_file(&p);
        assert!(!exists, "nothing disabled + nothing saved → no file");
    }

    // --- [MI-01] file format parsing ---------------------------------------

    #[test]
    fn s70_parse_bare_array_is_legacy() {
        assert_eq!(
            parse_disabled_monitors(r#"["\\\\.\\DISPLAY1"]"#),
            DisabledMonitorsDoc::Legacy(vec!["\\\\.\\DISPLAY1".to_string()]),
        );
    }

    #[test]
    fn s70_parse_versioned_object_is_current() {
        let s = format!(
            r#"{{"schemaVersion":{DISABLED_MONITORS_SCHEMA_VERSION},"disabled":["x"]}}"#
        );
        assert_eq!(
            parse_disabled_monitors(&s),
            DisabledMonitorsDoc::Current(vec!["x".to_string()]),
        );
    }

    /// A file written by a NEWER build still holds stable ids; treat it as
    /// current rather than "migrating" a downgrade back into ordinals.
    #[test]
    fn s70_parse_newer_schema_is_treated_as_current() {
        let s = r#"{"schemaVersion":99,"disabled":["x"]}"#;
        assert_eq!(
            parse_disabled_monitors(s),
            DisabledMonitorsDoc::Current(vec!["x".to_string()]),
        );
    }

    #[test]
    fn s70_parse_garbage_is_corrupt_not_empty() {
        assert_eq!(parse_disabled_monitors("{not json"), DisabledMonitorsDoc::Corrupt);
        assert_eq!(
            parse_disabled_monitors(r#"{"unexpected":"object"}"#),
            DisabledMonitorsDoc::Corrupt,
            "valid JSON of the wrong shape is corrupt, not an empty list",
        );
    }

    #[test]
    fn s70_absent_and_corrupt_are_distinct() {
        let p = temp_disabled_path("absent_vs_corrupt");
        let _ = std::fs::remove_file(&p);
        assert_eq!(read_disabled_monitors_doc_at(&p), DisabledMonitorsDoc::Absent);
        std::fs::write(&p, "@@@").expect("seed");
        assert_eq!(read_disabled_monitors_doc_at(&p), DisabledMonitorsDoc::Corrupt);
        let _ = std::fs::remove_file(&p);
    }

    // --- [MI-05] the build gate keys on the id -----------------------------

    #[test]
    fn s70_monitor_should_have_window_ignores_the_ordinal() {
        let disabled = set_of(&[DELL_ID]);
        assert!(!monitor_should_have_window(DELL_ID, &disabled), "disabled by identity");
        assert!(
            monitor_should_have_window("\\\\.\\DISPLAY1", &disabled),
            "an ordinal must never match a disabled id",
        );
        assert!(monitor_should_have_window(LG_ID, &set_of(&[])), "nothing disabled → build");
    }

    /// THE #70 regression. The user disabled the DELL panel. After a reboot
    /// Windows swapped the ordinals: DELL is now `\\.\DISPLAY2` and the LG is
    /// `\\.\DISPLAY1`. The persisted set is unchanged on disk, and it must
    /// still designate the DELL panel — not "whatever holds DISPLAY1 now".
    #[test]
    fn s70_disable_gate_keys_on_stable_id_not_device_ordinal() {
        let monitors = vec![
            mi_id(LG_ID, "\\\\.\\DISPLAY1", 0, 0, 1920, 1080),
            mi_id(DELL_ID, "\\\\.\\DISPLAY2", 1920, 0, 1920, 1080),
        ];
        let disabled = set_of(&[DELL_ID]);

        let actions = plan_fresh(&monitors, &disabled, |_| true);

        assert_eq!(actions.len(), 2);
        assert!(
            matches!(actions[0], ReconcileAction::Noop { reason: "enabled+present", .. }),
            "the LG panel is enabled and must keep its overlay: {:?}", actions[0],
        );
        assert_eq!(
            actions[1],
            ReconcileAction::Close { label: "monitor-1".to_string() },
            "the DELL panel is disabled by identity, wherever its ordinal landed",
        );
    }

    /// The user-visible half of #70: widgets must follow the PANEL across a
    /// reboot that reshuffles ordinals. Same physical setup as above, no
    /// windows built yet (startup): exactly one overlay, on the LG.
    #[test]
    fn s70_startup_builds_follow_the_panel_across_an_ordinal_reshuffle() {
        let disabled = set_of(&[DELL_ID]);

        // Boot A: DELL=DISPLAY1, LG=DISPLAY2.
        let boot_a = vec![
            mi_id(DELL_ID, "\\\\.\\DISPLAY1", 0, 0, 1920, 1080),
            mi_id(LG_ID, "\\\\.\\DISPLAY2", 1920, 0, 1920, 1080),
        ];
        // Boot B: the ordinals swapped, nothing else changed.
        let boot_b = vec![
            mi_id(LG_ID, "\\\\.\\DISPLAY1", 0, 0, 1920, 1080),
            mi_id(DELL_ID, "\\\\.\\DISPLAY2", 1920, 0, 1920, 1080),
        ];

        let built = |monitors: &[MonitorInfo]| -> Vec<String> {
            plan_fresh(monitors, &disabled, |_| false)
                .into_iter()
                .filter_map(|a| match a {
                    ReconcileAction::Build { monitor_id, .. } => Some(monitor_id),
                    _ => None,
                })
                .collect()
        };

        assert_eq!(built(&boot_a), vec![LG_ID.to_string()]);
        assert_eq!(
            built(&boot_b),
            vec![LG_ID.to_string()],
            "the same physical panel keeps its overlay across an ordinal reshuffle",
        );
    }

    // --- [DW-07] no phantom fallback ---------------------------------------

    /// The second, independent half of #70: the old startup path built an
    /// unconditional 1920x1080 window at (0,0) — the PRIMARY's origin — with a
    /// bogus "FALLBACK" device name whenever enumeration came back empty, and
    /// never consulted the disabled list. An autostart LogonTrigger has no
    /// delay ([AS-01]/[AS-02]), so this fired at the least-settled moment and
    /// dumped every widget onto the primary. Empty enumeration must build
    /// NOTHING; the watcher ([DW-05]) builds once displays actually appear.
    #[test]
    fn s70_fallback_never_ignores_disabled_list() {
        let disabled = set_of(&[DELL_ID]);
        let actions = plan_fresh(&[], &disabled, |_| false);
        assert!(
            actions.is_empty(),
            "empty enumeration must produce no build at all, got {actions:?}",
        );
    }

    /// And with a disabled monitor present, startup still builds only the
    /// enabled ones (no fail-open).
    #[test]
    fn s70_startup_never_builds_a_disabled_monitor() {
        let monitors = vec![
            mi_id(DELL_ID, "\\\\.\\DISPLAY1", 0, 0, 1920, 1080),
            mi_id(LG_ID, "\\\\.\\DISPLAY2", 1920, 0, 1920, 1080),
        ];
        let disabled = set_of(&[DELL_ID, LG_ID]);
        let actions = plan_fresh(&monitors, &disabled, |_| false);
        assert!(
            actions.iter().all(|a| matches!(a, ReconcileAction::Noop { reason: "disabled+absent", .. })),
            "all-disabled startup must build nothing: {actions:?}",
        );
    }

    // -----------------------------------------------------------------
    // #93: overlay slots follow the PANEL, not the enumeration index
    // ([MI-06])
    // -----------------------------------------------------------------
    //
    // Reported bug: a 2-monitor desk, widgets on the secondary. Powering the
    // monitors off overnight drops them from the topology; powering them back
    // on re-adds them in link-training order, which differs from the order they
    // left in. The pre-fix pass re-derived every label from the enumeration
    // index, so the overlay that was covering the secondary got re-pinned onto
    // the primary — the widgets visibly moved screens and stopped taking right
    // clicks (stale click-through region), until the app was killed and
    // relaunched. The reporter's own bisect nails the trigger: with only the
    // secondary powered on the widgets are correct, and they jump the moment
    // the primary comes back.

    /// THE #93 regression. Secondary alone → it holds slot 0. The primary wakes
    /// and enumerates FIRST. The secondary must keep slot 0 (its overlay stays
    /// put); the primary must take a free slot, not evict it.
    #[test]
    fn s93_waking_a_monitor_does_not_steal_another_panels_slot() {
        // Secondary alone while the primary is asleep: it owns `monitor-0`.
        let live = bound(&[(LG_ID, 0)]);

        // Both awake. The primary enumerates first, as it usually does.
        let monitors = vec![
            mi_id(DELL_ID, "\\\\.\\DISPLAY1", 0, 0, 2560, 1392),
            mi_id(LG_ID, "\\\\.\\DISPLAY2", 2560, 0, 3440, 1392),
        ];

        let slots = assign_monitor_slots(&monitors, &live);

        assert_eq!(
            slots[1], 0,
            "the LG must keep the slot its overlay already occupies; \
             re-deriving it from the enumeration index is #93",
        );
        assert_eq!(slots[0], 1, "the waking DELL takes the lowest FREE slot");
    }

    /// The consequence at the plan level: the secondary's existing overlay is a
    /// Noop (it stays where it is), and only the newly-awake primary is built.
    /// Pre-fix, `monitor-0` was Noop'd for the DELL and then re-pinned onto the
    /// DELL's work area, dragging the LG's widgets across the desk.
    #[test]
    fn s93_wake_plan_keeps_the_sleeping_panels_overlay_and_builds_only_the_new_one() {
        let live = bound(&[(LG_ID, 0)]);
        let monitors = vec![
            mi_id(DELL_ID, "\\\\.\\DISPLAY1", 0, 0, 2560, 1392),
            mi_id(LG_ID, "\\\\.\\DISPLAY2", 2560, 0, 3440, 1392),
        ];
        let disabled = set_of(&[]);
        let slots = assign_monitor_slots(&monitors, &live);

        let actions = plan_reconcile(&monitors, &slots, &disabled, |label| label == "monitor-0");

        assert_eq!(
            actions[0],
            ReconcileAction::Build {
                index: 1,
                monitor_id: DELL_ID.to_string(),
                x: 0, y: 0, width: 2560, height: 1392,
            },
            "the waking DELL gets a NEW overlay at a free slot",
        );
        assert!(
            matches!(actions[1], ReconcileAction::Noop { reason: "enabled+present", .. }),
            "the LG's overlay must be left alone: {:?}", actions[1],
        );
    }

    /// A panel that goes away frees its slot, and the slot is reused rather
    /// than growing forever across a week of nightly power-cycles.
    #[test]
    fn s93_slots_are_reused_after_a_panel_leaves() {
        let live = bound(&[(DELL_ID, 0), (LG_ID, 1)]);
        // The DELL is unplugged; a third panel appears.
        let monitors = vec![
            mi_id(LG_ID, "\\\\.\\DISPLAY1", 0, 0, 3440, 1392),
            mi_id("\\\\?\\DISPLAY#AOC0001#", "\\\\.\\DISPLAY2", 3440, 0, 1920, 1032),
        ];

        let slots = assign_monitor_slots(&monitors, &live);

        assert_eq!(slots[0], 1, "the LG keeps its slot");
        assert_eq!(slots[1], 0, "the newcomer reuses the slot the DELL freed");
    }

    /// Steady state must be a fixed point: re-running the assignment with the
    /// binding it just produced changes nothing. Without this the ~1s [DW-04]
    /// sweep would see divergence forever and fire a re-pin pass every 2s.
    #[test]
    fn s93_slot_assignment_is_idempotent() {
        let monitors = vec![
            mi_id(DELL_ID, "\\\\.\\DISPLAY1", 0, 0, 2560, 1392),
            mi_id(LG_ID, "\\\\.\\DISPLAY2", 2560, 0, 3440, 1392),
        ];
        let first = assign_monitor_slots(&monitors, &std::collections::HashMap::new());
        let live: std::collections::HashMap<String, usize> = monitors
            .iter()
            .zip(&first)
            .map(|(m, s)| (m.id.clone(), *s))
            .collect();

        assert_eq!(
            assign_monitor_slots(&monitors, &live),
            first,
            "a settled topology must re-assign to exactly the same slots",
        );
    }

    /// Two monitors resolving to the same bound slot (a stale map entry, or the
    /// [MI-02] ordinal fallback colliding) must never both be handed it: one
    /// label would be built twice and the other panel would get no overlay.
    #[test]
    fn s93_duplicate_binding_never_assigns_one_slot_twice() {
        let live = bound(&[(DELL_ID, 1), (LG_ID, 1)]);
        let monitors = vec![
            mi_id(DELL_ID, "\\\\.\\DISPLAY1", 0, 0, 1920, 1032),
            mi_id(LG_ID, "\\\\.\\DISPLAY2", 1920, 0, 1920, 1032),
        ];

        let slots = assign_monitor_slots(&monitors, &live);

        assert_eq!(slots[0], 1, "first in enumeration order keeps the contested slot");
        assert_eq!(slots[1], 0, "the loser is treated as new and takes a free slot");
    }

    /// Startup (nothing bound) must still be plain enumeration order, so the
    /// fix changes nothing about a cold launch.
    #[test]
    fn s93_unbound_startup_is_enumeration_order() {
        let monitors = vec![
            mi_id(DELL_ID, "\\\\.\\DISPLAY1", 0, 0, 1920, 1032),
            mi_id(LG_ID, "\\\\.\\DISPLAY2", 1920, 0, 1920, 1032),
        ];
        assert_eq!(
            assign_monitor_slots(&monitors, &std::collections::HashMap::new()),
            vec![0, 1],
        );
    }

    /// A disabled monitor still consumes a slot (it is part of the topology),
    /// so live slots are NOT guaranteed to be `0..len`. Pins the invariant the
    /// orphan sweep relies on: it must enumerate the live window set, never
    /// count up from `monitors.len()` and stop at the first gap.
    #[test]
    fn s93_slots_may_be_non_contiguous() {
        let live = bound(&[(LG_ID, 2)]);
        let monitors = vec![mi_id(LG_ID, "\\\\.\\DISPLAY1", 0, 0, 1920, 1032)];
        assert_eq!(assign_monitor_slots(&monitors, &live), vec![2]);
    }

    // --- [MI-03] legacy shape detection ------------------------------------

    #[test]
    fn s70_legacy_device_name_detection() {
        assert!(is_legacy_device_name("\\\\.\\DISPLAY1"));
        assert!(is_legacy_device_name("\\\\.\\display12"), "case-insensitive");
        assert!(!is_legacy_device_name(DELL_ID), "a device-interface path is not an ordinal");
        assert!(!is_legacy_device_name(""), "empty is not an ordinal");
    }

    // -----------------------------------------------------------------
    // decide_widget_kb_routing: keyboard hook must not swallow desktop
    // shortcuts when the WIDGET_KB target window is stale/missing.
    // -----------------------------------------------------------------

    /// Live target + successful emit: the widget owns the keystroke, so we
    /// consume it and keep the route. No stale-clear.
    #[test]
    fn widget_kb_consumes_when_window_found_and_emit_ok() {
        let out = decide_widget_kb_routing(true, true);
        assert!(out.consume, "live target + emit ok must consume");
        assert!(!out.clear_stale, "live target must not clear routing state");
    }

    /// Live target but a transient emit failure: the live window still owns the
    /// key, so we consume rather than leak it to the shell, and keep routing.
    #[test]
    fn widget_kb_consumes_when_window_found_but_emit_fails() {
        let out = decide_widget_kb_routing(true, false);
        assert!(out.consume, "live target must consume even if emit transiently fails");
        assert!(!out.clear_stale, "live target must not clear routing state");
    }

    /// Stale/missing target window (closed/destroyed): the keystroke must NOT
    /// be consumed: it has to fall through to CallNextHookEx so the desktop
    /// receives it, and the stale route must be cleared so the hook stops
    /// trying to use it. Reproduces the reported bug where a dead WIDGET_KB
    /// target swallowed desktop shortcuts.
    #[test]
    fn widget_kb_falls_through_and_clears_when_window_missing() {
        let out = decide_widget_kb_routing(false, false);
        assert!(!out.consume, "missing target must fall through (not swallow the key)");
        assert!(out.clear_stale, "missing target must clear stale routing state");
    }

    // -----------------------------------------------------------------
    // should_intercept_widget_keys / update_text_input_focus: issue #35,
    // while a text input is focused the overlay holds real Win32 focus and
    // native key delivery works, so the WH_KEYBOARD_LL hook must stand down
    // entirely. Intercepting there ate Backspace/Enter/arrows in the todo
    // add-task input AND the Backspaces Vietnamese IME tools (Unikey/EVKey
    // Telex) inject via SendInput to replace composed characters, turning
    // "Chào" into "Chaào".
    // -----------------------------------------------------------------

    /// Baseline: active route + no focused text input → hook intercepts
    /// (folder nav keys keep working while browsing lists).
    #[test]
    fn n35_intercepts_when_route_active_and_no_text_input() {
        assert!(should_intercept_widget_keys(true, false));
    }

    /// Regression for issue #35: a focused text input must force the hook to
    /// pass every whitelisted key through to native delivery, even with an
    /// active widget-keyboard route (mousedown on the widget set one).
    #[test]
    fn n35_passes_through_while_text_input_focused() {
        assert!(!should_intercept_widget_keys(true, true));
    }

    /// No active route → never intercept, regardless of input focus.
    #[test]
    fn n35_never_intercepts_without_active_route() {
        assert!(!should_intercept_widget_keys(false, false));
        assert!(!should_intercept_widget_keys(false, true));
    }

    // -----------------------------------------------------------------
    // should_consume_desktop_escape / update_escape_wanted: the desktop Escape
    // intercept (broadcasts folder-clear-selection, consumes the key) must
    // stand down while the focused webview's DOM escape stack has a handler
    // (edit mode / widget placing / an open context menu) so Escape reaches
    // the webview and closes the menu / exits the mode instead of being eaten.
    // -----------------------------------------------------------------

    /// Baseline: nothing wants Escape → the desktop intercept consumes it and
    /// broadcasts folder-clear-selection (cancel a cut from anywhere).
    #[test]
    fn esc_consumed_only_when_nothing_wants_it() {
        assert!(should_consume_desktop_escape(false, false));
    }

    /// Regression: while the DOM escape stack is active the hook must pass
    /// Escape through, or editMode/placing/ctxMenu never see it.
    #[test]
    fn esc_passes_through_when_escape_stack_active() {
        assert!(!should_consume_desktop_escape(false, true));
    }

    /// A focused text input already stands the hook down (issue #35); Escape
    /// must reach the input's own DOM handler regardless of the escape stack.
    #[test]
    fn esc_passes_through_while_text_input_focused() {
        assert!(!should_consume_desktop_escape(true, false));
        assert!(!should_consume_desktop_escape(true, true));
    }

    /// Multi-monitor: the escape-wanted flag aggregates per-window claims, so
    /// one window releasing Escape doesn't clear another window's claim.
    #[test]
    fn esc_wanted_aggregates_across_windows() {
        let mut set = std::collections::HashSet::new();
        assert!(update_escape_wanted(&mut set, "a".into(), true));
        assert!(update_escape_wanted(&mut set, "b".into(), true));
        assert!(update_escape_wanted(&mut set, "a".into(), false)); // b still wants it
        assert!(!update_escape_wanted(&mut set, "b".into(), false)); // now none
    }

    /// Regression for issue #35 symptom 1 (Backspace eaten) and symptom 2
    /// (IME doubling "Chào" → "Chaào"): both hinge on Backspace (VK 0x08)
    /// being a key the hook *claims*. The Vietnamese Telex IME (Unikey/EVKey)
    /// replaces a composed character by injecting a Backspace via SendInput;
    /// if the hook treats Backspace as a widget key it gets eaten before the
    /// WebView sees it and the old char is never removed. This pins the
    /// classification: Backspace IS a non-Ctrl widget key, so the focus gate
    /// (`should_intercept_widget_keys`) is the ONLY thing that lets the
    /// IME-injected Backspace through; assert that combination end to end.
    #[test]
    fn n35_backspace_is_a_widget_key_gated_by_text_input_focus() {
        // Backspace (no Ctrl) is part of the intercept whitelist...
        assert_eq!(
            widget_kb_action(false, 0x08),
            Some("backspace"),
            "Backspace must be a key the hook claims, else the gate is moot"
        );
        // ...so with an active route and NO focused input the hook would eat
        // it (the pre-fix behaviour that broke Backspace + IME)...
        assert!(should_intercept_widget_keys(true, false));
        // ...but a focused text input MUST force pass-through so the WebView
        // (and the IME's injected Backspace) get it natively.
        assert!(!should_intercept_widget_keys(true, true));
    }

    /// Regression for themia-releases #53 (Space eaten out of text inputs): the
    /// gallery's old GLOBAL `document` keydown called preventDefault() on Space,
    /// which, because a WS_EX_NOACTIVATE window only receives DOM keydown while
    /// a text input is focused, swallowed the space out of notes/todo inputs
    /// sharing the window. Space is now a hook-claimed widget key (slideshow
    /// pause) routed via the same gate as Backspace: with an active route and NO
    /// focused input the hook claims it, but a focused text input MUST force
    /// pass-through so the character reaches the input natively.
    #[test]
    fn issue53_space_is_a_widget_key_gated_by_text_input_focus() {
        assert_eq!(
            widget_kb_action(false, 0x20),
            Some("space"),
            "Space must be a key the hook claims (slideshow pause), else the gate is moot"
        );
        // Active route, no input focused → hook may claim Space for the widget.
        assert!(should_intercept_widget_keys(true, false));
        // Text input focused → hook stands down so Space reaches the input.
        assert!(!should_intercept_widget_keys(true, true));
    }

    /// The pure key classifier mirrors the inline whitelist exactly so the
    /// hook proc has a single, testable source of truth (one path). Ctrl
    /// letters map to clipboard/select-all actions; bare nav/edit keys map to
    /// their names; everything else is None (passes through untouched).
    #[test]
    fn n35_widget_kb_action_classification() {
        // Ctrl combos
        assert_eq!(widget_kb_action(true, 0x43), Some("c"));
        assert_eq!(widget_kb_action(true, 0x56), Some("v"));
        assert_eq!(widget_kb_action(true, 0x58), Some("x"));
        assert_eq!(widget_kb_action(true, 0x41), Some("a"));
        // Bare nav / edit keys
        assert_eq!(widget_kb_action(false, 0x2E), Some("delete"));
        assert_eq!(widget_kb_action(false, 0x71), Some("f2"));
        assert_eq!(widget_kb_action(false, 0x26), Some("up"));
        assert_eq!(widget_kb_action(false, 0x28), Some("down"));
        assert_eq!(widget_kb_action(false, 0x25), Some("left"));
        assert_eq!(widget_kb_action(false, 0x27), Some("right"));
        assert_eq!(widget_kb_action(false, 0x0D), Some("enter"));
        assert_eq!(widget_kb_action(false, 0x08), Some("backspace"));
        assert_eq!(widget_kb_action(false, 0x20), Some("space"));
        assert_eq!(widget_kb_action(false, 0x24), Some("home"));
        assert_eq!(widget_kb_action(false, 0x23), Some("end"));
        assert_eq!(widget_kb_action(false, 0x21), Some("pageup"));
        assert_eq!(widget_kb_action(false, 0x22), Some("pagedown"));
        // Tab is row-to-row focus in the list widgets (To Do [S-56]).
        assert_eq!(widget_kb_action(false, 0x09), Some("tab"));
        // Ctrl+Tab is the shell's window switcher, never ours.
        assert_eq!(widget_kb_action(true, 0x09), None);
        // A bare printable key (e.g. 'q' = 0x51) is NOT claimed; typing must
        // always reach the input. Ctrl+letter outside the clipboard set too.
        assert_eq!(widget_kb_action(false, 0x51), None);
        assert_eq!(widget_kb_action(true, 0x51), None);
        // Ctrl+Backspace is a Ctrl combo, not the bare-Backspace action; must
        // not be misclassified as the nav whitelist (delete-word reaches input).
        assert_eq!(widget_kb_action(true, 0x08), None);
    }

    /// The aggregate flag tracks per-window-label claims so two overlay
    /// windows (multi-monitor) can't clobber each other's focus state.
    #[test]
    fn n35_text_input_focus_aggregates_across_windows() {
        let mut set = std::collections::HashSet::new();
        assert!(update_text_input_focus(&mut set, "main-0".into(), true));
        assert!(update_text_input_focus(&mut set, "main-1".into(), true));
        // Window 0 blurs; window 1 still typing, flag must stay set.
        assert!(update_text_input_focus(&mut set, "main-0".into(), false));
        // Last window blurs; flag clears, hook resumes intercepting.
        assert!(!update_text_input_focus(&mut set, "main-1".into(), false));
    }

    /// Blurring a label that never claimed focus is a no-op (idempotent).
    #[test]
    fn n35_text_input_focus_blur_unknown_label_is_noop() {
        let mut set = std::collections::HashSet::new();
        assert!(update_text_input_focus(&mut set, "main-0".into(), true));
        assert!(update_text_input_focus(&mut set, "ghost".into(), false));
    }

    // -----------------------------------------------------------------
    // should_intercept_app_key: todo SPEC [S-38] extension, the app-level
    // shortcut block (Ctrl+E/W/Z/1-9) ran BEFORE the text-input gate, so a
    // focused inline input (todo add-task, folder rename) lost Ctrl+Z to
    // the file-op undo broadcast instead of native text undo, and Ctrl+1-9
    // could tear down the edit mid-typing via a view switch. Ctrl+E/W stay
    // consumed: falling through hands them to WebView2's browser
    // accelerators (Ctrl+W = close window).
    // -----------------------------------------------------------------

    /// Baseline: no focused text input → every app key intercepts.
    #[test]
    fn s38_app_keys_intercept_without_text_input() {
        for action in ["e", "w", "z", "1", "5", "9"] {
            assert!(
                should_intercept_app_key(action, false),
                "ctrl+{action} must intercept while no text input is focused"
            );
        }
    }

    /// Regression (todo SPEC [S-38]): Ctrl+Z while a text input is focused
    /// must fall through to native delivery so the input's own text undo
    /// runs, not the file-operation undo broadcast.
    #[test]
    fn s38_ctrl_z_stands_down_while_text_input_focused() {
        assert!(!should_intercept_app_key("z", true));
    }

    /// Ctrl+1-9 while a text input is focused must not switch views: the
    /// switch unmounts the widget tree and discards the in-progress edit.
    /// Falling through is safe: digit accelerators are tab-switch no-ops in
    /// a single-tab WebView2.
    #[test]
    fn s38_ctrl_digits_stand_down_while_text_input_focused() {
        for d in '1'..='9' {
            assert!(
                !should_intercept_app_key(&d.to_string(), true),
                "ctrl+{d} must stand down while a text input is focused"
            );
        }
    }

    /// Ctrl+E / Ctrl+W stay intercepted even while a text input is focused:
    /// passing them through hands them to WebView2's browser accelerators
    /// (Ctrl+W closes the window). Consuming is safe: the JS hotkey
    /// dispatcher skips them while an input is focused.
    #[test]
    fn s38_ctrl_e_w_stay_intercepted_while_text_input_focused() {
        assert!(should_intercept_app_key("e", true));
        assert!(should_intercept_app_key("w", true));
    }

    // -----------------------------------------------------------------
    // Off-runtime dispatch contract: ping_hosts / music_* commands
    // -----------------------------------------------------------------
    //
    // `ping_hosts` and the `music_*` commands block (ping subprocess up to ~2s;
    // WinRT SMTC `.get()`). They MUST run their blocking work via
    // `spawn_blocking` so they never stall the main event loop / occupy tokio
    // runtime threads. These tests pin the two halves of that contract:
    //   1) the work actually runs on a *different* thread than the awaiting one
    //      (i.e. it was moved off the runtime, not run inline), and
    //   2) the `.await.map_err(..)?` join-flatten preserves the inner Result
    //      value (Ok payload and Err message), guarding the `?` flatten used
    //      by every music command and the ping command.

    /// The blocking closure runs on a worker thread, not the awaiting thread.
    /// Reproduces the bug class: if someone reverts to calling the inner fn
    /// inline (no `spawn_blocking`), the work would run on the caller thread and
    /// this assertion would fail.
    #[test]
    fn spawn_blocking_moves_work_off_the_awaiting_thread() {
        let awaiting = std::thread::current().id();
        let worker = tauri::async_runtime::block_on(async move {
            tauri::async_runtime::spawn_blocking(|| std::thread::current().id())
                .await
                .expect("blocking task must join cleanly")
        });
        assert_ne!(
            awaiting, worker,
            "blocking work must run off the awaiting/runtime thread"
        );
    }

    /// The `.await.map_err(|e| e.to_string())?` join-flatten used by the ping
    /// and music command wrappers must surface the inner Result unchanged:
    /// both the Ok payload and an Err message propagate verbatim.
    #[test]
    fn spawn_blocking_join_flatten_preserves_inner_result() {
        // Mirrors the exact wrapper shape: spawn_blocking returning a
        // Result<T, String>, then `.await.map_err(..)?` to flatten the join.
        async fn run(inner: Result<i32, String>) -> Result<i32, String> {
            tauri::async_runtime::spawn_blocking(move || inner)
                .await
                .map_err(|e| e.to_string())?
        }

        let ok = tauri::async_runtime::block_on(run(Ok(7)));
        assert_eq!(ok, Ok(7), "Ok payload must pass through the join-flatten");

        let err = tauri::async_runtime::block_on(run(Err("boom".to_string())));
        assert_eq!(
            err,
            Err("boom".to_string()),
            "inner Err message must pass through unchanged (not swallowed)"
        );
    }

    // -----------------------------------------------------------------
    // compute_rect: percentage->pixel mapping for App Layouts.
    //
    // Pure: maps (xPct,yPct,wPct,hPct) within a monitor's work area to an
    // absolute (x,y,w,h) rect. Must (a) offset by the monitor origin,
    // (b) clamp every percent into [0,100], (c) floor width/height to the
    // 100x80 minimums, and (d) never panic on adversarial floats (NaN,
    // +/-inf, negatives, huge values).
    // -----------------------------------------------------------------

    /// Full-monitor pane (0,0,100,100) maps to the monitor's exact work area,
    /// including a non-zero origin (a secondary monitor to the right).
    #[test]
    fn compute_rect_full_pane_covers_work_area() {
        let mon = mi("\\\\.\\DISPLAY2", 1920, 0, 1920, 1080);
        let (x, y, w, h) = compute_rect(&mon, 0.0, 0.0, 100.0, 100.0);
        assert_eq!((x, y, w, h), (1920, 0, 1920, 1080));
    }

    /// A right-half pane (50% x, 0 y, 50% w, 100% h) on a primary monitor.
    /// Confirms the origin offset and the percentage→pixel rounding.
    #[test]
    fn compute_rect_right_half_pane() {
        let mon = mi("\\\\.\\DISPLAY1", 0, 0, 1920, 1080);
        let (x, y, w, h) = compute_rect(&mon, 50.0, 0.0, 50.0, 100.0);
        assert_eq!((x, y, w, h), (960, 0, 960, 1080));
    }

    /// The monitor origin is added to the x/y result, so a pane on a monitor
    /// with a negative origin (a display positioned left of primary) lands at
    /// the correct absolute coordinates.
    #[test]
    fn compute_rect_applies_negative_monitor_origin() {
        let mon = mi("\\\\.\\DISPLAY3", -1920, -200, 1920, 1080);
        let (x, y, _w, _h) = compute_rect(&mon, 0.0, 0.0, 50.0, 50.0);
        assert_eq!((x, y), (-1920, -200));
    }

    /// Percentages above 100 clamp to 100; the pane can't extend past the
    /// monitor's work area.
    #[test]
    fn compute_rect_clamps_over_100_percent() {
        let mon = mi("\\\\.\\DISPLAY1", 0, 0, 1000, 1000);
        let (x, y, w, h) = compute_rect(&mon, 150.0, 200.0, 300.0, 500.0);
        // x/y clamp to 100% → origin + full extent; w/h clamp to 100% → full size.
        assert_eq!((x, y, w, h), (1000, 1000, 1000, 1000));
    }

    /// Negative percentages clamp to 0. x/y collapse to the origin; w/h fall
    /// to the enforced minimum size rather than going negative or zero.
    #[test]
    fn compute_rect_clamps_negative_percent_to_min_size() {
        let mon = mi("\\\\.\\DISPLAY1", 100, 50, 1920, 1080);
        let (x, y, w, h) = compute_rect(&mon, -10.0, -10.0, -10.0, -10.0);
        assert_eq!((x, y), (100, 50), "negative x/y clamp to monitor origin");
        assert_eq!((w, h), (100, 80), "negative w/h floor to the 100x80 minimum");
    }

    /// A tiny but positive size still floors to the 100x80 minimums so the
    /// launched window is never unusably small.
    #[test]
    fn compute_rect_enforces_minimum_window_size() {
        let mon = mi("\\\\.\\DISPLAY1", 0, 0, 1920, 1080);
        let (_x, _y, w, h) = compute_rect(&mon, 0.0, 0.0, 1.0, 1.0);
        // 1% of 1920 = 19.2 → 19, below the 100 floor; 1% of 1080 = 10.8 → 11, below 80.
        assert_eq!((w, h), (100, 80));
    }

    /// Adversarial float inputs must not panic. NaN clamps to 0 (f32::max
    /// returns the non-NaN operand), +inf clamps to 100, -inf clamps to 0.
    #[test]
    fn compute_rect_handles_nan_and_infinity_without_panic() {
        let mon = mi("\\\\.\\DISPLAY1", 0, 0, 1920, 1080);

        // NaN everywhere → x/y at origin, w/h at the minimum floor.
        let (x, y, w, h) = compute_rect(&mon, f32::NAN, f32::NAN, f32::NAN, f32::NAN);
        assert_eq!((x, y, w, h), (0, 0, 100, 80));

        // +inf → clamps to 100% (full monitor); -inf → clamps to 0%.
        let (x2, y2, w2, h2) =
            compute_rect(&mon, f32::INFINITY, f32::NEG_INFINITY, f32::INFINITY, f32::NEG_INFINITY);
        assert_eq!((x2, y2), (1920, 0));
        assert_eq!((w2, h2), (1920, 80));
    }

    // -----------------------------------------------------------------
    // Logging pipeline: [LOG-NN] in SPEC.md
    // -----------------------------------------------------------------

    /// [LOG-01] The log dir must stay bounded. KeepAll once accumulated 628
    /// rotated segments / 2.8 GB on a real machine; rotation must keep a
    /// small fixed number of segments instead.
    #[test]
    fn log01_rotation_keeps_bounded_number_of_segments() {
        match log_rotation_strategy() {
            tauri_plugin_log::RotationStrategy::KeepSome(n) => {
                assert!((1..=50).contains(&n), "cap must be small and non-zero, got {n}");
            }
            _ => panic!("rotation must be KeepSome(n); KeepAll grows the log dir unbounded"),
        }
    }

    /// [LOG-02] WARN+ always forwards to the webview buffers, with or
    /// without an open Logs viewer: the Logs page must show warnings and
    /// errors that happened before it was opened.
    #[test]
    fn log02_webview_forwards_warn_plus_always() {
        for viewers in [0usize, 1, 3] {
            assert!(webview_forward_allowed(log::Level::Error, viewers));
            assert!(webview_forward_allowed(log::Level::Warn, viewers));
        }
    }

    /// [LOG-02] INFO and below forward ONLY while a Settings > Logs page is
    /// open, otherwise every record fans out via eval to every monitor
    /// window and fills each window's in-memory buffer.
    #[test]
    fn log02_webview_gates_info_and_below_on_open_viewer() {
        for level in [log::Level::Info, log::Level::Debug, log::Level::Trace] {
            assert!(!webview_forward_allowed(level, 0), "{level} must not forward with no viewer");
            assert!(webview_forward_allowed(level, 1), "{level} must forward while a viewer is open");
        }
    }

    /// [LOG-02] The viewer counter saturates: an unbalanced close (a window
    /// killed before its close call lands) must never underflow and wrap to
    /// usize::MAX, which would pin forwarding on forever.
    #[test]
    fn log02_viewer_count_saturates_on_unbalanced_close() {
        assert_eq!(next_viewer_count(0, false), 0);
        assert_eq!(next_viewer_count(0, true), 1);
        assert_eq!(next_viewer_count(1, false), 0);
        assert_eq!(next_viewer_count(usize::MAX, true), usize::MAX);
    }

    /// [LOG-04] Master-switch truth table for everything BELOW the error
    /// floor: release builds keep WARN and below only while the dev toggle is
    /// on; debug builds always log (dev workflow and the e2e suite rely on
    /// records flowing without the toggle).
    #[test]
    fn log04_master_gate_requires_toggle_below_error() {
        for level in [
            log::Level::Warn,
            log::Level::Info,
            log::Level::Debug,
            log::Level::Trace,
        ] {
            assert!(
                !logging_allowed(level, false, false),
                "release + toggle off must drop {level}"
            );
            assert!(
                logging_allowed(level, true, false),
                "release + toggle on must log {level}"
            );
            assert!(
                logging_allowed(level, false, true),
                "debug builds always log {level}"
            );
        }
    }

    /// [LOG-04] Error floor: ERROR is persisted on a release build with the
    /// toggle off. The panic hook logs at ERROR, and a crash that leaves no
    /// trace turns every bug report into "enable logging and reproduce it",
    /// which is exactly what a crash does not do on demand.
    #[test]
    fn log04_errors_log_without_the_toggle() {
        assert!(
            logging_allowed(log::Level::Error, false, false),
            "release + toggle off must still persist ERROR"
        );
        assert!(logging_allowed(log::Level::Error, true, false));
        assert!(logging_allowed(log::Level::Error, false, true));
    }

    /// [LOG-04] Marker roundtrip: the opt-in survives restarts so the gate is
    /// correct from the first record of the next boot (startup records,
    /// including setup panics, must persist for opted-in users).
    #[test]
    fn log04_marker_roundtrip_persists_opt_in() {
        let dir = std::env::temp_dir().join(format!("themia-log04-{}", std::process::id()));
        let path = dir.join("logging-enabled.marker");
        let _ = std::fs::remove_dir_all(&dir);

        assert!(!read_logging_marker(&path), "missing marker (fresh install) = logging off");
        write_logging_marker(&path, true);
        assert!(read_logging_marker(&path), "marker present = logging on at next boot");
        write_logging_marker(&path, false);
        assert!(!read_logging_marker(&path), "disable removes the marker");
        // Disabling again is idempotent, no panic on the missing file.
        write_logging_marker(&path, false);
        assert!(!read_logging_marker(&path));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// [LOG-04] The frontend pushes the persisted toggle through the
    /// `set_logging_enabled` command; the gate must follow it both ways.
    /// Uses the injectable apply fn with `None` so the test never touches the
    /// real %LOCALAPPDATA% marker of the machine running the tests.
    #[test]
    fn log04_set_logging_enabled_flips_the_gate() {
        use std::sync::atomic::Ordering;
        apply_logging_enabled(false, None);
        assert!(!LOGGING_ENABLED.load(Ordering::Relaxed));
        apply_logging_enabled(true, None);
        assert!(LOGGING_ENABLED.load(Ordering::Relaxed));
        apply_logging_enabled(false, None);
        assert!(!LOGGING_ENABLED.load(Ordering::Relaxed));
    }

    /// [LOG-06] Regression for issue #85 ("logging does not show any entry
    /// even when enabled"): release builds capped the whole dispatch at INFO,
    /// so every `debugLog()` (55 frontend call sites, arriving as a
    /// `webview:…` target) and every `log::debug!` was dropped and an opted-in
    /// user's Logs page stayed empty. Our own DEBUG must survive the cap.
    #[test]
    fn log06_own_debug_records_survive_the_cap() {
        for target in [
            "webview:console.debug@http://localhost:5173/src/debugLog.ts:174:4",
            "drag_drop",
            "folder:icons",
            "analytics",
            "app",
            "widget_netproxy::winhttp",
            "themia_lib::game_mode",
        ] {
            assert!(
                log_target_allowed(target, log::Level::Debug),
                "{target} is our own code; its DEBUG records must reach the Logs page"
            );
        }
    }

    /// [LOG-06] The cap that pays for the above: a dependency's DEBUG firehose
    /// (8.5k `rustls` lines in one session against 2.5k of ours) must never
    /// reach the file or the Logs page, or it buries the app's own records and
    /// churns the [LOG-01] budget.
    #[test]
    fn log06_dependency_debug_is_capped() {
        for target in [
            "rustls::client::hs",
            "rustls::common_state",
            "reqwest::connect",
            "ureq::pool",
            "tao::platform_impl::platform::event_loop::runner",
            "tauri::manager",
            "wmi::connection",
        ] {
            assert!(
                !log_target_allowed(target, log::Level::Debug),
                "{target} is a dependency; its DEBUG noise must stay out"
            );
        }
    }

    /// [LOG-06] The cap is DEBUG-only: a dependency's INFO/WARN/ERROR is rare
    /// and is exactly what diagnosing a user machine needs.
    #[test]
    fn log06_dependency_info_and_above_still_logs() {
        for level in [log::Level::Info, log::Level::Warn, log::Level::Error] {
            assert!(
                log_target_allowed("rustls::client::hs", level),
                "dependency {level} must not be capped"
            );
        }
    }

    // -----------------------------------------------------------------
    // WebView2 browser args [WV-02]
    // -----------------------------------------------------------------

    /// [MI-08] A degraded resolution must reach the frontend FLAGGED, not
    /// disguised as an identity. `choose_monitor_id` already picks the ordinal
    /// when `DeviceID` is empty; what #123 needs is the second return value
    /// surviving onto the descriptor, because that flag is the only thing that
    /// tells `initForMonitor` "do not intern state under this key".
    #[test]
    fn mi08_descriptor_carries_the_degraded_identity_flag() {
        let ordinal = r"\\.\DISPLAY2";
        let (id, fell_back) = choose_monitor_id("", ordinal);
        assert!(fell_back, "guard: an empty DeviceID is the degraded case");

        let degraded: MonitorDescriptor = MonitorInfo {
            id,
            id_stable: !fell_back,
            device_name: ordinal.to_string(),
            primary: false,
            x: 0,
            y: 0,
            width: 1920,
            height: 1032,
        }
        .into();
        assert!(
            !degraded.id_stable,
            "a fallback id must never present to the frontend as a resolved identity"
        );

        let resolved: MonitorDescriptor =
            mi_id(DELL_ID, r"\\.\DISPLAY1", 0, 0, 2560, 1392).into();
        assert!(resolved.id_stable, "a real device-interface path IS an identity");
    }

    /// [WV-02] `--enable-low-end-device-mode` sets `base::SysInfo::IsLowEndDevice()`,
    /// and Blink derives its raster budgets from it: the GPU memory policy drops
    /// to `bytes_limit_when_visible = 96 MB` and `max_staging_buffer_usage_in_bytes`
    /// is quartered. Themia is the worst shape for that cap - one full-work-area
    /// transparent overlay per monitor, plus a `backdrop-filter` on every widget
    /// surface, each needing its own backdrop snapshot on top of the layer's
    /// tiles. Past the budget cc stops filling, so the widget background and its
    /// blur end at a raster-tile edge with the wallpaper showing through the rest,
    /// and the settings window paints its background in one tile column only
    /// (themia-releases#124, #43, #108). Device-dependent purely because the cap
    /// is fixed while the layer size is not, which is why it never reproduces on
    /// a 1x 1080p dev box.
    #[test]
    fn wv02_browser_args_never_declare_a_low_end_device() {
        for banned in ["low-end-device", "force-gpu-mem-available-mb", "disable-gpu-rasterization"] {
            assert!(
                !WEBVIEW_BROWSER_ARGS.contains(banned),
                "{banned} starves the raster budget; widget fills and blur then                  stop at a tile edge (themia-releases#124)"
            );
        }
        // Guard: the args we DO depend on are still in the string, so a future
        // edit can't "fix" this test by emptying it.
        for kept in [
            "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection",
            "--js-flags=--max-old-space-size=192",
            "--autoplay-policy=no-user-gesture-required",
        ] {
            assert!(WEBVIEW_BROWSER_ARGS.contains(kept), "{kept} must stay");
        }
    }
}
