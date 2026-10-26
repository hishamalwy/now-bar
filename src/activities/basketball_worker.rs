//! Polls EuroLeague and EuroCup for the followed clubs ("bb-E-ULK"): each club's season list
//! every 6 h (every 2 min while one of its games is near), and the game's live header every 20 s
//! from ten minutes before tip-off until it's over. Posts every tracked game when one changes.

use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::basketball::{apply_header, clubs_url, game_of, games_url, header_url, parse_clubs, parse_games, parse_team_id, search_clubs, season_code, LEAGUES};
use super::football::{MatchSnapshot, TeamHit};
use super::football_worker::{fetch_text, queue_crest, CrestJob};
use crate::platform::log::log;

const LIST_EVERY: Duration = Duration::from_secs(6 * 3600);
const LIST_EVERY_NEAR: Duration = Duration::from_secs(120);
/// Games this close (tip-off within a day ahead, or up to 4 h ago) are tracked.
const TRACK_AHEAD_S: i64 = 24 * 3600;
// Last night's match too, even when the PC was off: the morning brief shows its score.
const TRACK_BEHIND_S: i64 = 20 * 3600;
const HEADER_EVERY_S: u64 = 20;
/// The live header is asked from this long before tip-off.
const HEADER_FROM_S: i64 = 10 * 60;

/// The last live header answer (GetTickCount64 ms), changed or not: what the live dot's
/// "stale" goes by (`post` only runs on a change).
pub static HEARD_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn now_utc() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub struct BasketballWorker {
    tx: Sender<Vec<String>>,
}

impl BasketballWorker {
    /// `teams`: every followed team id (the football ones are left out here).
    pub fn start(teams: Vec<String>, post: impl Fn(Vec<MatchSnapshot>) + Send + 'static) -> Self {
        let (tx, rx) = channel::<Vec<String>>();
        std::thread::Builder::new()
            .name("basketball".into())
            .spawn(move || {
                unsafe {
                    let _ = windows::Win32::System::WinRT::RoInitialize(windows::Win32::System::WinRT::RO_INIT_MULTITHREADED);
                }
                run(clubs(&teams), rx, post);
            })
            .expect("spawn basketball thread");
        BasketballWorker { tx }
    }

    pub fn set_teams(&self, teams: Vec<String>) {
        let _ = self.tx.send(teams);
    }
}

/// The basketball ids among the followed teams.
fn clubs(teams: &[String]) -> Vec<String> {
    teams.iter().filter(|t| parse_team_id(t).is_some()).cloned().collect()
}

struct Tracked {
    snap: MatchSnapshot,
    next_at: Instant,
    /// Header polls done after the end (one more, then it rests).
    after_end: u32,
}

fn run(mut teams: Vec<String>, rx: Receiver<Vec<String>>, post: impl Fn(Vec<MatchSnapshot>)) {
    let mut lists: HashMap<String, Instant> = HashMap::new();
    let mut tracked: HashMap<String, Tracked> = HashMap::new();
    let mut last_posted: Vec<MatchSnapshot> = Vec::new();
    loop {
        let now = Instant::now();
        let t = now_utc();
        // Season lists: each club every 6 h, every 2 min while one of its games is near.
        for team in &teams {
            let Some((comp, code)) = parse_team_id(team) else { continue };
            let near = tracked.values().any(|g| (g.snap.home.id == *team || g.snap.away.id == *team) && !g.snap.status.is_finished());
            let every = if near { LIST_EVERY_NEAR } else { LIST_EVERY };
            if lists.get(team).is_some_and(|at| now.duration_since(*at) < every) {
                continue;
            }
            match fetch_text(&games_url(comp, &season_code(comp, t), &code)).and_then(|text| serde_json::from_str(&text).map_err(|e| e.to_string())) {
                Ok(v) => {
                    lists.insert(team.clone(), now);
                    for game in parse_games(&v, comp, &teams) {
                        if game.kickoff_utc - t > TRACK_AHEAD_S || t - game.kickoff_utc > TRACK_BEHIND_S {
                            continue;
                        }
                        for side in [&game.home, &game.away] {
                            if let Some(url) = &side.logo {
                                queue_crest(CrestJob::Url { id: side.id.clone(), url: url.clone() });
                            }
                        }
                        match tracked.get_mut(&game.id) {
                            // The header knows better while a game is on; the list has the result.
                            Some(g) if game.status.is_finished() && !g.snap.status.is_finished() => g.snap = game,
                            Some(_) => {}
                            None => {
                                log(&format!("basketball: tracking {} {}–{} at {}", game.id, game.home.code, game.away.code, game.kickoff_utc));
                                tracked.insert(game.id.clone(), Tracked { snap: game, next_at: now, after_end: 0 });
                            }
                        }
                    }
                }
                Err(e) => {
                    log(&format!("basketball: games of {team}: {e}"));
                    lists.insert(team.clone(), now - LIST_EVERY + Duration::from_secs(300));
                }
            }
        }
        // Live headers of the games about to start or on.
        for g in tracked.values_mut().filter(|g| g.next_at <= now) {
            let to_tip = g.snap.kickoff_utc - t;
            if to_tip > HEADER_FROM_S {
                g.next_at = now + Duration::from_secs((to_tip - HEADER_FROM_S) as u64);
                continue;
            }
            if g.after_end > 1 {
                g.next_at = now + Duration::from_secs(365 * 86_400);
                continue;
            }
            let Some((season, game)) = game_of(&g.snap.id) else { continue };
            // Before a game the header is empty: nothing to apply yet.
            let header = fetch_text(&header_url(&season, game))
                .and_then(|text| if text.trim().is_empty() { Ok(serde_json::Value::Null) } else { serde_json::from_str(&text).map_err(|e| e.to_string()) });
            match header {
                Ok(v) => {
                    HEARD_MS.store(unsafe { windows::Win32::System::SystemInformation::GetTickCount64() }, std::sync::atomic::Ordering::Relaxed);
                    let before = g.snap.status;
                    apply_header(&mut g.snap, &v);
                    if g.snap.status.is_finished() {
                        g.snap.ended_utc = crate::activities::football::end_time(Some(before), g.snap.ended_utc, g.snap.status, t);
                        g.after_end += 1;
                    }
                    g.next_at = now + Duration::from_secs(if g.snap.status.is_finished() { 120 } else { HEADER_EVERY_S });
                }
                Err(e) => {
                    log(&format!("basketball: header of {}: {e}", g.snap.id));
                    g.next_at = now + Duration::from_secs(60);
                }
            }
        }
        // Forget games long over, and those no followed club plays any more.
        tracked.retain(|_, g| t - g.snap.kickoff_utc <= 22 * 3600 && (teams.contains(&g.snap.home.id) || teams.contains(&g.snap.away.id)));
        let mut snaps: Vec<MatchSnapshot> = tracked.values().map(|g| g.snap.clone()).collect();
        snaps.sort_by_key(|s| (s.kickoff_utc, s.id.clone()));
        if snaps != last_posted {
            post(snaps.clone());
            last_posted = snaps;
        }
        let next_game = tracked.values().map(|g| g.next_at).min();
        let next_list = teams
            .iter()
            .filter_map(|team| lists.get(team).map(|at| *at + if tracked.values().any(|g| g.snap.home.id == *team || g.snap.away.id == *team) { LIST_EVERY_NEAR } else { LIST_EVERY }))
            .min();
        let wake = [next_game, next_list].into_iter().flatten().min();
        let wait = wake.map(|w| w.saturating_duration_since(Instant::now())).unwrap_or(Duration::from_secs(24 * 3600));
        match rx.recv_timeout(wait.max(Duration::from_secs(1))) {
            Ok(new) => {
                let new = clubs(&new);
                if new != teams {
                    log(&format!("basketball: following {new:?}"));
                    lists.retain(|team, _| new.contains(team));
                    teams = new;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// EuroLeague and EuroCup clubs whose name holds `query`, for the settings window's team search
/// (blocking; the club lists are fetched once per run).
pub fn search_basketball(query: &str) -> Vec<TeamHit> {
    static CLUBS: OnceLock<Mutex<Vec<TeamHit>>> = OnceLock::new();
    let cache = CLUBS.get_or_init(|| Mutex::new(Vec::new()));
    let mut clubs = cache.lock().map(|c| c.clone()).unwrap_or_default();
    if clubs.is_empty() {
        let t = now_utc();
        for (comp, _, _) in LEAGUES {
            if let Ok(v) = fetch_text(&clubs_url(comp, &season_code(comp, t))).and_then(|text| serde_json::from_str(&text).map_err(|e| e.to_string())) {
                clubs.extend(parse_clubs(&v, comp));
            }
        }
        if let Ok(mut c) = cache.lock() {
            *c = clubs.clone();
        }
    }
    search_clubs(&clubs, query)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asks the feeds for real: `cargo test --lib live_basketball -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_basketball() {
        for q in ["fener", "efes", "beşiktaş"] {
            let hits: Vec<(String, String, String)> = search_basketball(q).into_iter().map(|h| (h.id, h.name, h.league)).collect();
            println!("search {q}: {hits:?}");
        }
        let t = now_utc();
        let v: serde_json::Value = serde_json::from_str(&fetch_text(&games_url('E', &season_code('E', t), "ULK")).unwrap()).unwrap();
        let games = parse_games(&v, 'E', &["bb-E-ULK".to_string()]);
        let next = games.iter().filter(|g| g.kickoff_utc > t - 4 * 3600).min_by_key(|g| g.kickoff_utc).unwrap();
        println!("next: {} {} {}–{} {:?} {:?}", next.id, next.kickoff_utc, next.home.name, next.away.name, next.status, next.venue);
        let (season, game) = game_of(&next.id).unwrap();
        let header = fetch_text(&header_url(&season, game)).unwrap();
        println!("header: {}", &header[..header.len().min(300)]);
    }
}
