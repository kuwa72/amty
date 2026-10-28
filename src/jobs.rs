//! Scheduled jobs: prompts that fire on a timer inside the running app.
//! Jobs persist as `<data_dir>/jobs.json` (atomic tmp+rename like notes and
//! sessions). A job's `spec` is `"every 30m|6h|1d"` or `"at HH:MM"` (local
//! time); the runner ticks on a 30s interval and feeds due prompts through
//! the normal `agent::start_run` path so tools, approvals and the session
//! transcript all apply.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub name: String,
    /// `"every 30m" | "every 6h" | "every 1d" | "at 09:00"` (local time).
    pub spec: String,
    pub prompt: String,
    /// Session id; empty = a fresh `[job] <name>` session each fire.
    #[serde(default)]
    pub session: String,
    #[serde(default = "tru")]
    pub enabled: bool,
    /// Unix time of the next fire.
    pub next_run: u64,
    #[serde(default)]
    pub last_run: u64,
    #[serde(default)]
    pub last_status: String,
}

fn tru() -> bool {
    true
}

/// Parsed schedule spec.
#[derive(Clone, Debug, PartialEq)]
pub enum Spec {
    /// Fire every N seconds.
    Every(u64),
    /// Fire daily at local hh:mm.
    Daily { hh: u32, mm: u32 },
}

pub fn parse_spec(s: &str) -> Result<Spec, String> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("every ") {
        return Ok(Spec::Every(parse_dur(rest.trim())?));
    }
    if let Some(rest) = s.strip_prefix("at ") {
        let (hh, mm) = rest
            .trim()
            .split_once(':')
            .ok_or_else(|| format!("at wants HH:MM, got '{rest}'"))?;
        let hh: u32 = hh.parse().map_err(|_| format!("bad hour '{hh}'"))?;
        let mm: u32 = mm.parse().map_err(|_| format!("bad minute '{mm}'"))?;
        if hh > 23 || mm > 59 {
            return Err(format!("at out of range: {hh:02}:{mm:02}"));
        }
        return Ok(Spec::Daily { hh, mm });
    }
    Err(format!("unknown spec '{s}' — want 'every 30m' or 'at 09:00'"))
}

/// `30s` `15m` `6h` `1d` (optionally combined: `1h30m`).
fn parse_dur(s: &str) -> Result<u64, String> {
    let mut total = 0u64;
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let n: u64 = num.parse().map_err(|_| format!("bad duration '{s}'"))?;
        num.clear();
        let mult = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            _ => return Err(format!("bad unit '{c}' in '{s}'")),
        };
        total += n * mult;
    }
    if !num.is_empty() {
        return Err(format!("dangling number in '{s}'"));
    }
    if total == 0 {
        return Err("duration must be > 0".into());
    }
    Ok(total)
}

/// Next fire time strictly after `now` (unix secs).
pub fn next_after(spec: &Spec, now: u64) -> u64 {
    match *spec {
        Spec::Every(secs) => now + secs,
        Spec::Daily { hh, mm } => {
            let now = now as i64;
            let day0 = (now + utc_offset_at(now as u64)).div_euclid(86_400);
            // Today first, then later days; the wall target's own day may sit
            // on a different UTC offset (DST), so refine once per candidate.
            for d in 0..3 {
                let wall = (day0 + d) * 86_400 + (hh as i64) * 3600 + (mm as i64) * 60;
                let rough = wall - utc_offset_at(now as u64);
                if rough <= now {
                    continue;
                }
                let t = wall - utc_offset_at(rough as u64);
                if t > now {
                    return t as u64;
                }
            }
            now as u64 + 86_400 // unreachable in practice
        }
    }
}

/// Local offset from UTC in seconds for instant `t`, via the OS tz database
/// (so DST transitions are handled). Zero on failure — the job then fires in
/// UTC, which is the documented fallback.
fn utc_offset_at(t: u64) -> i64 {
    #[cfg(unix)]
    unsafe {
        let tt = t as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        if !libc::localtime_r(&tt, &mut tm).is_null() {
            return tm.tm_gmtoff;
        }
        0
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Time::*;
        // FILETIME = 100ns ticks since 1601-01-01 UTC.
        let ft = ((t + 11_644_473_600) * 10_000_000) as u64;
        let ftc = FILETIME {
            dwLowDateTime: ft as u32,
            dwHighDateTime: (ft >> 32) as u32,
        };
        unsafe {
            let mut utc_st: SYSTEMTIME = std::mem::zeroed();
            let mut loc_st: SYSTEMTIME = std::mem::zeroed();
            let mut loc_ft: FILETIME = std::mem::zeroed();
            if FileTimeToSystemTime(&ftc, &mut utc_st) == 0
                || SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc_st, &mut loc_st) == 0
                || SystemTimeToFileTime(&loc_st, &mut loc_ft) == 0
            {
                return 0;
            }
            let wall = ((loc_ft.dwHighDateTime as u64) << 32) | loc_ft.dwLowDateTime as u64;
            (wall as i64 - ft as i64) / 10_000_000
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = t;
        0
    }
}

pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join("jobs.json")
}

pub fn load(data_dir: &Path) -> Vec<Job> {
    let p = path(data_dir);
    std::fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(data_dir: &Path, jobs: &[Job]) -> Result<(), String> {
    let p = path(data_dir);
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let tmp = p.with_extension("json.tmp");
    let body = serde_json::to_string_pretty(jobs).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, body).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &p).map_err(|e| format!("rename to {}: {e}", p.display()))
}

pub fn find_mut<'a>(jobs: &'a mut [Job], id_or_name: &str) -> Option<&'a mut Job> {
    jobs.iter_mut()
        .find(|j| j.id == id_or_name || j.name == id_or_name || j.id.starts_with(id_or_name))
}

/// Fire one job now; returns the recorded status string. `session`-less jobs
/// get a fresh `[job] <name>` session each time.
fn fire(app: &std::sync::Arc<crate::app::App>, j: &Job) -> String {
    let sid = if j.session.is_empty() {
        let id = app.sessions.create(None, None);
        app.sessions.set_title(&id, &format!("[job] {}", j.name));
        id
    } else {
        match app.sessions.resolve(&j.session) {
            Some(s) => s,
            None => return "session gone".into(),
        }
    };
    let prompt = format!("[scheduled job: {}]\n\n{}", j.name, j.prompt);
    match crate::agent::start_run(app, &sid, prompt) {
        Ok(()) => "started".into(),
        Err(e) => format!("failed: {e}"),
    }
}

/// Scheduler tick: fire every enabled job whose `next_run` passed. Called
/// once at startup (so jobs missed while down fire once) then every ~30 s.
pub fn tick(app: &std::sync::Arc<crate::app::App>) {
    let now = crate::session::now_secs();
    let mut jobs = app.jobs.lock().unwrap();
    let mut dirty = false;
    for j in jobs.iter_mut() {
        if !j.enabled || j.next_run > now {
            continue;
        }
        match parse_spec(&j.spec) {
            Ok(spec) => j.next_run = next_after(&spec, now),
            Err(e) => {
                j.enabled = false;
                j.last_status = format!("bad spec: {e}");
                dirty = true;
                continue;
            }
        }
        j.last_run = now;
        j.last_status = fire(app, j);
        dirty = true;
    }
    if dirty {
        let _ = save(&crate::config::data_dir(), &jobs);
    }
}

pub fn add(
    app: &std::sync::Arc<crate::app::App>,
    name: &str,
    spec: &str,
    prompt: &str,
    session: &str,
) -> Result<Job, String> {
    let parsed = parse_spec(spec)?;
    if prompt.trim().is_empty() {
        return Err("prompt required".into());
    }
    let job = Job {
        id: uuid::Uuid::new_v4().simple().to_string()[..8].into(),
        name: if name.trim().is_empty() {
            prompt.chars().take(24).collect()
        } else {
            name.trim().into()
        },
        spec: spec.trim().into(),
        prompt: prompt.into(),
        session: session.into(),
        enabled: true,
        next_run: next_after(&parsed, crate::session::now_secs()),
        last_run: 0,
        last_status: String::new(),
    };
    let mut jobs = app.jobs.lock().unwrap();
    jobs.push(job.clone());
    save(&crate::config::data_dir(), &jobs)?;
    Ok(job)
}

pub fn remove(app: &std::sync::Arc<crate::app::App>, id_or_name: &str) -> Result<(), String> {
    let mut jobs = app.jobs.lock().unwrap();
    let before = jobs.len();
    jobs.retain(|j| !(j.id == id_or_name || j.name == id_or_name || j.id.starts_with(id_or_name)));
    if jobs.len() == before {
        return Err(format!("no job '{id_or_name}'"));
    }
    save(&crate::config::data_dir(), &jobs)
}

pub fn set_enabled(
    app: &std::sync::Arc<crate::app::App>,
    id_or_name: &str,
    enabled: bool,
) -> Result<(), String> {
    let mut jobs = app.jobs.lock().unwrap();
    let j = find_mut(&mut jobs, id_or_name).ok_or_else(|| format!("no job '{id_or_name}'"))?;
    j.enabled = enabled;
    save(&crate::config::data_dir(), &jobs)
}

/// Run a job immediately regardless of schedule; returns the status string.
pub fn run_now(app: &std::sync::Arc<crate::app::App>, id_or_name: &str) -> Result<String, String> {
    let now = crate::session::now_secs();
    let mut jobs = app.jobs.lock().unwrap();
    let j = find_mut(&mut jobs, id_or_name).ok_or_else(|| format!("no job '{id_or_name}'"))?;
    j.last_run = now;
    if let Ok(spec) = parse_spec(&j.spec) {
        j.next_run = next_after(&spec, now);
    }
    j.last_status = fire(app, j);
    let status = j.last_status.clone();
    let _ = save(&crate::config::data_dir(), &jobs);
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("amty-jobs-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn parse_every_durations() {
        assert_eq!(parse_spec("every 30m"), Ok(Spec::Every(1800)));
        assert_eq!(parse_spec("every 6h"), Ok(Spec::Every(21600)));
        assert_eq!(parse_spec("every 1d"), Ok(Spec::Every(86400)));
        assert_eq!(parse_spec("every 1h30m"), Ok(Spec::Every(5400)));
        assert_eq!(parse_spec("every 45s"), Ok(Spec::Every(45)));
        assert!(parse_spec("every 0s").is_err());
        assert!(parse_spec("every soon").is_err());
    }

    #[test]
    fn parse_daily_at() {
        assert_eq!(parse_spec("at 09:00"), Ok(Spec::Daily { hh: 9, mm: 0 }));
        assert_eq!(parse_spec("at 23:59"), Ok(Spec::Daily { hh: 23, mm: 59 }));
        assert!(parse_spec("at 24:00").is_err());
        assert!(parse_spec("at 9:60").is_err());
        assert!(parse_spec("at noon").is_err());
    }

    #[test]
    fn unknown_specs_rejected() {
        assert!(parse_spec("*/5 * * * *").is_err()); // no cron syntax
        assert!(parse_spec("").is_err());
        assert!(parse_spec("hourly").is_err());
    }

    #[test]
    fn every_fires_in_future() {
        let now = 1_700_000_000;
        assert_eq!(next_after(&Spec::Every(1800), now), now + 1800);
    }

    #[test]
    fn daily_fires_next_wall_time() {
        // 2023-11-14 00:00 UTC. "at 09:00" local must land strictly in the
        // future and, converted back with the same offset, read as 09:00.
        let now = 1_699_920_000;
        let spec = Spec::Daily { hh: 9, mm: 0 };
        let next = next_after(&spec, now);
        assert!(next > now);
        assert!(next <= now + 86_400); // == when `now` is exactly the target time
        let local = (next as i64 + utc_offset_at(now)).rem_euclid(86_400);
        assert_eq!(local, 9 * 3600);
        // and again later the same day → tomorrow's slot
        let next2 = next_after(&spec, next);
        assert_eq!(next2, next + 86_400);
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tmp_dir();
        let jobs = vec![Job {
            id: "abc123".into(),
            name: "morning".into(),
            spec: "at 09:00".into(),
            prompt: "summarize news".into(),
            session: String::new(),
            enabled: true,
            next_run: 1_700_000_000,
            last_run: 0,
            last_status: String::new(),
        }];
        save(&dir, &jobs).unwrap();
        let got = load(&dir);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "morning");
        assert_eq!(got[0].spec, "at 09:00");
        // missing file → empty, not an error
        let _ = std::fs::remove_file(path(&dir));
        assert!(load(&dir).is_empty());
    }

    #[test]
    fn find_matches_id_prefix_and_name() {
        let mut jobs = vec![
            Job {
                id: "a1b2c3".into(),
                name: "nightly".into(),
                spec: "every 1h".into(),
                prompt: "x".into(),
                session: String::new(),
                enabled: true,
                next_run: 0,
                last_run: 0,
                last_status: String::new(),
            },
        ];
        assert!(find_mut(&mut jobs, "a1b2").is_some());
        assert!(find_mut(&mut jobs, "nightly").is_some());
        assert!(find_mut(&mut jobs, "zzz").is_none());
    }
}
