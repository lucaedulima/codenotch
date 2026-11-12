//! Gemini app usage, read from the page Google shows it on.
//!
//! The Gemini app (web and desktop) has no API and leaves no reusable credential on disk, unlike the
//! coding CLIs the other providers borrow a session from. What it does have, since May 2026, is a
//! usage page — https://gemini.google.com/usage — with the share of the five-hour allowance used and
//! the weekly one, the same two windows Claude reports. So this provider reads that page:
//!
//!   1. Sign-in: a normal window of our own opens the page; the person signs in to Google there once.
//!      WebView2 keeps the cookies in Codenotch's own profile, so the session outlives restarts. It
//!      is a separate profile from the Gemini desktop app's: nothing of that app is read.
//!   2. Reading: every few minutes a hidden window loads the page, waits for the numbers to render,
//!      and hands the page's visible text back by navigating to `https://codenotch.invalid/r#…`,
//!      which `on_navigation` intercepts and cancels — so the page needs no IPC permission at all.
//!      The hidden window is destroyed again once it has answered.
//!   3. Parsing: the text is searched for percentages and the words around them (English and
//!      Portuguese): which window a number belongs to, and whether it says used or remaining.
//!
//! This is scraping, so it is the most fragile provider here: a redesign of the page can break the
//! parse until it is adjusted. When a reading fails, the page's text is written to run.log (that page
//! shows usage, not conversations) so the parser can be fixed against what Google actually shows.

use crate::usage::{LimitWindow, UsageSnapshot};
use crate::AppState;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

pub const USAGE_URL: &str = "https://gemini.google.com/usage";
const LABEL: &str = "gemini";
const REPORT_HOST: &str = "codenotch.invalid";
const POLL_SECS: u64 = 300;
/// A plain Edge user agent: Google refuses to sign in some embedded browsers by their UA alone.
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36 Edg/140.0.0.0";

/// Waits for a percentage to render (the page is an SPA), then reports whatever text is there.
const EXTRACT_JS: &str = r#"(function(){
  if(window.__codenotchReading) return; window.__codenotchReading=true;
  var n=0;
  function send(){
    var t=(document.body&&document.body.innerText||'').slice(0,8000);
    var d={href:location.href,title:document.title,text:t};
    window.__codenotchReading=false;
    location.href='https://codenotch.invalid/r#'+encodeURIComponent(JSON.stringify(d));
  }
  (function wait(){
    var t=document.body?document.body.innerText:'';
    if(/\d\s?%/.test(t)||n++>24) send(); else setTimeout(wait,500);
  })();
})();"#;

static REFRESH: AtomicBool = AtomicBool::new(false);
/// The sign-in window is up: the reading window is the same window, so it is not destroyed.
static SIGNING_IN: AtomicBool = AtomicBool::new(false);
/// A reading is in flight (the hidden window exists for it).
static READING: AtomicBool = AtomicBool::new(false);

pub fn request_refresh() {
    REFRESH.store(true, Ordering::Relaxed);
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn store_path() -> std::path::PathBuf {
    crate::config::config_path().with_file_name("gemini_app.json")
}

pub fn load_persisted() -> UsageSnapshot {
    std::fs::read_to_string(store_path())
        .ok()
        .and_then(|t| serde_json::from_str::<UsageSnapshot>(&t).ok())
        .unwrap_or_else(signed_out)
}

fn signed_out() -> UsageSnapshot {
    UsageSnapshot {
        status: "needsAuth".into(),
        note: "Sign in to Gemini: click this ring, or right-click the notch".into(),
        ..Default::default()
    }
}

fn broadcast(app: &AppHandle, snap: UsageSnapshot) {
    let st = app.state::<AppState>();
    *st.gemini_app.lock().unwrap() = snap.clone();
    if let Ok(t) = serde_json::to_string_pretty(&snap) {
        let _ = std::fs::write(store_path(), t);
    }
    let _ = app.emit("geminiapp", &snap);
}

fn current(app: &AppHandle) -> UsageSnapshot {
    app.state::<AppState>().gemini_app.lock().unwrap().clone()
}

/// A ring click: signs in when there is no session yet, reads again otherwise.
pub fn ring_clicked(app: &AppHandle) -> bool {
    if current(app).status == "needsAuth" {
        sign_in(app);
        false
    } else {
        request_refresh();
        true
    }
}

/// Opens (or brings forward) the sign-in window on the usage page.
pub fn sign_in(app: &AppHandle) {
    let app = app.clone();
    // Window creation from a command handler can deadlock WebView2 on Windows; a thread of its own cannot.
    std::thread::spawn(move || {
        SIGNING_IN.store(true, Ordering::SeqCst);
        match window(&app, true) {
            Some(w) => {
                let _ = w.show();
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
            None => SIGNING_IN.store(false, Ordering::SeqCst),
        }
    });
}

/// The one Gemini window: built hidden for a reading or visible for signing in.
fn window(app: &AppHandle, visible: bool) -> Option<tauri::WebviewWindow> {
    if let Some(w) = app.get_webview_window(LABEL) {
        if visible {
            if let Ok(url) = USAGE_URL.parse() {
                let _ = w.navigate(url);
            }
        }
        return Some(w);
    }
    let url: tauri::Url = USAGE_URL.parse().ok()?;
    let nav_app = app.clone();
    let load_app = app.clone();
    let built = tauri::WebviewWindowBuilder::new(app, LABEL, tauri::WebviewUrl::External(url))
        .title("Codenotch · Gemini")
        .inner_size(520.0, 760.0)
        .center()
        .visible(visible)
        .skip_taskbar(!visible)
        .user_agent(USER_AGENT)
        .on_navigation(move |url| {
            if url.host_str() == Some(REPORT_HOST) {
                let raw = url.fragment().unwrap_or("").to_string();
                let a = nav_app.clone();
                std::thread::spawn(move || on_report(&a, &raw));
                return false;
            }
            true
        })
        .on_page_load(move |w, payload| {
            if payload.event() != tauri::webview::PageLoadEvent::Finished {
                return;
            }
            let host = payload.url().host_str().unwrap_or("").to_string();
            if host == "gemini.google.com" {
                // Give the SPA a moment before the script starts waiting for numbers itself
                let w = w.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(1500));
                    let _ = w.eval(EXTRACT_JS);
                });
            } else if host.ends_with("accounts.google.com") && !SIGNING_IN.load(Ordering::SeqCst) {
                // A hidden reading sent to the sign-in page: the session is gone
                crate::applog("gemini app: the usage page asked for a Google sign-in");
                broadcast(&load_app, signed_out());
                finish_reading(&load_app);
            }
        })
        .build();
    match built {
        Ok(w) => {
            let a = app.clone();
            w.on_window_event(move |ev| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = ev {
                    // Closing the sign-in window hides it and reads right away
                    if SIGNING_IN.swap(false, Ordering::SeqCst) {
                        api.prevent_close();
                        if let Some(w) = a.get_webview_window(LABEL) {
                            let _ = w.hide();
                            let _ = w.destroy();
                        }
                        request_refresh();
                    }
                }
            });
            Some(w)
        }
        Err(e) => {
            crate::applog(&format!("gemini app: could not open its window: {e}"));
            None
        }
    }
}

fn finish_reading(app: &AppHandle) {
    READING.store(false, Ordering::SeqCst);
    if !SIGNING_IN.load(Ordering::SeqCst) {
        if let Some(w) = app.get_webview_window(LABEL) {
            let _ = w.destroy();
        }
    }
}

#[derive(serde::Deserialize, Default)]
struct Report {
    #[serde(default)]
    href: String,
    #[serde(default)]
    text: String,
}

fn on_report(app: &AppHandle, raw: &str) {
    let report: Report = serde_json::from_str(&percent_decode(raw)).unwrap_or_default();
    let prev = current(app);
    let parsed = parse(&report.text);
    let now = now_ms();
    let snap = match parsed {
        Some((windows, note)) => {
            if prev.status != "ok" {
                crate::applog(&format!("gemini app: first reading after sign-in; page text was: {}", excerpt(&report.text)));
            }
            UsageSnapshot { status: "ok".into(), windows, fetched_at: now, note, backoff_until: 0 }
        }
        None if looks_signed_out(&report) => signed_out(),
        None => {
            crate::applog(&format!(
                "gemini app: no usage figures found at {}; page text was: {}",
                report.href,
                excerpt(&report.text)
            ));
            UsageSnapshot {
                status: if prev.windows.is_empty() { "error" } else { "stale" }.into(),
                note: "Could not read gemini.google.com/usage; the last reading stands".into(),
                ..prev
            }
        }
    };
    broadcast(app, snap);
    finish_reading(app);
}

fn looks_signed_out(r: &Report) -> bool {
    let t = r.text.to_lowercase();
    r.href.contains("accounts.google.com")
        || ["sign in", "fazer login", "entrar", "faça login"].iter().any(|k| t.contains(k)) && !t.contains('%')
}

fn excerpt(text: &str) -> String {
    text.chars().take(1500).collect::<String>().replace('\n', " | ")
}

/// `%XX` sequences back to bytes, then UTF-8 (what `encodeURIComponent` produced).
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The five-hour window's heading. The Portuguese page does not say "5 horas" at all: it calls it
/// "Uso atual" (current usage), with "Limite semanal" for the week below it. Not a bare "atual",
/// which would also match the "Atualizado agora" the page prints above everything.
const SESSION_WORDS: [&str; 14] = [
    "5 hour", "5-hour", "five hour", "five-hour", "5 horas", "cinco horas", "5h", "5 h ", "hourly",
    " uso atual ", "limite atual", "current usage", "current limit", "current use",
];
const WEEKLY_WORDS: [&str; 4] = ["week", "semana", "semanal", "7 dias"];
const REMAINING_WORDS: [&str; 7] = ["remaining", "left", "restante", "restam", "resta", "disponível", "disponivel"];
const USED_WORDS: [&str; 5] = ["used", "usado", "usada", "utilizado", "consumido"];
const RESET_WORDS: [&str; 6] = ["reset", "renova", "redefin", "reinicia", "reabastec", "refresh"];

/// A clock time on a line — "0:55", "23:55", "12:55 PM" — as 24-hour (hour, minute).
fn clock_in(line: &str) -> Option<(u32, u32)> {
    let b: Vec<char> = line.chars().collect();
    for i in 0..b.len() {
        if b[i] != ':' {
            continue;
        }
        let mut j = i;
        while j > 0 && b[j - 1].is_ascii_digit() && i - j < 2 {
            j -= 1;
        }
        if j == i || (j > 0 && b[j - 1].is_ascii_digit()) {
            continue;
        }
        if i + 2 >= b.len() || !b[i + 1].is_ascii_digit() || !b[i + 2].is_ascii_digit() {
            continue;
        }
        if b.get(i + 3).is_some_and(|c| c.is_ascii_digit()) {
            continue;
        }
        let (Ok(h), Ok(m)) = (
            b[j..i].iter().collect::<String>().parse::<u32>(),
            b[i + 1..i + 3].iter().collect::<String>().parse::<u32>(),
        ) else {
            continue;
        };
        let after: String = b[i + 3..].iter().take(5).collect::<String>().to_lowercase();
        let after = after.trim_start();
        let h = if after.starts_with("pm") && h < 12 {
            h + 12
        } else if after.starts_with("am") && h == 12 {
            0
        } else {
            h
        };
        if h < 24 && m < 60 {
            return Some((h, m));
        }
    }
    None
}

/// Month words, Portuguese and English, abbreviated and in full, as the page prints them ("17 de
/// nov.", "Nov 17"). Matched whole, never as a prefix: "mais" is not May.
const MONTHS: [(&str, u32); 44] = [
    ("jan", 1), ("janeiro", 1), ("january", 1),
    ("fev", 2), ("feb", 2), ("fevereiro", 2), ("february", 2),
    ("mar", 3), ("março", 3), ("marco", 3), ("march", 3),
    ("abr", 4), ("apr", 4), ("abril", 4), ("april", 4),
    ("mai", 5), ("may", 5), ("maio", 5),
    ("jun", 6), ("junho", 6), ("june", 6),
    ("jul", 7), ("julho", 7), ("july", 7),
    ("ago", 8), ("aug", 8), ("agosto", 8), ("august", 8),
    ("set", 9), ("sep", 9), ("sept", 9), ("setembro", 9), ("september", 9),
    ("out", 10), ("oct", 10), ("outubro", 10), ("october", 10),
    ("nov", 11), ("novembro", 11), ("november", 11),
    ("dez", 12), ("dec", 12), ("dezembro", 12), ("december", 12),
];

/// A calendar day on a line, as (month, day). Only a month word with a day number right beside it
/// counts — "17 de nov", "17 nov", "nov 17" — so "out of", "mais" or "2 hours ago" never read as dates.
fn date_in(line: &str) -> Option<(u32, u32)> {
    let words: Vec<String> = line
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
        .collect();
    let day = |w: Option<&String>| w.and_then(|w| w.parse::<u32>().ok()).filter(|d| (1..=31).contains(d));
    for (k, w) in words.iter().enumerate() {
        let Some(&(_, month)) = MONTHS.iter().find(|(name, _)| *name == w.as_str()) else { continue };
        let before = k.checked_sub(1).and_then(|j| words.get(j));
        let before2 = k.checked_sub(2).and_then(|j| words.get(j));
        let found = day(before)
            .or_else(|| if before.map(|s| s.as_str()) == Some("de") { day(before2) } else { None })
            .or_else(|| day(words.get(k + 1)));
        if let Some(d) = found {
            return Some((month, d));
        }
    }
    None
}

/// The moment a reset line names, in epoch ms: a clock time alone is its next occurrence (today, or
/// tomorrow once it has passed), and a day with it is that day this year, or next year's once it is
/// more than a day behind. Local time, because that is the clock the page wrote it in.
fn reset_at(line: &str, now: chrono::DateTime<chrono::Local>) -> Option<u64> {
    use chrono::{Datelike, TimeZone};
    let (h, m) = clock_in(line)?;
    let at = match date_in(line) {
        Some((month, d)) => {
            let on = |y: i32| chrono::Local.with_ymd_and_hms(y, month, d, h, m, 0).earliest();
            let t = on(now.year())?;
            if t < now - chrono::Duration::days(1) {
                on(now.year() + 1)?
            } else {
                t
            }
        }
        None => {
            let t = chrono::Local.from_local_datetime(&now.date_naive().and_hms_opt(h, m, 0)?).earliest()?;
            if t <= now {
                t + chrono::Duration::days(1)
            } else {
                t
            }
        }
    };
    u64::try_from(at.timestamp_millis()).ok()
}

/// The first percentage on a line, as a fraction.
fn first_pct(line: &str) -> Option<f64> {
    let chars: Vec<char> = line.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if *c != '%' {
            continue;
        }
        let mut j = i;
        while j > 0 && chars[j - 1] == ' ' {
            j -= 1;
        }
        let end = j;
        while j > 0 && (chars[j - 1].is_ascii_digit() || chars[j - 1] == '.' || chars[j - 1] == ',') {
            j -= 1;
        }
        if j == end {
            continue;
        }
        let num: String = chars[j..end].iter().collect::<String>().replace(',', ".");
        if let Ok(v) = num.trim_matches('.').parse::<f64>() {
            if (0.0..=100.0).contains(&v) {
                return Some(v / 100.0);
            }
        }
    }
    None
}

fn has_any(text: &str, words: &[&str]) -> bool {
    words.iter().any(|w| text.contains(w))
}

/// The page's text → the two windows, and a note carrying any reset wording that could not be read
/// as a moment (one that could becomes the window's own `resets_at`).
pub fn parse(text: &str) -> Option<(Vec<LimitWindow>, String)> {
    parse_at(text, chrono::Local::now())
}

fn parse_at(text: &str, now: chrono::DateTime<chrono::Local>) -> Option<(Vec<LimitWindow>, String)> {
    let lines: Vec<String> = text.lines().map(|l| l.trim().to_lowercase()).filter(|l| !l.is_empty()).collect();
    let mut session: Option<(f64, Option<String>)> = None;
    let mut weekly: Option<(f64, Option<String>)> = None;
    let mut loose: Vec<(f64, Option<String>)> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let Some(p) = first_pct(line) else { continue };
        // The nearest heading decides which window this is: this line, then up to three above it
        let mut kind = "";
        for k in 0..=3usize {
            let Some(l) = i.checked_sub(k).and_then(|j| lines.get(j)) else { break };
            let l = format!(" {l} ");
            if has_any(&l, &SESSION_WORDS) {
                kind = "session";
                break;
            }
            if has_any(&l, &WEEKLY_WORDS) {
                kind = "weekly";
                break;
            }
        }
        let near = format!("{} {}", i.checked_sub(1).and_then(|j| lines.get(j)).map(|s| s.as_str()).unwrap_or(""), line);
        let used = if has_any(line, &USED_WORDS) || !has_any(&near, &REMAINING_WORDS) { p } else { 1.0 - p };
        let reset = (i.saturating_sub(2)..(i + 4).min(lines.len()))
            .filter_map(|j| lines.get(j))
            .find(|l| has_any(l, &RESET_WORDS) && l.chars().count() <= 80)
            .cloned();
        let entry = (used.clamp(0.0, 1.0), reset);
        match kind {
            "session" if session.is_none() => session = Some(entry),
            "weekly" if weekly.is_none() => weekly = Some(entry),
            "session" | "weekly" => {}
            _ => loose.push(entry),
        }
    }
    // Unlabelled figures fill whichever window is still missing, in page order: the page lists the
    // five-hour window first and the week second. A figure is never dropped just because its heading
    // was not recognised — that is how a maxed-out "Uso atual" went missing while the week showed.
    let mut loose = loose.into_iter();
    if session.is_none() {
        session = loose.next();
    }
    if weekly.is_none() {
        weekly = loose.next();
    }
    if session.is_none() && weekly.is_none() {
        return None;
    }
    let mut windows = Vec::new();
    let mut notes = Vec::new();
    for (id, label, w) in [("session", "5-Hour Limit", session), ("weekly", "Weekly limit", weekly)] {
        if let Some((used, reset)) = w {
            let resets_at = reset.as_deref().and_then(|r| reset_at(r, now));
            windows.push(LimitWindow {
                id: id.into(),
                label: label.into(),
                used,
                resets_at,
                count: None,
                derived: false,
                group: None,
            });
            // Wording that became a moment is shown by the card as "Resets at …"; only what could
            // not be read that way stays as a note
            if let (None, Some(r)) = (resets_at, reset) {
                if !notes.contains(&r) {
                    notes.push(r);
                }
            }
        }
    }
    Some((windows, notes.join(" · ")))
}

fn sleep_interruptible(secs: u64) {
    for _ in 0..secs {
        if REFRESH.swap(false, Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Reads on a timer once a session exists; before that it waits for a sign-in.
pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        let _ = app.emit("geminiapp", &current(&app));
        // The first reading waits for the notch to settle; the window it builds is not free
        sleep_interruptible(20);
        loop {
            if current(&app).status == "needsAuth" {
                // No session yet: nothing to read until the sign-in window closes or a ring click asks
                while !REFRESH.swap(false, Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_secs(1));
                }
                read(&app);
                continue;
            }
            read(&app);
            sleep_interruptible(POLL_SECS);
        }
    });
}

fn read(app: &AppHandle) {
    if READING.swap(true, Ordering::SeqCst) {
        return;
    }
    if SIGNING_IN.load(Ordering::SeqCst) {
        // The visible window reads on its own page loads
        READING.store(false, Ordering::SeqCst);
        return;
    }
    if window(app, false).is_none() {
        READING.store(false, Ordering::SeqCst);
        return;
    }
    // A page that never answers must not leave the window behind
    for _ in 0..45 {
        std::thread::sleep(Duration::from_secs(1));
        if !READING.load(Ordering::SeqCst) {
            return;
        }
    }
    crate::applog("gemini app: the usage page did not answer within 45 s");
    let prev = current(app);
    if prev.status == "ok" {
        broadcast(app, UsageSnapshot { status: "stale".into(), ..prev });
    }
    finish_reading(app);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn used(ws: &[LimitWindow], id: &str) -> Option<f64> {
        ws.iter().find(|w| w.id == id).map(|w| (w.used * 100.0).round())
    }

    #[test]
    fn english_used_figures() {
        let t = "Usage limits\n5-hour limit\n42% used\nResets in 2 hr 10 min\nWeekly limit\n18% used\nResets Monday";
        let (ws, note) = parse(t).unwrap();
        assert_eq!(used(&ws, "session"), Some(42.0));
        assert_eq!(used(&ws, "weekly"), Some(18.0));
        assert!(note.contains("resets in 2 hr"));
    }

    #[test]
    fn portuguese_remaining_figures_flip() {
        let t = "Limites de uso\nLimite de 5 horas\n70% restante\nRenova em 1 h\nLimite semanal\n95 % restante";
        let (ws, _) = parse(t).unwrap();
        assert_eq!(used(&ws, "session"), Some(30.0));
        assert_eq!(used(&ws, "weekly"), Some(5.0));
    }

    /// The page as it really reads in Portuguese (November 2026): the five-hour window is headed
    /// "Uso atual", never "5 horas", and both resets are printed as clock times, the week with a day.
    const PT_PAGE: &str = "Gemini\nLimites de uso\nPRO\n\nOs limites do seu plano determinam quanto você pode usar o Gemini ao longo do tempo. Modelos e recursos avançados podem consumir mais desse limite. Saiba mais\n\nAtualizado agora\n\nUso atual\ninfo\nQuantidade usada: 100%\n0:55: o limite de uso é redefinido\nLimite semanal\n17 de nov., 23:55: o limite de uso é redefinido\nQuantidade usada: 6%\nAumente o limite de uso em 5x com o AI Ultra\nR$ 779,90/mês";

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> chrono::DateTime<chrono::Local> {
        use chrono::TimeZone;
        chrono::Local.with_ymd_and_hms(y, mo, d, h, mi, 0).earliest().unwrap()
    }

    #[test]
    fn current_usage_is_the_five_hour_window() {
        let now = at(2026, 11, 11, 23, 57);
        let (ws, note) = parse_at(PT_PAGE, now).unwrap();
        assert_eq!(used(&ws, "session"), Some(100.0));
        assert_eq!(used(&ws, "weekly"), Some(6.0));
        let resets = |id: &str| ws.iter().find(|w| w.id == id).and_then(|w| w.resets_at);
        // 0:55 has passed today, so it is tomorrow's; the week names its own day
        assert_eq!(resets("session"), Some(at(2026, 11, 12, 0, 55).timestamp_millis() as u64));
        assert_eq!(resets("weekly"), Some(at(2026, 11, 17, 23, 55).timestamp_millis() as u64));
        // Both resets became moments, so no wording is left over for the note
        assert!(note.is_empty(), "{note}");
    }

    #[test]
    fn an_unrecognised_heading_still_fills_the_missing_window() {
        let (ws, _) = parse("Algo novo\n80%\nLimite semanal\n10%").unwrap();
        assert_eq!(used(&ws, "session"), Some(80.0));
        assert_eq!(used(&ws, "weekly"), Some(10.0));
    }

    #[test]
    fn reset_times() {
        let now = at(2026, 11, 11, 23, 57);
        assert_eq!(clock_in("0:55: o limite"), Some((0, 55)));
        assert_eq!(clock_in("resets at 12:55 pm"), Some((12, 55)));
        assert_eq!(clock_in("resets at 12:05 am"), Some((0, 5)));
        assert_eq!(clock_in("r$ 779,90/mês"), None);
        assert_eq!(date_in("17 de nov., 23:55"), Some((11, 17)));
        assert_eq!(date_in("resets nov 17 at 11:55 pm"), Some((11, 17)));
        assert_eq!(date_in("mais 2 hours ago out of 5"), None);
        // A day already more than a day behind belongs to next year
        assert_eq!(reset_at("2 de jan., 10:00", now), Some(at(2027, 1, 2, 10, 0).timestamp_millis() as u64));
        assert_eq!(reset_at("redefinido em breve", now), None);
    }

    #[test]
    fn unlabelled_figures_go_in_page_order() {
        let (ws, _) = parse("Uso\n12%\n3%").unwrap();
        assert_eq!(used(&ws, "session"), Some(12.0));
        assert_eq!(used(&ws, "weekly"), Some(3.0));
    }

    #[test]
    fn no_figures_is_none() {
        assert!(parse("Sign in to continue").is_none());
    }

    #[test]
    fn decodes_encode_uri_component() {
        assert_eq!(percent_decode("%7B%22a%22%3A%22%C3%A9%22%7D"), "{\"a\":\"é\"}");
    }
}
