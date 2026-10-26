//! Updates from GitHub Releases: whether the latest release is newer, its files and their
//! checksums, and the renames that swap the exes. The pure part; platform/updater.rs asks
//! GitHub, downloads and swaps.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// "owner/name" of the repository whose releases are the updates (asked without a token: it
/// answers once the repository is public); empty: none asked.
pub const REPO: &str = "hishamalwy/now-bar";

/// The files a release must carry: the two exes and their checksums.
pub const ASSETS: [&str; 4] = ["nowbar.exe", "nowbar-settings.exe", "SHA256SUMS.txt", SUMS_SIG];

/// SHA256SUMS.txt's signature (tools\sign-sums.ps1): ECDSA P-256, r then s, as hex.
pub const SUMS_SIG: &str = "SHA256SUMS.txt.sig";

/// The P-256 public key (X then Y, as hex) whose signature on SHA256SUMS.txt an update must carry
/// (tools\make-update-key.ps1 makes the pair). Empty: this build swaps no exes itself; an update
/// opens the release page and is installed with the setup, by hand.
pub const UPDATE_KEY: &str = "0f7e178a45b480a846b6b3652ad5e0e84b7a15a8dfaea02490bc256a18dc77628d6d05035e5712aa4bd727aa8700151178912228b4b6eacddd0cd3be6e49c87d";

/// 64 bytes from hex (a public key or a signature); `None` for anything else.
pub fn hex64(text: &str) -> Option<[u8; 64]> {
    let text = text.trim();
    if text.len() != 128 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 64];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// Whether this build may install `r` itself: it has a key, and the release a signature.
pub fn installs_itself(r: &Release, key: &str) -> bool {
    hex64(key).is_some() && r.asset(SUMS_SIG).is_some()
}

pub type Version = (u32, u32, u32);

pub fn latest_url(repo: &str) -> String {
    format!("https://api.github.com/repos/{repo}/releases/latest")
}

/// "v0.3.0" or "0.3.0"; `None` for anything else, a pre-release suffix included.
pub fn parse_version(tag: &str) -> Option<Version> {
    let mut parts = tag.strip_prefix('v').unwrap_or(tag).split('.').map(|p| p.parse::<u32>().ok());
    let version = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(version)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    pub tag: String,
    /// The release's page (opened when the exes can't be swapped).
    pub page: String,
    /// (file name, download URL).
    pub assets: Vec<(String, String)>,
}

impl Release {
    pub fn asset(&self, name: &str) -> Option<&str> {
        self.assets.iter().find(|(n, _)| n == name).map(|(_, url)| url.as_str())
    }
}

/// GitHub's latest-release answer; `None` for a draft, a pre-release or an error answer.
pub fn parse_release(v: &Value) -> Option<Release> {
    if v["draft"].as_bool() != Some(false) || v["prerelease"].as_bool() != Some(false) {
        return None;
    }
    let tag = v["tag_name"].as_str()?;
    let assets = v["assets"]
        .as_array()?
        .iter()
        .filter_map(|a| Some((a["name"].as_str()?.to_string(), a["browser_download_url"].as_str()?.to_string())))
        .collect();
    Some(Release { version: parse_version(tag)?, tag: tag.to_string(), page: v["html_url"].as_str()?.to_string(), assets })
}

/// Whether `r` is newer than the running `current` ("0.2.0").
pub fn is_newer(r: &Release, current: &str) -> bool {
    parse_version(current).is_some_and(|now| r.version > now)
}

/// Whether the version `latest` ("0.3.0", as update.json keeps it) is newer than `current`.
pub fn newer(latest: &str, current: &str) -> bool {
    matches!((parse_version(latest), parse_version(current)), (Some(l), Some(c)) if l > c)
}

/// The Hakkında page's update line, and whether its row installs.
pub fn update_line(state: Option<&UpdateState>, current: &str) -> (String, bool) {
    match state {
        None => (crate::i18n::trf("{} · denetlenmedi", &[&current]), false),
        Some(s) if newer(&s.latest, current) => (crate::i18n::trf("{} hazır · şimdiki {}", &[&s.latest, &current]), true),
        Some(_) => (crate::i18n::trf("{} · güncel", &[&current]), false),
    }
}

/// `sha256sum`'s lines ("<hex>  <name>" or "<hex> *<name>"): (name, lowercase hex).
pub fn parse_sums(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let (hex, name) = line.trim().split_once(char::is_whitespace)?;
            let name = name.trim_start().trim_start_matches('*');
            (hex.chars().all(|c| c.is_ascii_hexdigit()) && !name.is_empty()).then(|| (name.to_string(), hex.to_ascii_lowercase()))
        })
        .collect()
}

/// The renames, in order, that put the exes staged in `staged` in place of the ones in `dir`:
/// the running ones step aside first (a running exe can be renamed, not overwritten).
pub fn swap_plan(dir: &Path, staged: &Path) -> Vec<(PathBuf, PathBuf)> {
    let exes = ["nowbar", "nowbar-settings"];
    let aside = exes.iter().map(|e| (dir.join(format!("{e}.exe")), dir.join(format!("{e}.old.exe"))));
    let new_in = exes.iter().map(|e| (staged.join(format!("{e}.exe")), dir.join(format!("{e}.exe"))));
    aside.chain(new_in).collect()
}

/// What undoes the renames `done` (the start of a `swap_plan`), in order.
pub fn rollback(done: &[(PathBuf, PathBuf)]) -> Vec<(PathBuf, PathBuf)> {
    done.iter().rev().map(|(from, to)| (to.clone(), from.clone())).collect()
}

/// `%LOCALAPPDATA%\NowBar\update.json`: what the last check found, for the settings window.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateState {
    /// The latest release's version ("0.3.0").
    pub latest: String,
    pub page: String,
    /// When it was asked (unix seconds).
    pub checked: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_as_numbers() {
        assert_eq!(parse_version("v0.10.0"), Some((0, 10, 0)));
        assert_eq!(parse_version("0.2.0"), Some((0, 2, 0)));
        assert_eq!(parse_version("v0.3.0-beta"), None);
        assert_eq!(parse_version("latest"), None);
        assert_eq!(parse_version("v1.2"), None);
        let r = Release { version: (0, 10, 0), tag: "v0.10.0".into(), page: String::new(), assets: Vec::new() };
        assert!(is_newer(&r, "0.9.3"));
        assert!(!is_newer(&r, "0.10.0"));
        assert!(!is_newer(&r, "1.0.0"));
        assert!(!is_newer(&r, "garbage"), "an unreadable current version never updates");
        assert!(newer("0.3.0", "0.2.0"));
        assert!(!newer("0.2.0", "0.2.0"));
        assert!(!newer("", "0.2.0"), "nothing found yet");
    }

    fn answer() -> Value {
        serde_json::json!({
            "tag_name": "v0.3.0", "html_url": "https://github.com/k/nowbar/releases/tag/v0.3.0",
            "draft": false, "prerelease": false,
            "assets": [
                {"name": "nowbar.exe", "browser_download_url": "https://github.com/k/nowbar/releases/download/v0.3.0/nowbar.exe", "size": 9000000},
                {"name": "SHA256SUMS.txt", "browser_download_url": "https://github.com/k/nowbar/releases/download/v0.3.0/SHA256SUMS.txt"}
            ]
        })
    }

    #[test]
    fn keys_and_signatures_are_64_bytes_of_hex() {
        let hex = "00ff".repeat(32);
        assert_eq!(hex64(&hex).map(|b| (b[0], b[1], b[63])), Some((0x00, 0xFF, 0xFF)));
        assert!(hex64(&format!("{hex}\n")).is_some(), "a trailing newline is fine");
        assert_eq!(hex64(&hex[2..]), None, "too short");
        assert_eq!(hex64(&format!("{hex}00")), None, "too long");
        assert_eq!(hex64(&"zz".repeat(64)), None);
        assert_eq!(hex64(""), None);
    }

    #[test]
    fn only_a_keyed_build_and_a_signed_release_install_themselves() {
        let key = "ab".repeat(64);
        let unsigned = parse_release(&answer()).unwrap();
        let mut signed = unsigned.clone();
        signed.assets.push((SUMS_SIG.into(), "https://github.com/k/nowbar/releases/download/v0.3.0/SHA256SUMS.txt.sig".into()));
        assert!(installs_itself(&signed, &key));
        assert!(!installs_itself(&unsigned, &key), "no signature: the release page");
        assert!(!installs_itself(&signed, ""), "no key in this build: the release page");
    }

    #[test]
    fn release_reads_tag_page_and_assets() {
        let r = parse_release(&answer()).unwrap();
        assert_eq!((r.version, r.tag.as_str()), ((0, 3, 0), "v0.3.0"));
        assert_eq!(r.page, "https://github.com/k/nowbar/releases/tag/v0.3.0");
        assert_eq!(r.asset("nowbar.exe"), Some("https://github.com/k/nowbar/releases/download/v0.3.0/nowbar.exe"));
        assert_eq!(r.asset("nowbar-settings.exe"), None);
        let mut draft = answer();
        draft["draft"] = true.into();
        assert_eq!(parse_release(&draft), None);
        let mut pre = answer();
        pre["prerelease"] = true.into();
        assert_eq!(parse_release(&pre), None);
        assert_eq!(parse_release(&serde_json::json!({"message": "Not Found", "status": "404"})), None);
        assert_eq!(latest_url("k/nowbar"), "https://api.github.com/repos/k/nowbar/releases/latest");
    }

    #[test]
    fn the_about_line_says_where_it_stands() {
        let state = |latest: &str| UpdateState { latest: latest.into(), page: String::new(), checked: 1 };
        assert_eq!(update_line(None, "0.2.0"), ("0.2.0 · denetlenmedi".to_string(), false));
        assert_eq!(update_line(Some(&state("0.2.0")), "0.2.0"), ("0.2.0 · güncel".to_string(), false));
        assert_eq!(update_line(Some(&state("0.3.0")), "0.2.0"), ("0.3.0 hazır · şimdiki 0.2.0".to_string(), true));
    }

    #[test]
    fn sums_read_both_formats() {
        let text = "AB12cd  nowbar.exe\r\nef34 *nowbar-settings.exe\n\nnot a line\n";
        assert_eq!(parse_sums(text), vec![("nowbar.exe".into(), "ab12cd".into()), ("nowbar-settings.exe".into(), "ef34".into())]);
    }

    #[test]
    fn swap_moves_old_aside_then_new_in() {
        let (dir, staged) = (Path::new("C:/app"), Path::new("C:/stage"));
        assert_eq!(
            swap_plan(dir, staged),
            vec![
                (dir.join("nowbar.exe"), dir.join("nowbar.old.exe")),
                (dir.join("nowbar-settings.exe"), dir.join("nowbar-settings.old.exe")),
                (staged.join("nowbar.exe"), dir.join("nowbar.exe")),
                (staged.join("nowbar-settings.exe"), dir.join("nowbar-settings.exe")),
            ]
        );
    }

    #[test]
    fn rollback_undoes_in_reverse() {
        let plan = swap_plan(Path::new("C:/app"), Path::new("C:/stage"));
        let undo = rollback(&plan[..3]);
        assert_eq!(undo, vec![(plan[2].1.clone(), plan[2].0.clone()), (plan[1].1.clone(), plan[1].0.clone()), (plan[0].1.clone(), plan[0].0.clone())]);
        assert!(rollback(&[]).is_empty());
    }
}
