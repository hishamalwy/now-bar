//! The network side of the live-match activity: follows the teams from settings, refreshes
//! their schedules every 6 h, polls each nearby match at the cadence its status asks for
//! (football::next_poll_s), caches crests, and posts every tracked match to the UI. No teams,
//! no requests. A replay mode plays recorded scoreboard files instead (`--match-replay <dir>`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::core::HSTRING;
use windows::Foundation::Uri;
use windows::Web::Http::Filters::{HttpBaseProtocolFilter, HttpCacheReadBehavior};
use windows::Web::Http::HttpClient;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

use super::football::{end_time, next_poll_s, parse_event, parse_schedule, parse_search, Fixture, MatchSnapshot, TeamHit};
use crate::platform::log::log;

const BASE: &str = "https://site.api.espn.com/apis/site/v2/sports/soccer";
const SCHEDULE_EVERY: Duration = Duration::from_secs(6 * 3600);
/// Matches this close (kickoff within a day ahead, or started up to 4 h ago) are polled.
const TRACK_AHEAD_S: i64 = 24 * 3600;
// Last night's match too, even when the PC was off: the morning brief shows its score.
const TRACK_BEHIND_S: i64 = 20 * 3600;

fn now_utc() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn now_ms() -> u64 {
    unsafe { windows::Win32::System::SystemInformation::GetTickCount64() }
}

/// The last scoreboard answer (GetTickCount64 ms), changed or not: what the live dot's
/// "stale" goes by (`post` only runs on a change).
pub static HEARD_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Where the JSON comes from: ESPN, or a folder of recorded scoreboards.
trait Source {
    fn get(&mut self, url: &str) -> Result<String, String>;
}

struct Espn {
    client: HttpClient,
}

impl Espn {
    fn new() -> windows::core::Result<Self> {
        let filter = HttpBaseProtocolFilter::new()?;
        // Live data must never come from the WinINet cache.
        filter.CacheControl()?.SetReadBehavior(HttpCacheReadBehavior::MostRecent)?;
        let client = HttpClient::Create(&filter)?;
        let _ = client.DefaultRequestHeaders()?.UserAgent()?.TryParseAdd(&HSTRING::from("Mozilla/5.0 (Windows NT 10.0; Win64; x64) NowBar"));
        Ok(Espn { client })
    }

    fn bytes(&self, url: &str) -> Result<Vec<u8>, String> {
        let uri = Uri::CreateUri(&HSTRING::from(url)).map_err(|e| format!("{e:?}"))?;
        let buffer = self.client.GetBufferAsync(&uri).and_then(|op| crate::join_within!(op, 20)).map_err(|e| format!("{e:?}"))?;
        let reader = windows::Storage::Streams::DataReader::FromBuffer(&buffer).map_err(|e| format!("{e:?}"))?;
        let mut out = vec![0u8; buffer.Length().unwrap_or(0) as usize];
        reader.ReadBytes(&mut out).map_err(|e| format!("{e:?}"))?;
        Ok(out)
    }
}

impl Source for Espn {
    fn get(&mut self, url: &str) -> Result<String, String> {
        let uri = Uri::CreateUri(&HSTRING::from(url)).map_err(|e| format!("{e:?}"))?;
        self.client.GetStringAsync(&uri).and_then(|op| crate::join_within!(op, 20)).map(|s| s.to_string()).map_err(|e| format!("{e:?}"))
    }
}

/// Recorded scoreboards of one match, played back one file per poll (sorted by name); the
/// schedule is made up from the first file so the match counts as followed.
struct Replay {
    files: Vec<PathBuf>,
    next: usize,
}

impl Source for Replay {
    fn get(&mut self, url: &str) -> Result<String, String> {
        let read = |p: &PathBuf| std::fs::read_to_string(p).map_err(|e| format!("{e:?}"));
        let first = self.files.first().ok_or("replay: no files")?;
        if url.contains("/schedule") {
            let e: serde_json::Value = serde_json::from_str(&read(first)?).map_err(|e| format!("{e:?}"))?;
            let league = std::env::var("NOWBAR_REPLAY_LEAGUE").unwrap_or_else(|_| "uefa.champions".into());
            let schedule = serde_json::json!({ "events": [{ "id": e["id"], "date": e["date"], "league": { "slug": league },
                "competitions": [{ "timeValid": true }] }] });
            return Ok(schedule.to_string());
        }
        let file = &self.files[self.next.min(self.files.len() - 1)];
        self.next += 1;
        read(file)
    }
}

enum Msg {
    Teams(Vec<String>),
}

pub struct FootballWorker {
    tx: Sender<Msg>,
}

impl FootballWorker {
    /// `post` runs on the worker thread with every tracked match whenever one changes.
    pub fn start(teams: Vec<String>, replay: Option<PathBuf>, post: impl Fn(Vec<MatchSnapshot>) + Send + 'static) -> Self {
        let (tx, rx) = channel::<Msg>();
        let replaying = replay.is_some();
        // A replay needs one "team" so its made-up schedule is read.
        let teams = if replaying && teams.is_empty() { vec![std::env::var("NOWBAR_REPLAY_FOLLOW").unwrap_or_else(|_| "replay".to_string())] } else { teams };
        std::thread::Builder::new()
            .name("football".into())
            .spawn(move || {
                unsafe {
                    let _ = RoInitialize(RO_INIT_MULTITHREADED);
                }
                let source: Box<dyn Source> = match replay {
                    Some(dir) => {
                        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
                            .map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect())
                            .unwrap_or_default();
                        files.sort();
                        log(&format!("football: replaying {} files from {}", files.len(), dir.display()));
                        Box::new(Replay { files, next: 0 })
                    }
                    None => match Espn::new() {
                        Ok(espn) => Box::new(espn),
                        Err(e) => {
                            log(&format!("football: no HTTP client: {e:?}"));
                            return;
                        }
                    },
                };
                run(source, teams, replaying, rx, post);
            })
            .expect("spawn football thread");
        FootballWorker { tx }
    }

    pub fn set_teams(&self, teams: Vec<String>) {
        let _ = self.tx.send(Msg::Teams(teams));
    }
}

struct Tracked {
    fixture: Fixture,
    snap: Option<MatchSnapshot>,
    next_at: Instant,
    failures: u32,
    /// Polls done after full time (one more, for late corrections, then it rests).
    after_end: u32,
}

fn run(mut source: Box<dyn Source>, mut teams: Vec<String>, replaying: bool, rx: Receiver<Msg>, post: impl Fn(Vec<MatchSnapshot>)) {
    let mut schedules: HashMap<String, Instant> = HashMap::new();
    let mut tracked: HashMap<String, Tracked> = HashMap::new();
    let mut last_posted: Vec<MatchSnapshot> = Vec::new();
    loop {
        let now = Instant::now();
        // Schedules: every followed team, every 6 h.
        for team in &teams {
            if schedules.get(team).is_some_and(|at| now.duration_since(*at) < SCHEDULE_EVERY) {
                continue;
            }
            match source.get(&format!("{BASE}/all/teams/{team}/schedule?fixture=true")) {
                Ok(text) => {
                    schedules.insert(team.clone(), now);
                    // And the results: after a cold start that's how last night's match is found.
                    let results = source.get(&format!("{BASE}/all/teams/{team}/schedule")).unwrap_or_else(|e| {
                        log(&format!("football: results of {team}: {e}"));
                        String::new()
                    });
                    let parse = |text: &str| serde_json::from_str(text).map(|v| parse_schedule(&v)).unwrap_or_default();
                    let fixtures: Vec<Fixture> = parse(&text).into_iter().chain(parse(&results)).collect();
                    let t = now_utc();
                    for f in fixtures {
                        let near = f.kickoff_utc - t <= TRACK_AHEAD_S && t - f.kickoff_utc <= TRACK_BEHIND_S;
                        if near && !tracked.contains_key(&f.event_id) {
                            log(&format!("football: tracking {} ({}) at {}", f.event_id, f.league, f.kickoff_utc));
                            tracked.insert(f.event_id.clone(), Tracked { fixture: f, snap: None, next_at: now, failures: 0, after_end: 0 });
                        }
                    }
                }
                Err(e) => {
                    log(&format!("football: schedule of {team}: {e}"));
                    // Try again in 5 minutes.
                    schedules.insert(team.clone(), now - SCHEDULE_EVERY + Duration::from_secs(300));
                }
            }
        }
        // Matches due for a poll.
        for t in tracked.values_mut().filter(|t| t.next_at <= now) {
            let url = format!("{BASE}/{}/scoreboard/{}", t.fixture.league, t.fixture.event_id);
            match source.get(&url).map(|text| serde_json::from_str::<serde_json::Value>(&text)) {
                Ok(Ok(v)) => match parse_event(&v, &t.fixture, &teams) {
                    Some(mut snap) => {
                        HEARD_MS.store(now_ms(), std::sync::atomic::Ordering::Relaxed);
                        if let Some(c) = snap.clock.as_mut() {
                            c.as_of_ms = now_ms();
                        }
                        let prev = t.snap.as_ref();
                        snap.ended_utc = end_time(prev.map(|s| s.status), prev.and_then(|s| s.ended_utc), snap.status, now_utc());
                        t.failures = 0;
                        if snap.status.is_finished() {
                            t.after_end += 1;
                        }
                        let wait = if t.after_end > 1 { u64::MAX } else { next_poll_s(snap.status, snap.kickoff_utc - now_utc(), 0) };
                        t.next_at = if wait == u64::MAX { now + Duration::from_secs(365 * 86_400) } else { now + Duration::from_secs(wait) };
                        cache_crests(&snap);
                        t.snap = Some(snap);
                    }
                    None => backoff(t, now, "unexpected scoreboard"),
                },
                Ok(Err(e)) => backoff(t, now, &format!("bad JSON: {e}")),
                Err(e) => backoff(t, now, &e),
            }
        }
        // Forget matches long over, and those no followed team plays any more.
        let t_now = now_utc();
        tracked.retain(|_, t| {
            let over = t_now - t.fixture.kickoff_utc > 22 * 3600;
            let still_followed = t.snap.as_ref().is_none_or(|s| s.followed.is_some());
            replaying || (!over && still_followed)
        });
        let mut snaps: Vec<MatchSnapshot> = tracked.values().filter_map(|t| t.snap.clone()).collect();
        snaps.sort_by_key(|s| (s.kickoff_utc, s.id.clone()));
        if snaps != last_posted {
            post(snaps.clone());
            last_posted = snaps;
        }

        // Sleep until the next poll or schedule refresh, or a new team list.
        let next_poll = tracked.values().map(|t| t.next_at).min();
        let next_schedule = teams.iter().filter_map(|team| schedules.get(team).map(|at| *at + SCHEDULE_EVERY)).min();
        let wake = [next_poll, next_schedule].into_iter().flatten().min();
        let wait = wake.map(|w| w.saturating_duration_since(Instant::now())).unwrap_or(Duration::from_secs(24 * 3600));
        match rx.recv_timeout(wait) {
            Ok(Msg::Teams(new)) => {
                if new != teams {
                    log(&format!("football: following {new:?}"));
                    schedules.retain(|team, _| new.contains(team));
                    teams = new;
                    // Re-check who is followed on the next poll.
                    for t in tracked.values_mut() {
                        t.next_at = Instant::now();
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn backoff(t: &mut Tracked, now: Instant, why: &str) {
    t.failures += 1;
    let status = t.snap.as_ref().map_or(super::football::MatchStatus::Scheduled, |s| s.status);
    let wait = next_poll_s(status, t.fixture.kickoff_utc - now_utc(), t.failures);
    log(&format!("football: {} failed ({why}); retry in {wait} s", t.fixture.event_id));
    t.next_at = now + Duration::from_secs(wait.min(3600));
}

/// One GET for the other sports and the weather (the same client: no cache, a browser-like user
/// agent); WinHTTP where that client can't get through (Open-Meteo's TLS).
pub(crate) fn fetch_text(url: &str) -> Result<String, String> {
    match Espn::new().map_err(|e| format!("{e:?}")).and_then(|mut c| c.get(url)) {
        Ok(text) => Ok(text),
        Err(first) => crate::platform::http::get_text(url).map_err(|e| format!("{first}; WinHTTP: {e}")),
    }
}

/// %LOCALAPPDATA%\NowBar\crests\{team id}.png
pub fn crest_path(team_id: &str) -> PathBuf {
    crate::platform::log::log_path().with_file_name("crests").join(format!("{team_id}.png"))
}

/// A cached crest older than this is fetched again: clubs redraw their badges.
const CREST_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 3600);
/// After a team's crest couldn't be fetched, it isn't tried again for this long (not every poll).
const CREST_RETRY: Duration = Duration::from_secs(6 * 3600);

/// Keeps each team's crest cached and current (the UI scales it to the exact size).
fn cache_crests(m: &MatchSnapshot) {
    for team in [&m.home, &m.away] {
        queue_crest(CrestJob::Team { id: team.id.clone(), name: team.name.clone(), logo: team.logo.clone() });
    }
}

/// Bumped by every crest saved: the UI drops the crests it decoded and draws them again.
pub static CREST_SAVED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A crest for the crest thread to keep current.
pub enum CrestJob {
    /// A football team's: [`fetch_team_crest`].
    Team { id: String, name: String, logo: Option<String> },
    /// A picture at a known address (basketball's feed): [`fetch_crest`].
    Url { id: String, url: String },
}

/// Hands `job` to the crest thread (started on first use): the score polls never wait on a
/// picture. Each saved crest wakes the UI as a match update.
pub fn queue_crest(job: CrestJob) {
    static JOBS: std::sync::Mutex<Option<Sender<CrestJob>>> = std::sync::Mutex::new(None);
    let mut jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    if jobs.is_none() {
        let (tx, rx) = channel::<CrestJob>();
        let spawned = std::thread::Builder::new().name("crests".into()).spawn(move || {
            unsafe {
                let _ = RoInitialize(RO_INIT_MULTITHREADED);
            }
            for job in rx {
                let before = CREST_SAVED.load(std::sync::atomic::Ordering::Relaxed);
                match job {
                    CrestJob::Team { id, name, logo } => fetch_team_crest(&id, &name, logo.as_deref()),
                    CrestJob::Url { id, url } => fetch_crest(&id, &url),
                };
                if CREST_SAVED.load(std::sync::atomic::Ordering::Relaxed) != before {
                    crate::platform::windows::post_matches();
                }
            }
        });
        match spawned {
            Ok(_) => *jobs = Some(tx),
            Err(e) => {
                log(&format!("crests: no thread: {e}"));
                return;
            }
        }
    }
    if let Some(tx) = jobs.as_ref() {
        let _ = tx.send(job);
    }
}

/// Whether `team_id`'s cached crest is there and younger than [`CREST_MAX_AGE`].
fn crest_fresh(path: &std::path::Path) -> bool {
    let age = std::fs::metadata(path).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
    age.is_some_and(|a| a < CREST_MAX_AGE)
}

/// Whether `team_id` failed lately (and notes a failure now with `failed`).
fn crest_backoff(team_id: &str, failed: bool) -> bool {
    static FAILED: std::sync::Mutex<Option<HashMap<String, Instant>>> = std::sync::Mutex::new(None);
    let mut map = FAILED.lock().unwrap_or_else(|e| e.into_inner());
    let map = map.get_or_insert_with(HashMap::new);
    if failed {
        map.insert(team_id.to_string(), Instant::now());
        return true;
    }
    map.get(team_id).is_some_and(|t| t.elapsed() < CREST_RETRY)
}

/// `team_id`'s crest, current: TheSportsDB's badge for a team of exactly that name (kept up to
/// date by its community; ESPN's lag years behind outside Europe's big leagues), else ESPN's
/// `logo`, else ESPN's picture by id. A stale crest stays when none can be fetched. Whether one
/// is cached now (blocking: off the UI thread).
pub fn fetch_team_crest(team_id: &str, name: &str, logo: Option<&str>) -> bool {
    let path = crest_path(team_id);
    if crest_fresh(&path) || crest_backoff(team_id, false) {
        return path.exists();
    }
    let Ok(espn) = Espn::new() else { return path.exists() };
    let by_id = format!("https://a.espncdn.com/i/teamlogos/soccer/500/{team_id}.png");
    let espn_urls = logo.filter(|u| u.starts_with("https://")).into_iter().chain((!team_id.is_empty()).then_some(by_id.as_str()));
    let badge = sportsdb_badge(&espn, name);
    for url in badge.iter().map(String::as_str).chain(espn_urls) {
        if let Some(png) = espn.bytes(url).ok().filter(|b| is_picture(b)) {
            return save_crest(team_id, &path, &png);
        }
    }
    log(&format!("football: crest {team_id} ({name}): none found"));
    crest_backoff(team_id, true);
    path.exists()
}

/// TheSportsDB's badge for the football team called `name` (accents, case and punctuation aside),
/// when exactly one team has that name or alternate name.
fn sportsdb_badge(espn: &Espn, name: &str) -> Option<String> {
    let key = fold(name);
    if key.len() < 3 {
        return None;
    }
    let url = format!(
        "https://www.thesportsdb.com/api/v1/json/123/searchteams.php?t={}",
        Uri::EscapeComponent(&HSTRING::from(name)).ok()?
    );
    let v: serde_json::Value = serde_json::from_slice(&espn.bytes(&url).ok()?).ok()?;
    let named = |t: &&serde_json::Value| {
        let alternates = t["strTeamAlternate"].as_str().unwrap_or("");
        t["strSport"] == "Soccer"
            && std::iter::once(t["strTeam"].as_str().unwrap_or("")).chain(alternates.split(',')).any(|n| fold(n) == key)
    };
    let hits: Vec<&serde_json::Value> = v["teams"].as_array()?.iter().filter(named).collect();
    match hits.as_slice() {
        [t] => t["strBadge"].as_str().filter(|u| u.starts_with("https://")).map(str::to_string),
        _ => None,
    }
}

/// A team name to compare: lower case, Latin letters without accents, letters and digits only.
fn fold(name: &str) -> String {
    name.chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' => 'a',
            'ç' | 'ć' | 'č' => 'c',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'ğ' => 'g',
            'í' | 'ì' | 'î' | 'ï' | 'ı' => 'i',
            'ñ' => 'n',
            'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'ø' => 'o',
            'ş' | 'š' | 'ś' => 's',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'ž' | 'ź' | 'ż' => 'z',
            c => c,
        })
        .filter(|c| c.is_alphanumeric())
        .collect()
}

/// PNG or JPEG bytes (an error page or an empty answer isn't a crest).
fn is_picture(b: &[u8]) -> bool {
    b.starts_with(b"\x89PNG") || b.starts_with(b"\xFF\xD8\xFF")
}

fn save_crest(team_id: &str, path: &std::path::Path, png: &[u8]) -> bool {
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(path));
    match std::fs::write(path, png) {
        Ok(()) => {
            CREST_SAVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            true
        }
        Err(e) => {
            log(&format!("football: crest {team_id}: {e:?}"));
            false
        }
    }
}

/// Downloads `team_id`'s crest from `url` into the cache unless a current one is there; whether
/// one is now (blocking: off the UI thread; basketball uses it with its own feed's crests).
pub fn fetch_crest(team_id: &str, url: &str) -> bool {
    let path = crest_path(team_id);
    if crest_fresh(&path) || crest_backoff(team_id, false) || !url.starts_with("https://") {
        return path.exists();
    }
    let Ok(espn) = Espn::new() else { return path.exists() };
    match espn.bytes(url) {
        Ok(png) if is_picture(&png) => save_crest(team_id, &path, &png),
        Ok(_) => {
            log(&format!("football: crest {team_id}: not a picture"));
            crest_backoff(team_id, true);
            path.exists()
        }
        Err(e) => {
            log(&format!("football: crest {team_id}: {e}"));
            crest_backoff(team_id, true);
            path.exists()
        }
    }
}

/// Team search for the settings window (blocking; call it off the UI thread).
pub fn search_teams(query: &str) -> Vec<TeamHit> {
    try_search_teams(query).unwrap_or_default()
}

/// Team search; `None` when ESPN couldn't be reached or answered nonsense (so the window can
/// say "Bağlantı kurulamadı" instead of "bulunamadı").
pub fn try_search_teams(query: &str) -> Option<Vec<TeamHit>> {
    let q: String = query.trim().chars().filter(|c| !c.is_control()).collect();
    if q.chars().count() < 2 {
        return Some(Vec::new());
    }
    let mut espn = Espn::new().ok()?;
    let url = format!(
        "https://site.web.api.espn.com/apis/common/v3/search?query={}&type=team&sport=soccer&limit=10",
        Uri::EscapeComponent(&HSTRING::from(q.as_str())).map(|h| h.to_string()).unwrap_or(q)
    );
    let v: serde_json::Value = serde_json::from_str(&espn.get(&url).ok()?).ok()?;
    Some(parse_search(&v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn team_names_compare_without_accents_case_or_punctuation() {
        assert_eq!(fold("Fenerbahçe"), fold("FENERBAHCE"));
        assert_eq!(fold("Beşiktaş J.K."), "besiktasjk");
        assert_eq!(fold("Al-Ahly"), fold("Al Ahly"));
        assert_ne!(fold("Al Ahli"), fold("Al Ahly"), "another club");
    }

    #[test]
    fn only_pictures_are_crests() {
        assert!(is_picture(b"\x89PNG\r\n\x1a\n...."));
        assert!(is_picture(b"\xFF\xD8\xFF\xE0...."));
        assert!(!is_picture(b"<html>404</html>"));
        assert!(!is_picture(b""));
    }
}
