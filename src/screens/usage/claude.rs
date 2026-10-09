//! Claude plan usage from the OAuth usage endpoint that Claude Code's `/usage`
//! uses. It is undocumented, so everything here fails soft: on any problem
//! the screen keeps the last numbers and shows their age.
//!
//! Fetched on a background thread, politely: every 5 minutes, backing off on
//! errors and 429s, and never with a token that is expired or was rejected.
//! The token is only read from `~/.claude/.credentials.json` (Claude Code
//! refreshes it while it runs) and is only sent to api.anthropic.com.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde_json::Value;

use super::Limit;

const URL: &str = "https://api.anthropic.com/api/oauth/usage";
const POLL: Duration = Duration::from_secs(5 * 60);
const MAX_BACKOFF: Duration = Duration::from_secs(60 * 60);
/// How often to look for a fresh token while ours is expired or rejected.
/// Only reads a local file.
const TOKEN_CHECK: Duration = Duration::from_secs(60);

pub struct ClaudeUsage {
    pub limits: Vec<Limit>,
    pub fetched_at: SystemTime,
}

pub type Shared = Arc<Mutex<Option<ClaudeUsage>>>;

/// Starts the fetcher thread and returns where it publishes results.
pub fn spawn() -> Shared {
    let shared: Shared = Arc::default();
    let out = shared.clone();
    if let Err(e) = thread::Builder::new()
        .name("claude-usage".into())
        .spawn(move || run(&out))
    {
        eprintln!("claude usage: can't start fetcher: {e}");
    }
    shared
}

enum FetchError {
    Unauthorized,
    RateLimited(Option<Duration>),
    Other(anyhow::Error),
}

fn run(shared: &Shared) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut backoff = POLL;
    let mut rejected: Option<String> = None;
    let mut last_error = String::new();

    loop {
        let wait = match read_token() {
            None => TOKEN_CHECK,
            Some(token) if rejected.as_ref() == Some(&token) => TOKEN_CHECK,
            Some(token) => match fetch(&agent, &token) {
                Ok(limits) => {
                    *shared.lock().unwrap() = Some(ClaudeUsage {
                        limits,
                        fetched_at: SystemTime::now(),
                    });
                    last_error.clear();
                    backoff = POLL;
                    POLL
                }
                Err(FetchError::Unauthorized) => {
                    report(
                        &mut last_error,
                        "token rejected; waiting for Claude Code to refresh it".into(),
                    );
                    rejected = Some(token);
                    TOKEN_CHECK
                }
                Err(FetchError::RateLimited(retry_after)) => {
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    report(
                        &mut last_error,
                        format!("rate limited; backing off {}m", backoff.as_secs() / 60),
                    );
                    retry_after.map_or(backoff, |r| r.max(backoff))
                }
                Err(FetchError::Other(e)) => {
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    report(&mut last_error, format!("{e:#}"));
                    backoff
                }
            },
        };
        thread::sleep(wait);
    }
}

/// Logs `msg` unless it repeats the previous one.
fn report(last: &mut String, msg: String) {
    if msg != *last {
        eprintln!("claude usage: {msg}");
        *last = msg;
    }
}

fn fetch(agent: &ureq::Agent, token: &str) -> Result<Vec<Limit>, FetchError> {
    let mut resp = agent
        .get(URL)
        .header("Authorization", format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .call()
        .map_err(|e| FetchError::Other(e.into()))?;
    match resp.status().as_u16() {
        200 => {}
        401 | 403 => return Err(FetchError::Unauthorized),
        429 => {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok()?.parse().ok())
                .map(Duration::from_secs);
            return Err(FetchError::RateLimited(retry_after));
        }
        code => return Err(FetchError::Other(anyhow!("HTTP {code}"))),
    }
    let body = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| FetchError::Other(e.into()))?;
    parse(&body).map_err(FetchError::Other)
}

fn parse(body: &str) -> Result<Vec<Limit>> {
    let json: Value = serde_json::from_str(body).context("parsing usage response")?;
    let limits: Vec<Limit> = [("five_hour", "5h"), ("seven_day", "week")]
        .into_iter()
        .filter_map(|(key, label)| {
            let window = json.get(key)?;
            Some(Limit {
                label: label.into(),
                used: window.get("utilization")?.as_f64()? as f32,
                resets_at: window
                    .get("resets_at")
                    .and_then(Value::as_str)
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|t| t.with_timezone(&Utc)),
            })
        })
        .collect();
    if limits.is_empty() {
        return Err(anyhow!("usage response has no five_hour/seven_day windows"));
    }
    Ok(limits)
}

/// The OAuth access token, unless it is missing or about to expire.
fn read_token() -> Option<String> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let text = fs::read_to_string(home.join(".claude/.credentials.json")).ok()?;
    let json: Value = serde_json::from_str(&text).ok()?;
    let oauth = json.get("claudeAiOauth")?;
    let expires_ms = oauth.get("expiresAt")?.as_i64()?;
    if expires_ms <= Utc::now().timestamp_millis() + 60_000 {
        return None;
    }
    Some(oauth.get("accessToken")?.as_str()?.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_usage_windows() {
        let body = r#"{
            "five_hour": {"utilization": 12.0, "resets_at": "2026-10-09T19:19:59.529101+00:00"},
            "seven_day": {"utilization": 2.0, "resets_at": null},
            "iguana_necktie": {"utilization": 9.7}
        }"#;
        let limits = parse(body).unwrap();
        assert_eq!(limits.len(), 2);
        assert_eq!((limits[0].label.as_str(), limits[0].used), ("5h", 12.0));
        assert!(limits[0].resets_at.is_some());
        assert_eq!(
            (limits[1].label.as_str(), limits[1].resets_at),
            ("week", None)
        );
    }

    #[test]
    fn rejects_unexpected_shape() {
        assert!(parse(r#"{"something_else": {}}"#).is_err());
        assert!(parse("not json").is_err());
    }
}
