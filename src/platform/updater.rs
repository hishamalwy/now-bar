//! The update thread: asks GitHub for the latest release once a day, and on demand downloads it,
//! checks SHA256SUMS.txt's signature against the key built in (UPDATE_KEY) and the exes against
//! SHA256SUMS.txt, swaps them in and starts the new Now Bar. A release it can't verify that way
//! (unsigned, or this build has no key) opens its page instead: the setup installs it by hand.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::core::{w, HSTRING};
use windows::Win32::Security::Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use super::http::get_bytes;
use super::log::{log, log_path};
use crate::update::{
    hex64, installs_itself, is_newer, latest_url, parse_release, parse_sums, rollback, swap_plan, Release, UpdateState, ASSETS, REPO, SUMS_SIG, UPDATE_KEY,
};

/// The running version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The first ask waits for the start to settle; then one a day (GitHub allows 60 an hour).
const FIRST: Duration = Duration::from_secs(60);
const EVERY: Duration = Duration::from_secs(24 * 3600);
const EXES: [&str; 2] = ["nowbar.exe", "nowbar-settings.exe"];

/// What the thread tells the app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Progress {
    /// The latest release (newer or not).
    Found(UpdateState),
    /// The new exes are in place: this one quits the normal way, then main starts the new one.
    Installed,
    Failed(String),
}

pub type UpdateMailbox = Arc<Mutex<Vec<Progress>>>;

enum Msg {
    Apply,
    ApplyFrom(PathBuf),
    Enable(bool),
}

pub struct Updater {
    tx: Sender<Msg>,
}

impl Updater {
    /// `post` runs on the thread after each `Progress` (the mailbox then holds it).
    pub fn start(mailbox: UpdateMailbox, post: impl Fn() + Send + 'static) -> Updater {
        let (tx, rx) = channel::<Msg>();
        std::thread::Builder::new()
            .name("update".into())
            .spawn(move || {
                let report = |p: Progress| {
                    if let Ok(mut m) = mailbox.lock() {
                        m.push(p);
                    }
                    post();
                };
                let mut enabled = crate::settings::current().update_check;
                let mut found: Option<Release> = None;
                let mut next = Instant::now() + FIRST;
                loop {
                    match rx.recv_timeout(next.saturating_duration_since(Instant::now())) {
                        Ok(Msg::Enable(on)) => {
                            if on && !enabled {
                                next = Instant::now() + FIRST;
                            }
                            enabled = on;
                        }
                        Ok(Msg::Apply) => {
                            if found.is_none() {
                                found = check().map(|(r, _)| r);
                            }
                            let result = match found.as_ref().filter(|r| is_newer(r, VERSION)) {
                                Some(r) if installs_itself(r, UPDATE_KEY) => download(r).and_then(|dir| install(&dir, &r.page)),
                                // Nothing unverified is swapped in: the user installs it with the setup.
                                Some(r) => {
                                    open_page(&r.page);
                                    Err(format!("{} isn't signed with this build's key: opened its page", r.tag))
                                }
                                None => Err("no newer release".into()),
                            };
                            report(result.map_or_else(Progress::Failed, |()| Progress::Installed));
                        }
                        Ok(Msg::ApplyFrom(dir)) => report(verify(&dir).and_then(|()| install(&dir, "")).map_or_else(Progress::Failed, |()| Progress::Installed)),
                        Err(RecvTimeoutError::Timeout) => {
                            if enabled && !REPO.is_empty() {
                                if let Some((r, state)) = check() {
                                    found = Some(r);
                                    report(Progress::Found(state));
                                }
                            }
                            next = Instant::now() + EVERY;
                        }
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
            })
            .expect("spawn update thread");
        Updater { tx }
    }

    /// Download, check and install the newer release (asks GitHub first if it hasn't yet).
    pub fn apply(&self) {
        let _ = self.tx.send(Msg::Apply);
    }

    /// Dev: install the exes already in `dir` (with their SHA256SUMS.txt), no network.
    pub fn apply_from(&self, dir: PathBuf) {
        let _ = self.tx.send(Msg::ApplyFrom(dir));
    }

    pub fn set_enabled(&self, on: bool) {
        let _ = self.tx.send(Msg::Enable(on));
    }
}

/// `%LOCALAPPDATA%\NowBar\update.json`.
fn state_path() -> PathBuf {
    log_path().with_file_name("update.json")
}

/// What the last check found (the settings window and the tray read it).
pub fn state() -> Option<UpdateState> {
    serde_json::from_slice(&std::fs::read(state_path()).ok()?).ok()
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Asks GitHub for the latest release and keeps the answer in update.json.
fn check() -> Option<(Release, UpdateState)> {
    let (status, body) = get_bytes(&latest_url(REPO), &[("Accept", "application/vnd.github+json")])
        .inspect_err(|e| log(&format!("update: {e}")))
        .ok()?;
    if status != 200 {
        log(&format!("update: GitHub answered {status}"));
        return None;
    }
    let r = parse_release(&serde_json::from_slice(&body).ok()?)?;
    let (a, b, c) = r.version;
    let state = UpdateState { latest: format!("{a}.{b}.{c}"), page: r.page.clone(), checked: unix_now() };
    let _ = std::fs::write(state_path(), serde_json::to_vec(&state).unwrap_or_default());
    log(&format!("update: latest {} (running {VERSION})", r.tag));
    Some((r, state))
}

/// The release's files into `%LOCALAPPDATA%\NowBar\update\<tag>\`, checked.
fn download(r: &Release) -> Result<PathBuf, String> {
    let dir = log_path().with_file_name("update").join(&r.tag);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for name in ASSETS {
        let url = r.asset(name).ok_or(format!("{} has no {name}", r.tag))?;
        let (status, bytes) = get_bytes(url, &[])?;
        if status != 200 {
            return Err(format!("{name}: HTTP {status}"));
        }
        std::fs::write(dir.join(name), bytes).map_err(|e| format!("{name}: {e}"))?;
    }
    verify(&dir).inspect_err(|_| {
        let _ = std::fs::remove_dir_all(&dir);
    })?;
    Ok(dir)
}

/// `dir`'s SHA256SUMS.txt carries the signature of the key built in, and both exes match it.
fn verify(dir: &Path) -> Result<(), String> {
    let sums_bytes = std::fs::read(dir.join("SHA256SUMS.txt")).map_err(|e| format!("SHA256SUMS.txt: {e}"))?;
    verify_signature(&sums_bytes, &std::fs::read_to_string(dir.join(SUMS_SIG)).map_err(|e| format!("{SUMS_SIG}: {e}"))?, UPDATE_KEY)?;
    let sums = parse_sums(&String::from_utf8_lossy(&sums_bytes));
    for exe in EXES {
        let want = sums.iter().find(|(n, _)| n == exe).map(|(_, h)| h).ok_or(format!("{exe} isn't in SHA256SUMS.txt"))?;
        let got = sha256_hex(&std::fs::read(dir.join(exe)).map_err(|e| format!("{exe}: {e}"))?);
        if &got != want {
            return Err(format!("{exe}: checksum {got}, expected {want}"));
        }
    }
    Ok(())
}

/// A rename, or a copy when the two are on different drives.
fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to).or_else(|_| std::fs::copy(from, to).map(|_| ()))
}

/// Renames done, as (from, to).
type Renames = Vec<(PathBuf, PathBuf)>;

/// A swap waiting for the app to quit: the folder and the renames, to undo if the new exe dies.
static PENDING: Mutex<Option<(PathBuf, Renames)>> = Mutex::new(None);

/// Swaps the exes in the running one's folder for those in `staged`; the new Now Bar starts once
/// this one has quit (`restart_after_update`). A folder it may not write (Program Files) opens
/// `page` instead.
fn install(staged: &Path, page: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("current exe: {e}"))?;
    let dir = exe.parent().ok_or("the exe has no folder")?;
    match swap(dir, staged) {
        Ok(done) => {
            if let Ok(mut p) = PENDING.lock() {
                *p = Some((dir.to_path_buf(), done));
            }
            log(&format!("update: installed from {}", staged.display()));
            Ok(())
        }
        Err((e, step)) => {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                open_page(page);
            }
            Err(format!("{step}: {e}"))
        }
    }
}

/// The release's page in the browser (GitHub's own address for it, nothing else).
fn open_page(page: &str) {
    if page.starts_with("https://github.com/") {
        unsafe {
            ShellExecuteW(None, w!("open"), &HSTRING::from(page), None, None, SW_SHOWNORMAL);
        }
    }
}

/// `signature` (hex, from SHA256SUMS.txt.sig) is `key`'s ECDSA P-256 signature of `sums`.
fn verify_signature(sums: &[u8], signature: &str, key: &str) -> Result<(), String> {
    let key = hex64(key).ok_or("this build has no update key")?;
    let signature = hex64(signature).ok_or(format!("{SUMS_SIG} isn't a signature"))?;
    let hash = sha256(sums).ok_or("SHA256SUMS.txt: no hash")?;
    if ecdsa_p256_verifies(&key, &hash, &signature) {
        Ok(())
    } else {
        Err("SHA256SUMS.txt isn't signed by this build's update key".into())
    }
}

#[link(name = "bcrypt")]
extern "system" {
    fn BCryptOpenAlgorithmProvider(algorithm: *mut *mut c_void, id: *const u16, implementation: *const u16, flags: u32) -> i32;
    fn BCryptCloseAlgorithmProvider(algorithm: *mut c_void, flags: u32) -> i32;
    fn BCryptImportKeyPair(algorithm: *mut c_void, import_key: *mut c_void, blob_type: *const u16, key: *mut *mut c_void, input: *const u8, input_len: u32, flags: u32) -> i32;
    fn BCryptDestroyKey(key: *mut c_void) -> i32;
    fn BCryptVerifySignature(key: *mut c_void, padding: *const c_void, hash: *const u8, hash_len: u32, signature: *const u8, signature_len: u32, flags: u32) -> i32;
}

/// Whether `signature` (r then s) is `key`'s (X then Y) ECDSA P-256 signature of the SHA-256
/// `hash`, by Windows' own BCrypt.
fn ecdsa_p256_verifies(key: &[u8; 64], hash: &[u8; 32], signature: &[u8; 64]) -> bool {
    // BCRYPT_ECCKEY_BLOB: BCRYPT_ECDSA_PUBLIC_P256_MAGIC ("ECS1"), the coordinates' length, X, Y.
    let mut blob = Vec::with_capacity(72);
    blob.extend_from_slice(&0x3153_4345u32.to_le_bytes());
    blob.extend_from_slice(&32u32.to_le_bytes());
    blob.extend_from_slice(key);
    unsafe {
        let mut algorithm = std::ptr::null_mut();
        if BCryptOpenAlgorithmProvider(&mut algorithm, w!("ECDSA_P256").0, std::ptr::null(), 0) != 0 {
            return false;
        }
        let mut handle = std::ptr::null_mut();
        let imported = BCryptImportKeyPair(algorithm, std::ptr::null_mut(), w!("ECCPUBLICBLOB").0, &mut handle, blob.as_ptr(), blob.len() as u32, 0) == 0;
        let verified = imported && BCryptVerifySignature(handle, std::ptr::null(), hash.as_ptr(), hash.len() as u32, signature.as_ptr(), signature.len() as u32, 0) == 0;
        if imported {
            BCryptDestroyKey(handle);
        }
        BCryptCloseAlgorithmProvider(algorithm, 0);
        verified
    }
}

fn undo(done: &[(PathBuf, PathBuf)]) {
    for (from, to) in rollback(done) {
        let _ = move_file(&from, &to);
    }
}

/// After the message loop of an update's quit (main calls it, still holding the mutex): starts the
/// new Now Bar and watches it 2 s. One that dies at once (blocked, broken) gets the old exes put
/// back, and the old Now Bar starts again in its place: never no Now Bar at all.
pub fn restart_after_update() {
    let Some((dir, done)) = PENDING.lock().ok().and_then(|mut p| p.take()) else { return };
    let exe = dir.join("nowbar.exe");
    if !start_and_watch(&exe, &["--after-update"], Duration::from_secs(2)) {
        log("update: the new Now Bar died at start; the old exes are back");
        undo(&done);
        let _ = std::process::Command::new(&exe).arg("--after-update").spawn();
    }
}

/// Puts the exes in `staged` in place of those in `dir`; on any failure the ones done are undone
/// and the old exes are back under their own names. The renames done, for undoing later.
fn swap(dir: &Path, staged: &Path) -> Result<Renames, (std::io::Error, String)> {
    let mut done = Vec::new();
    for (from, to) in swap_plan(dir, staged) {
        // The running exes step aside by renaming only (a copy of them would be no step aside);
        // the new ones may come from another drive.
        let moved = if from.parent() == Some(dir) { std::fs::rename(&from, &to) } else { move_file(&from, &to) };
        if let Err(e) = moved {
            undo(&done);
            return Err((e, format!("{} → {}", from.display(), to.display())));
        }
        done.push((from, to));
    }
    Ok(done)
}

/// Starts `exe` with `args`; whether it is still running after `watch` (a blocked or broken exe
/// dies at once).
fn start_and_watch(exe: &Path, args: &[&str], watch: Duration) -> bool {
    let Ok(mut child) = std::process::Command::new(exe).args(args).spawn() else { return false };
    let until = Instant::now() + watch;
    while Instant::now() < until {
        if child.try_wait().ok().flatten().is_some() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

/// The old exes a swap left beside the new ones (one may still be open: next time, then).
pub fn cleanup_old() {
    let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) else { return };
    for old in ["nowbar.old.exe", "nowbar-settings.old.exe"] {
        let _ = std::fs::remove_file(dir.join(old));
    }
}

/// The exe's SHA-256 as lowercase hex (Windows' own BCrypt).
pub fn sha256_hex(bytes: &[u8]) -> String {
    sha256(bytes).map(|out| out.iter().map(|b| format!("{b:02x}")).collect()).unwrap_or_default()
}

fn sha256(bytes: &[u8]) -> Option<[u8; 32]> {
    let mut out = [0u8; 32];
    let status = unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, bytes, &mut out) };
    status.is_ok().then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vector() {
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(sha256_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    }

    /// A fresh folder under the temp dir with `files` (name, contents).
    fn folder(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nowbar-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (f, text) in files {
            std::fs::write(dir.join(f), text).unwrap();
        }
        dir
    }

    fn read(p: PathBuf) -> String {
        std::fs::read_to_string(p).unwrap_or_default()
    }

    #[test]
    fn swap_puts_the_new_exes_in_and_the_old_aside() {
        let app = folder("app", &[("nowbar.exe", "old"), ("nowbar-settings.exe", "old settings")]);
        let staged = folder("staged", &[("nowbar.exe", "new"), ("nowbar-settings.exe", "new settings")]);
        assert!(swap(&app, &staged).is_ok());
        assert_eq!((read(app.join("nowbar.exe")), read(app.join("nowbar-settings.exe"))), ("new".into(), "new settings".into()));
        assert_eq!(read(app.join("nowbar.old.exe")), "old");
        let _ = (std::fs::remove_dir_all(&app), std::fs::remove_dir_all(&staged));
    }

    #[test]
    fn a_locked_exe_leaves_the_old_ones_in_place() {
        use std::os::windows::fs::OpenOptionsExt;
        let app = folder("locked", &[("nowbar.exe", "old"), ("nowbar-settings.exe", "old settings")]);
        let staged = folder("locked-staged", &[("nowbar.exe", "new"), ("nowbar-settings.exe", "new settings")]);
        // An open settings window of an older build: no sharing, so it can't be renamed.
        let lock = std::fs::OpenOptions::new().read(true).share_mode(0).open(app.join("nowbar-settings.exe")).unwrap();
        assert!(swap(&app, &staged).is_err());
        drop(lock);
        assert_eq!((read(app.join("nowbar.exe")), read(app.join("nowbar-settings.exe"))), ("old".into(), "old settings".into()), "rolled back");
        assert!(!app.join("nowbar.old.exe").exists());
        let _ = (std::fs::remove_dir_all(&app), std::fs::remove_dir_all(&staged));
    }

    #[test]
    fn start_and_watch_tells_a_quick_death() {
        let cmd = Path::new(r"C:\Windows\System32\cmd.exe");
        assert!(!start_and_watch(cmd, &["/c", "exit", "1"], Duration::from_millis(800)), "died at once");
        assert!(start_and_watch(cmd, &["/c", "ping -n 3 127.0.0.1 >nul"], Duration::from_millis(500)), "still running");
        assert!(!start_and_watch(Path::new(r"C:\no\such.exe"), &[], Duration::from_millis(100)), "didn't start");
    }

    /// A pair made by tools\make-update-key.ps1 and a signature of "abc" by tools\sign-sums.ps1:
    /// what .NET signs, BCrypt verifies.
    const TEST_KEY: &str = "943beba35c58187ce37c6497584ea898e79cf8eb4f50a6aa4918fae6888fd06924788c5010264dd3ae63843f8db8a6304fe39649a0e28f3ab9774d5763981d76";
    const TEST_SIG: &str = "7280807f4ed0d5bd62d5ac524cbbd41adc264955791267ae620aa3e28a151e8ff951ab6bbee1fb72b2138fb74628e2de949219bf102d55c20863f2825717f6d8";

    #[test]
    fn only_the_keys_signature_passes() {
        assert_eq!(verify_signature(b"abc", TEST_SIG, TEST_KEY), Ok(()));
        assert!(verify_signature(b"abd", TEST_SIG, TEST_KEY).is_err(), "another file");
        let mut forged = TEST_SIG.to_string();
        forged.replace_range(0..2, "00");
        assert!(verify_signature(b"abc", &forged, TEST_KEY).is_err(), "another signature");
        let other_key = format!("{}{}", &TEST_KEY[..64], "00".repeat(32));
        assert!(verify_signature(b"abc", TEST_SIG, &other_key).is_err(), "another key");
        assert!(verify_signature(b"abc", TEST_SIG, "").is_err(), "no key built in");
        assert!(verify_signature(b"abc", "", TEST_KEY).is_err(), "no signature");
    }

    /// Checks a release folder the way an update will (BCrypt against its SHA256SUMS.txt), before
    /// publishing it: `NOWBAR_DIST=<repo>\dist cargo test --lib verify_dist -- --ignored`.
    #[test]
    #[ignore]
    fn verify_dist() {
        let dir = std::env::var("NOWBAR_DIST").expect("NOWBAR_DIST=<folder>");
        verify(Path::new(&dir)).unwrap();
    }

    /// Asks GitHub for real: `cargo test --lib live_github -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_github() {
        if REPO.is_empty() {
            println!("REPO is empty: nothing to ask");
            return;
        }
        let (status, body) = get_bytes(&latest_url(REPO), &[("Accept", "application/vnd.github+json")]).unwrap();
        println!("HTTP {status}: {:?}", serde_json::from_slice(&body).ok().as_ref().and_then(parse_release));
    }
}
