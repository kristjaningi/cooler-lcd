//! Live aircraft around a point, from adsb.lol's free API (aircraft heard by
//! community ADS-B receivers; no account or key).
//!
//! Fetched on a background thread, and only while the radar has been on
//! screen recently, so a carousel that rarely shows it makes few requests.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use serde_json::Value;

/// adsb.lol rate-limits by load without saying how much is allowed;
/// positions are dead reckoned between polls, so a slow rate still moves
/// smoothly.
const POLL: Duration = Duration::from_secs(10);
/// First wait after HTTP 429, doubling while it persists.
const RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(30);
const MAX_BACKOFF: Duration = Duration::from_secs(5 * 60);
/// Stop fetching once the radar has been off screen this long.
const IDLE: Duration = Duration::from_secs(20);

#[derive(Clone, Debug)]
pub struct Aircraft {
    /// ICAO 24-bit address, stable per airframe.
    pub hex: String,
    /// Callsign, else registration, else the hex address.
    pub ident: String,
    pub lat: f32,
    pub lon: f32,
    /// Barometric altitude in feet; `None` on the ground.
    pub alt: Option<i32>,
    /// Ground speed in knots.
    pub gs: Option<f32>,
    /// Track over ground, degrees true.
    pub track: Option<f32>,
    /// Climb (+) or descent (-) in feet per minute.
    pub vrate: Option<i32>,
    /// Squawking 7500, 7600 or 7700, or flagged as an emergency.
    pub emergency: bool,
    /// When the position was measured.
    pub fixed_at: Instant,
}

#[derive(Default)]
pub struct State {
    /// The latest aircraft list and its sequence number, which increases
    /// with every successful fetch.
    pub aircraft: Vec<Aircraft>,
    pub seq: u64,
    pub error: Option<String>,
    /// Last time the radar was drawn; fetching pauses when this gets old.
    wanted_at: Option<Instant>,
}

impl State {
    pub fn want(&mut self) {
        self.wanted_at = Some(Instant::now());
    }
}

pub type Shared = Arc<Mutex<State>>;

pub fn spawn(lat: f32, lon: f32, radius_nm: u32) -> Shared {
    let shared = Shared::default();
    let url = format!("https://api.adsb.lol/v2/point/{lat}/{lon}/{radius_nm}");
    let s = shared.clone();
    thread::spawn(move || run(&s, &url));
    shared
}

fn run(shared: &Shared, url: &str) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .http_status_as_error(false)
        .user_agent(concat!("cooler-lcd/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let mut backoff = POLL;
    loop {
        let wanted = shared
            .lock()
            .unwrap()
            .wanted_at
            .is_some_and(|t| t.elapsed() < IDLE);
        if !wanted {
            thread::sleep(Duration::from_millis(500));
            continue;
        }
        let wait = match fetch(&agent, url) {
            Ok(aircraft) => {
                let mut s = shared.lock().unwrap();
                if s.error.take().is_some() {
                    eprintln!("radar: adsb.lol reachable again");
                }
                s.aircraft = aircraft;
                s.seq += 1;
                backoff = POLL;
                POLL
            }
            Err(e) => {
                backoff = match e {
                    FetchError::RateLimited(retry_after) => (backoff * 2)
                        .max(RATE_LIMIT_BACKOFF)
                        .max(retry_after.unwrap_or_default()),
                    FetchError::Other(_) => backoff * 2,
                }
                .min(MAX_BACKOFF);
                let msg = match e {
                    FetchError::RateLimited(_) => "adsb.lol: rate limited".to_string(),
                    FetchError::Other(e) => format!("{e:#}"),
                };
                let mut s = shared.lock().unwrap();
                if s.error.as_ref() != Some(&msg) {
                    eprintln!("radar: {msg}; retrying in {}s", backoff.as_secs());
                }
                s.error = Some(msg);
                backoff
            }
        };
        thread::sleep(wait);
    }
}

enum FetchError {
    RateLimited(Option<Duration>),
    Other(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for FetchError {
    fn from(e: E) -> Self {
        FetchError::Other(e.into())
    }
}

fn fetch(agent: &ureq::Agent, url: &str) -> Result<Vec<Aircraft>, FetchError> {
    let mut resp = agent.get(url).call().context("adsb.lol")?;
    match resp.status().as_u16() {
        200 => {}
        429 => {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok()?.parse().ok())
                .map(Duration::from_secs);
            return Err(FetchError::RateLimited(retry_after));
        }
        status => return Err(anyhow!("adsb.lol: HTTP {status}").into()),
    }
    let body = resp.body_mut().read_to_string()?;
    Ok(parse(&body, Instant::now())?)
}

#[derive(Deserialize)]
struct Response {
    #[serde(default)]
    ac: Vec<Entry>,
}

#[derive(Deserialize)]
struct Entry {
    hex: String,
    #[serde(rename = "type", default)]
    kind: String,
    flight: Option<String>,
    r: Option<String>,
    lat: Option<f32>,
    lon: Option<f32>,
    alt_baro: Option<Value>,
    gs: Option<f32>,
    track: Option<f32>,
    baro_rate: Option<i32>,
    geom_rate: Option<i32>,
    squawk: Option<String>,
    emergency: Option<String>,
    seen_pos: Option<f32>,
}

fn parse(body: &str, now: Instant) -> Result<Vec<Aircraft>> {
    let resp: Response = serde_json::from_str(body).context("parsing adsb.lol response")?;
    Ok(resp
        .ac
        .into_iter()
        // Fixed ground transmitters (towers, test beacons) are not traffic.
        .filter(|e| !e.kind.ends_with("_nt"))
        .filter_map(|e| {
            let ident = [e.flight.as_deref(), e.r.as_deref()]
                .into_iter()
                .flatten()
                .map(str::trim)
                .find(|s| !s.is_empty())
                .unwrap_or(&e.hex)
                .to_string();
            let alt = match &e.alt_baro {
                Some(Value::Number(n)) => n.as_f64().map(|a| a as i32),
                _ => None,
            };
            let squawk_emergency = matches!(e.squawk.as_deref(), Some("7500" | "7600" | "7700"));
            let flagged = e.emergency.as_deref().is_some_and(|s| s != "none");
            let age = Duration::from_secs_f32(e.seen_pos.unwrap_or(0.0).clamp(0.0, 60.0));
            Some(Aircraft {
                lat: e.lat?,
                lon: e.lon?,
                ident,
                alt,
                gs: e.gs,
                track: e.track,
                vrate: e.baro_rate.or(e.geom_rate),
                emergency: squawk_emergency || flagged,
                fixed_at: now.checked_sub(age).unwrap_or(now),
                hex: e.hex,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_adsb_lol() {
        let body = r#"{"ac":[
            {"hex":"4cc516","type":"adsb_icao","flight":"ICE27Y  ","r":"TF-FXH",
             "alt_baro":1075,"gs":102.1,"track":357.75,"geom_rate":-576,
             "squawk":"2313","emergency":"none","lat":64.1002,"lon":-21.9322,"seen_pos":0.2},
            {"hex":"4cc088","type":"adsb_icao_nt","r":"TWR","alt_baro":"ground",
             "lat":63.977,"lon":-21.634},
            {"hex":"4cc2aa","type":"adsb_icao","alt_baro":"ground","squawk":"7700",
             "lat":63.985,"lon":-22.6},
            {"hex":"abcdef","type":"mlat"}
        ]}"#;
        let ac = parse(body, Instant::now()).unwrap();
        assert_eq!(ac.len(), 2);
        assert_eq!((ac[0].ident.as_str(), ac[0].alt), ("ICE27Y", Some(1075)));
        assert_eq!(ac[0].vrate, Some(-576));
        assert!(!ac[0].emergency);
        assert_eq!((ac[1].ident.as_str(), ac[1].alt), ("4cc2aa", None));
        assert!(ac[1].emergency);
    }
}
