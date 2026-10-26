#![windows_subsystem = "windows"]

use nowbar::activities::claude::{card_can_answer, decision_json};
use nowbar::activities::live::{claude_msg, parse_push};
use nowbar::app::App;
use nowbar::i18n::{tr, trf};
use nowbar::platform::ipc::{self, Command};
use nowbar::platform::autostart::{autostart_enabled, set_autostart};
use nowbar::platform::log::{log, log_path};
use nowbar::platform::windows::take_events;
use windows::core::{w, Result};
use windows::UI::Composition::Compositor;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::System::WinRT::{
    CreateDispatcherQueueController, DispatcherQueueOptions, RoInitialize, DQTAT_COM_NONE, DQTYPE_THREAD_CURRENT,
    RO_INIT_SINGLETHREADED,
};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, GetMessageW, TranslateMessage, MSG};

/// First run turns "start with Windows" on; later runs keep the Run key pointing at this exe
/// (so moving the app keeps autostart working), unless the user switched it off.
fn autostart_on_start() {
    let marker = log_path().with_file_name("installed");
    let first_run = !marker.exists();
    let wanted = nowbar::settings::current().autostart;
    if !wanted && autostart_enabled() {
        let _ = set_autostart(false);
    }
    if wanted && (first_run || autostart_enabled()) {
        if let Err(e) = set_autostart(true) {
            log(&format!("autostart: {e:?}"));
        }
    }
    if first_run {
        let _ = std::fs::write(&marker, b"");
    }
}

fn run() -> Result<()> {
    unsafe {
        // The manifest already asks for Per-Monitor v2; this only matters if it is missing,
        // and fails with "access denied" when it isn't.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        RoInitialize(RO_INIT_SINGLETHREADED)?;
    }
    if let Err(e) = nowbar::platform::dragdrop::init() {
        log(&format!("shelf: no OLE ({e})"));
    }
    let _queue = unsafe {
        CreateDispatcherQueueController(DispatcherQueueOptions {
            dwSize: std::mem::size_of::<DispatcherQueueOptions>() as u32,
            threadType: DQTYPE_THREAD_CURRENT,
            apartmentType: DQTAT_COM_NONE,
        })?
    };
    nowbar::settings::install(nowbar::settings::load());
    autostart_on_start();
    let compositor = Compositor::new()?;
    // Explorer restarting at logon or after an update, or a GPU driver mid-update: try for two
    // minutes before giving up, instead of leaving no pill until the next sign-in.
    let mut tries = 0;
    let mut app = loop {
        match App::new(&compositor) {
            Ok(app) => break app,
            Err(e) if tries < 120 => {
                if tries == 0 {
                    log(&format!("start failed ({e}), retrying"));
                }
                tries += 1;
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            Err(e) => return Err(e),
        }
    };
    if tries > 0 {
        log(&format!("started after {tries} retries"));
    }
    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
            // Until empty: a handler's modal loop (a drag, a menu) queues more meanwhile.
            loop {
                let events = take_events();
                if events.is_empty() {
                    break;
                }
                for e in events {
                    app.handle(e);
                }
            }
        }
    }
    Ok(())
}

/// `nowbar.exe push …`, `claude-hook` (Claude Code's hook JSON on stdin) and `update` hand one
/// command to the running Now Bar and
/// end: for the scripts and hooks that run them they never take the mutex, never show anything
/// and always exit 0. Whether this run was one.
fn forward(args: &[String]) -> bool {
    let command = match args.get(1).map(String::as_str) {
        Some("push") => parse_push(&args[2..]).map(|m| Command::Live(Box::new(m))),
        Some("claude-hook") => {
            claude_hook(args.iter().any(|a| a == "--ask"));
            return true;
        }
        Some("update") => Some(Command::Update),
        Some("claude-hooks") => {
            claude_hooks_command(args.get(2).map(String::as_str));
            return true;
        }
        // Bare, with something piped in: a Claude Code hook from a client that dropped its
        // `args` (an older Claude Code, another tool reading ~/.claude/settings.json). Never a
        // person starting the app: without this, every tool call opened the settings window.
        None if stdin_is_pipe() => {
            claude_hook(false);
            return true;
        }
        _ => return false,
    };
    if let Some(c) = command {
        ipc::send(&c);
    }
    true
}

/// Whether stdin is a pipe: a hook or a script feeding the exe, not a person starting it (a
/// shortcut or Explorer gives a GUI exe no stdin at all).
fn stdin_is_pipe() -> bool {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Storage::FileSystem::{GetFileType, FILE_TYPE_PIPE};
    let h = std::io::stdin().as_raw_handle();
    !h.is_null() && unsafe { GetFileType(windows::Win32::Foundation::HANDLE(h)) } == FILE_TYPE_PIPE
}

/// `nowbar.exe claude-hooks install | remove`: Claude Code's hooks for this exe, said in a box
/// (the exe has no console).
fn claude_hooks_command(what: Option<&str>) {
    use nowbar::claude_hooks::{install, remove, rewrite};
    // Run from a shell, before any settings are installed: the language they pick.
    nowbar::i18n::set_language(nowbar::i18n::chosen(nowbar::settings::load().language));
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let (done, text) = match what {
        Some("install") => (rewrite(|v| install(v, &exe)), tr("Claude Code hook'ları kuruldu. Yeni oturumlarda pilde görünür.")),
        Some("remove") => (rewrite(|v| Some(remove(v))), tr("Claude Code hook'ları kaldırıldı.")),
        _ => (Ok(true), tr("Kullanım: nowbar.exe claude-hooks install | remove")),
    };
    let text = match done {
        Err(e) => trf("Claude Code ayarları yazılamadı: {}", &[&e]),
        // Only install declines: its "hooks" isn't an object.
        Ok(false) => tr("Claude Code ayarlarındaki \"hooks\" alanı tanınmadı; dosyaya dokunulmadı.").to_string(),
        Ok(true) => text.to_string(),
    };
    message_box(&text);
}

fn message_box(text: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONINFORMATION, MB_OK};
    unsafe {
        MessageBoxW(None, &windows::core::HSTRING::from(text), w!("Now Bar"), MB_OK | MB_ICONINFORMATION);
    }
}

/// One Claude Code hook call. UserPromptSubmit notes the window the prompt was typed in; with
/// `--ask` (the synchronous PermissionRequest hook) it waits for the card's answer as long as the
/// Now Bar says, prints Claude Code's decision, or lets Claude's own dialog take over.
fn claude_hook(asks: bool) {
    use std::io::{Read, Write};
    // A hook's JSON is small; a runaway one is cut, not waited on.
    let mut text = String::new();
    let _ = std::io::stdin().take(1 << 20).read_to_string(&mut text);
    let Ok(mut hook) = serde_json::from_str::<serde_json::Value>(&text) else { return };
    let event = hook["hook_event_name"].as_str().unwrap_or("").to_string();
    if event == "UserPromptSubmit" {
        let window = unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() };
        hook["nowbar_window"] = serde_json::json!(window.0 as i64);
    }
    // Only a permission the card shows in full gets an id (the card's buttons): a question, a
    // plan, a cut or many-line command stay with Claude's own dialog.
    let (tool, input) = (hook["tool_name"].as_str().unwrap_or("").to_string(), hook["tool_input"].clone());
    let answerable = asks && event == "PermissionRequest" && card_can_answer(&tool, &input);
    // Without a secret no card can answer: the ask stays with Claude's own dialog.
    let ask = answerable
        .then(nonce)
        .flatten()
        .map(|nonce| (format!("{}-{}", std::process::id(), unsafe { windows::Win32::System::SystemInformation::GetTickCount64() }), nonce));
    // The window comes first, so a quick click on the card always finds it.
    let window = ask.as_ref().and_then(|(id, nonce)| nowbar::platform::ask::AskWindow::create(id, nonce));
    if let (Some((id, nonce)), Some(_)) = (&ask, &window) {
        hook["nowbar_ask_id"] = serde_json::json!(id);
        hook["nowbar_ask_nonce"] = serde_json::json!(nonce);
    }
    let Some(msg) = claude_msg(&hook) else { return };
    let command = ipc::fitted(Command::Live(Box::new(msg)));
    let answer = ipc::send_answering(w!("NowBarMain"), windows::core::PCWSTR::null(), &command);
    if let (Some((id, _)), Some(window), Some(secs)) = (ask, window, answer.filter(|a| *a >= 2)) {
        match window.wait(secs as u32) {
            Some(allow) => {
                let mut out = std::io::stdout();
                let _ = out.write_all(decision_json(allow).as_bytes());
                let _ = out.flush();
            }
            None => {
                ipc::send(&Command::AskGone { id });
            }
        }
    }
}

#[link(name = "bcrypt")]
extern "system" {
    fn BCryptGenRandom(algorithm: *mut std::ffi::c_void, buffer: *mut u8, len: u32, flags: u32) -> i32;
}

/// 128 bits from Windows' cryptographic random generator, as hex; `None` rather than a guessable
/// secret if it fails (it doesn't, on a working Windows).
fn nonce() -> Option<String> {
    const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 2;
    let mut bytes = [0u8; 16];
    let status = unsafe { BCryptGenRandom(std::ptr::null_mut(), bytes.as_mut_ptr(), bytes.len() as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    if status != 0 {
        log(&format!("claude-hook: no random bytes ({status:#x})"));
        return None;
    }
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn main() {
    if forward(&std::env::args().collect::<Vec<_>>()) {
        return;
    }
    // One instance: a second launch exits silently. Started by an update, it waits (up to 30 s)
    // for the old one to quit instead.
    let after_update = std::env::args().any(|a| a == "--after-update");
    let started = std::time::Instant::now();
    let _instance = loop {
        let mutex = unsafe { CreateMutexW(None, true, w!(r"Local\NowBar.SingleInstance")) };
        if unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
            break mutex;
        }
        if let Ok(h) = mutex {
            unsafe {
                let _ = CloseHandle(h);
            }
        }
        if !after_update {
            // Launched again while running: the settings are what one is looking for.
            if let Ok(exe) = std::env::current_exe() {
                let _ = std::process::Command::new(exe.with_file_name("nowbar-settings.exe")).spawn();
            }
            return;
        }
        if started.elapsed() > std::time::Duration::from_secs(30) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    };
    nowbar::platform::updater::cleanup_old();
    // After an update (or any new exe dropped in): the installed apps list shows this version.
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(std::path::Path::to_path_buf)) {
        nowbar::platform::uninstall_entry::refresh_version(&dir, nowbar::platform::updater::VERSION);
    }
    std::panic::set_hook(Box::new(|info| log(&format!("panic: {info}"))));
    let build = nowbar::platform::os::build();
    log(&format!("windows build {build:?} ({:?})", nowbar::platform::os::generation()));
    if build.is_some_and(|b| !nowbar::platform::os::supported(b)) {
        nowbar::i18n::set_language(nowbar::i18n::chosen(nowbar::settings::load().language));
        return message_box(tr("Now Bar, Windows 10 sürüm 1809 veya sonrasını gerektirir."));
    }
    // The app was moved: Claude's hooks still point at the old exe, which is gone.
    if let Ok(exe) = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()) {
        let moved = nowbar::claude_hooks::rewrite(|v| nowbar::claude_hooks::follow(v, &exe, |c| std::path::Path::new(c).exists()));
        if let Ok(true) = moved {
            log(&format!("claude hooks moved to {exe}"));
        }
    }
    if let Err(e) = run() {
        log(&format!("fatal: {e:?}"));
        message_box(&trf("Now Bar açılamadı: {}\nAyrıntılar: {}", &[&e, &log_path().display()]));
    }
    // An update quit the loop: start the new Now Bar (it waits for this one's mutex).
    nowbar::platform::updater::restart_after_update();
}
