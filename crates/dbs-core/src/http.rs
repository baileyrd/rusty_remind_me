//! A small managed HTTP client wrapper for connectors that opt in
//! (`wants_managed_http`).
//!
//! Mirrors `src/dbs/core/http.py` in baileyrd/Daily-Backup-System
//! (pinned `@6cc6491`): retry with exponential backoff + jitter on
//! transient failures (network errors, 5xx, and 429), honoring
//! `Retry-After` (both delta-seconds and HTTP-date forms) capped at
//! `max_retry_after`, immediate return on a non-429 4xx (a client error
//! won't fix itself by retrying), and optional pre-emptive rate
//! limiting.
//!
//! **Blocking, not async** — `reqwest::blocking`, not `tokio`. The rest
//! of this crate is synchronous by design (`Connector::fetch` returns a
//! plain `Iterator`, not a `Stream`); threading async through the whole
//! connector trait would be a far bigger, more invasive change than this
//! issue warrants. `reqwest::blocking` runs its own internal runtime, so
//! this doesn't require adding `tokio` as an explicit dependency of this
//! crate. `gap-analysis.md`'s foundational-dependency decision named
//! `tokio` for a *future* need (most plausibly the web tier); revisit
//! then, not preemptively here.
//!
//! Jitter/backoff use a deterministic LCG (no global RNG) so behavior is
//! reproducible in tests, same as the reference.

use std::collections::VecDeque;
use std::fmt;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::errors::ConnectorError;

const RETRY_STATUS: [u16; 5] = [429, 500, 502, 503, 504];

/// Seconds to wait, from either `Retry-After` form (delta-seconds or an
/// HTTP-date). `now` is injectable for deterministic tests; a past
/// HTTP-date clamps to zero.
fn parse_retry_after(value: Option<&str>, now: Option<DateTime<Utc>>) -> Option<Duration> {
    let value = value?;
    if let Ok(secs) = value.trim().parse::<f64>() {
        return Some(Duration::from_secs_f64(secs.max(0.0)));
    }
    let when = DateTime::parse_from_rfc2822(value)
        .ok()?
        .with_timezone(&Utc);
    let reference = now.unwrap_or_else(Utc::now);
    let millis = (when - reference).num_milliseconds().max(0);
    Some(Duration::from_millis(millis as u64))
}

/// Outcome of a request that didn't return a usable response.
#[derive(Debug)]
pub enum HttpError {
    /// Retries exhausted on a transient failure or repeated 429s.
    Exhausted(ConnectorError),
    /// A non-retryable HTTP status (any 4xx/5xx not in the retry set,
    /// most commonly a non-429 4xx). Raised as-is, matching the
    /// reference's `response.raise_for_status()` — not wrapped in a
    /// `ConnectorError`. A connector's own `fetch()` must catch and
    /// reclassify this if a given status should be treated as
    /// config/auth/transient instead of aborting the run. `headers`
    /// are the response's own (captured before `reqwest` consumes the
    /// response converting it to an error) — some APIs distinguish two
    /// different failure modes under the same status code via a header
    /// (e.g. GitHub's 403 rate-limit-exhausted vs. 403
    /// token-lacks-access, told apart by `X-RateLimit-Remaining`).
    Status {
        error: reqwest::Error,
        headers: reqwest::header::HeaderMap,
    },
    /// The response declared a `Content-Length` exceeding the caller's
    /// configured [`ManagedHttpClient::max_response_bytes`] — rejected
    /// before the body is ever read, so an oversized response can never
    /// be buffered into memory. Only catches a server that honestly
    /// declares its size; a response with no `Content-Length` (chunked
    /// transfer-encoding) or a lying one still reaches the caller's own
    /// `.json()`/`.text()`/`.bytes()` uncapped.
    TooLarge { limit: u64, declared: u64 },
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exhausted(e) => write!(f, "{e}"),
            Self::Status { error, .. } => write!(f, "{error}"),
            Self::TooLarge { limit, declared } => write!(
                f,
                "response declared Content-Length {declared} bytes, exceeding the {limit}-byte limit"
            ),
        }
    }
}

impl std::error::Error for HttpError {}

/// Resilient wrapper around a blocking [`reqwest::blocking::Client`].
pub struct ManagedHttpClient {
    client: reqwest::blocking::Client,
    max_attempts: u32,
    rate_limit_per_min: Option<u32>,
    base_backoff: Duration,
    max_backoff: Duration,
    max_retry_after: Duration,
    max_response_bytes: Option<u64>,
    sleep: Box<dyn FnMut(Duration) + Send>,
    request_times: VecDeque<Instant>,
    jitter_state: u32,
}

impl ManagedHttpClient {
    /// `max_attempts=5`, `base_backoff=500ms`, `max_backoff=30s`,
    /// `max_retry_after=300s`, no rate limit, real `std::thread::sleep` —
    /// matching the reference's constructor defaults.
    pub fn new(client: reqwest::blocking::Client) -> Self {
        Self::with_sleep(client, std::thread::sleep)
    }

    /// Like [`Self::new`] but with an injectable sleep function — tests
    /// pass a no-op to avoid real waits, matching the reference's
    /// `sleep: Callable` parameter.
    pub fn with_sleep(
        client: reqwest::blocking::Client,
        sleep: impl FnMut(Duration) + Send + 'static,
    ) -> Self {
        Self {
            client,
            max_attempts: 5,
            rate_limit_per_min: None,
            base_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(30),
            max_retry_after: Duration::from_secs(300),
            max_response_bytes: None,
            sleep: Box::new(sleep),
            request_times: VecDeque::new(),
            jitter_state: 0x9E37_79B9,
        }
    }

    pub fn max_attempts(mut self, n: u32) -> Self {
        self.max_attempts = n.max(1);
        self
    }

    pub fn rate_limit_per_min(mut self, n: u32) -> Self {
        self.rate_limit_per_min = Some(n);
        self
    }

    pub fn base_backoff(mut self, d: Duration) -> Self {
        self.base_backoff = d;
        self
    }

    pub fn max_backoff(mut self, d: Duration) -> Self {
        self.max_backoff = d;
        self
    }

    pub fn max_retry_after(mut self, d: Duration) -> Self {
        self.max_retry_after = d;
        self
    }

    /// Rejects any response whose `Content-Length` declares more than
    /// `n` bytes, before the body is ever read — a connector opts in
    /// per-call-shape (a JSON API response and a media/enclosure
    /// download have very different legitimate sizes) via this builder,
    /// not a single global default. `None` (the default) applies no
    /// limit, preserving existing behavior for every connector that
    /// doesn't opt in.
    pub fn max_response_bytes(mut self, n: u64) -> Self {
        self.max_response_bytes = Some(n);
        self
    }

    pub fn get(&mut self, url: &str) -> Result<reqwest::blocking::Response, HttpError> {
        self.request(reqwest::Method::GET, url, |b| b)
    }

    /// `build` customizes the request (headers, query params, body, ...)
    /// before it's sent; it may be called more than once across retries.
    pub fn request(
        &mut self,
        method: reqwest::Method,
        url: &str,
        build: impl Fn(reqwest::blocking::RequestBuilder) -> reqwest::blocking::RequestBuilder,
    ) -> Result<reqwest::blocking::Response, HttpError> {
        let mut last_err: Option<ConnectorError> = None;
        for attempt in 1..=self.max_attempts {
            self.throttle();
            let request = build(self.client.request(method.clone(), url));
            let response = match request.send() {
                Ok(r) => r,
                Err(e) => {
                    last_err = Some(ConnectorError::Transient(format!(
                        "{method} {url} failed: {e}"
                    )));
                    self.backoff(attempt, None);
                    continue;
                }
            };

            let status = response.status();
            if RETRY_STATUS.contains(&status.as_u16()) {
                let retry_after = parse_retry_after(
                    response
                        .headers()
                        .get("Retry-After")
                        .and_then(|v| v.to_str().ok()),
                    None,
                );
                last_err = Some(if status.as_u16() == 429 {
                    ConnectorError::RateLimited(format!("{method} {url} -> 429 (rate limited)"))
                } else {
                    ConnectorError::Transient(format!("{method} {url} -> {status}"))
                });
                if attempt < self.max_attempts {
                    self.backoff(attempt, retry_after);
                    continue;
                }
                break;
            }

            if status.is_client_error() || status.is_server_error() {
                let headers = response.headers().clone();
                return Err(HttpError::Status {
                    error: response.error_for_status().unwrap_err(),
                    headers,
                });
            }
            if let Some(limit) = self.max_response_bytes {
                if let Some(declared) = response.content_length() {
                    if declared > limit {
                        return Err(HttpError::TooLarge { limit, declared });
                    }
                }
            }
            return Ok(response);
        }
        Err(HttpError::Exhausted(last_err.expect(
            "the loop always sets last_err before falling through",
        )))
    }

    fn throttle(&mut self) {
        let Some(limit) = self.rate_limit_per_min else {
            return;
        };
        let now = Instant::now();
        let window = Duration::from_secs(60);
        while let Some(&front) = self.request_times.front() {
            if now.duration_since(front) >= window {
                self.request_times.pop_front();
            } else {
                break;
            }
        }
        if self.request_times.len() >= limit as usize {
            if let Some(&front) = self.request_times.front() {
                let elapsed = now.duration_since(front);
                if elapsed < window {
                    (self.sleep)(window - elapsed);
                }
            }
        }
        self.request_times.push_back(Instant::now());
    }

    fn next_jitter(&mut self) -> f64 {
        self.jitter_state = ((1_103_515_245u64.wrapping_mul(self.jitter_state as u64) + 12345)
            & 0x7FFF_FFFF) as u32;
        self.jitter_state as f64 / 0x7FFF_FFFF as f64
    }

    fn backoff(&mut self, attempt: u32, retry_after: Option<Duration>) {
        if let Some(ra) = retry_after {
            (self.sleep)(ra.min(self.max_retry_after));
            return;
        }
        let delay = self
            .max_backoff
            .min(self.base_backoff * 2u32.saturating_pow(attempt - 1));
        let jitter = self.next_jitter() * self.base_backoff.as_secs_f64();
        (self.sleep)(delay + Duration::from_secs_f64(jitter));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::sync::{Arc, Mutex};

    fn sleepless_client() -> (ManagedHttpClient, Arc<Mutex<Vec<Duration>>>) {
        let sleeps = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&sleeps);
        let client = ManagedHttpClient::with_sleep(reqwest::blocking::Client::new(), move |d| {
            recorder.lock().unwrap().push(d);
        });
        (client, sleeps)
    }

    #[test]
    fn parse_retry_after_delta_seconds() {
        assert_eq!(
            parse_retry_after(Some("120"), None),
            Some(Duration::from_secs(120))
        );
    }

    #[test]
    fn parse_retry_after_negative_delta_clamps_to_zero() {
        assert_eq!(
            parse_retry_after(Some("-5"), None),
            Some(Duration::from_secs(0))
        );
    }

    #[test]
    fn parse_retry_after_http_date_future() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        // 30 seconds after `now`, in HTTP-date (RFC 2822-compatible) form.
        let future = "Thu, 01 Jan 2026 00:00:30 GMT";
        let wait = parse_retry_after(Some(future), Some(now)).unwrap();
        assert_eq!(wait, Duration::from_secs(30));
    }

    #[test]
    fn parse_retry_after_past_http_date_clamps_to_zero() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 1, 0).unwrap();
        let past = "Thu, 01 Jan 2026 00:00:00 GMT";
        assert_eq!(
            parse_retry_after(Some(past), Some(now)),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn parse_retry_after_garbage_falls_back_to_none() {
        assert_eq!(parse_retry_after(Some("not-a-value"), None), None);
    }

    #[test]
    fn parse_retry_after_missing_header_is_none() {
        assert_eq!(parse_retry_after(None, None), None);
    }

    #[test]
    fn jitter_sequence_is_deterministic() {
        let (mut a, _) = sleepless_client();
        let (mut b, _) = sleepless_client();
        let seq_a: Vec<f64> = (0..5).map(|_| a.next_jitter()).collect();
        let seq_b: Vec<f64> = (0..5).map(|_| b.next_jitter()).collect();
        assert_eq!(seq_a, seq_b);
        // And it's not degenerate (not all the same value).
        assert!(seq_a.iter().any(|&v| v != seq_a[0]));
    }

    #[test]
    fn jitter_values_stay_in_unit_range() {
        let (mut client, _) = sleepless_client();
        for _ in 0..50 {
            let v = client.next_jitter();
            assert!((0.0..1.0).contains(&v));
        }
    }

    #[test]
    fn backoff_uses_retry_after_when_given_capped_at_max_retry_after() {
        let (mut client, sleeps) = sleepless_client();
        client = client.max_retry_after(Duration::from_secs(10));
        client.backoff(1, Some(Duration::from_secs(9999)));
        assert_eq!(sleeps.lock().unwrap()[0], Duration::from_secs(10));
    }

    #[test]
    fn backoff_exponential_growth_is_capped_at_max_backoff() {
        let (mut client, sleeps) = sleepless_client();
        client = client
            .base_backoff(Duration::from_millis(100))
            .max_backoff(Duration::from_secs(1));
        client.backoff(10, None); // 100ms * 2^9 would be far over the cap
        let waited = sleeps.lock().unwrap()[0];
        // Capped delay (1s) plus up-to-one-base_backoff jitter (<=100ms).
        assert!(waited >= Duration::from_secs(1));
        assert!(waited <= Duration::from_secs(1) + Duration::from_millis(100));
    }

    #[test]
    fn throttle_sleeps_when_rate_limit_is_exceeded_within_the_window() {
        let (mut client, sleeps) = sleepless_client();
        client = client.rate_limit_per_min(2);
        client.throttle();
        client.throttle();
        assert!(sleeps.lock().unwrap().is_empty());
        client.throttle();
        assert_eq!(sleeps.lock().unwrap().len(), 1);
    }

    #[test]
    fn throttle_is_a_no_op_without_a_configured_limit() {
        let (mut client, sleeps) = sleepless_client();
        for _ in 0..10 {
            client.throttle();
        }
        assert!(sleeps.lock().unwrap().is_empty());
    }

    #[test]
    fn get_returns_ok_on_a_successful_response() {
        let mut server = mockito::Server::new();
        let mock = server.mock("GET", "/ok").with_status(200).create();
        let (mut client, _) = sleepless_client();
        let response = client.get(&format!("{}/ok", server.url())).unwrap();
        assert_eq!(response.status(), 200);
        mock.assert();
    }

    #[test]
    fn get_returns_status_error_immediately_on_a_non_retryable_4xx() {
        let mut server = mockito::Server::new();
        let mock = server.mock("GET", "/missing").with_status(404).create();
        let (mut client, sleeps) = sleepless_client();
        let err = client
            .get(&format!("{}/missing", server.url()))
            .unwrap_err();
        assert!(matches!(err, HttpError::Status { .. }));
        // No retry, so no backoff sleep happened.
        assert!(sleeps.lock().unwrap().is_empty());
        mock.assert();
    }

    #[test]
    fn a_declared_content_length_over_the_limit_is_rejected_before_reading_the_body() {
        let mut server = mockito::Server::new();
        // mockito sets Content-Length itself from the body it's given, so
        // a 100-byte body with a 10-byte limit exercises the real header
        // path, not a hand-crafted one.
        let mock = server
            .mock("GET", "/big")
            .with_status(200)
            .with_body(vec![b'x'; 100])
            .create();
        let (client, _) = sleepless_client();
        let mut client = client.max_response_bytes(10);
        let err = client.get(&format!("{}/big", server.url())).unwrap_err();
        assert!(
            matches!(
                err,
                HttpError::TooLarge {
                    limit: 10,
                    declared: 100
                }
            ),
            "{err:?}"
        );
        assert!(err.to_string().contains("100"), "{err}");
        mock.assert();
    }

    #[test]
    fn a_declared_content_length_within_the_limit_is_allowed_through() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/small")
            .with_status(200)
            .with_body(vec![b'x'; 10])
            .create();
        let (client, _) = sleepless_client();
        let mut client = client.max_response_bytes(100);
        let response = client.get(&format!("{}/small", server.url())).unwrap();
        assert_eq!(response.status(), 200);
        mock.assert();
    }

    #[test]
    fn get_retries_a_persistent_5xx_up_to_max_attempts_then_reports_transient() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/flaky")
            .with_status(503)
            .expect(3)
            .create();
        let (mut client, sleeps) = sleepless_client();
        client = client.max_attempts(3);
        let err = client.get(&format!("{}/flaky", server.url())).unwrap_err();
        match err {
            HttpError::Exhausted(ConnectorError::Transient(_)) => {}
            other => panic!("expected Exhausted(Transient), got {other:?}"),
        }
        // 3 attempts registered and consumed (mockito's .expect(3) would
        // itself fail the assert below if fewer/more calls landed);
        // 2 backoffs happened between the 3 attempts.
        mock.assert();
        assert_eq!(sleeps.lock().unwrap().len(), 2);
    }

    #[test]
    fn get_exhausts_retries_on_persistent_429_and_reports_rate_limited() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/limited")
            .with_status(429)
            .expect_at_least(2)
            .create();
        let (mut client, _) = sleepless_client();
        client = client.max_attempts(2);
        let err = client
            .get(&format!("{}/limited", server.url()))
            .unwrap_err();
        match err {
            HttpError::Exhausted(ConnectorError::RateLimited(_)) => {}
            other => panic!("expected Exhausted(RateLimited), got {other:?}"),
        }
        mock.assert();
    }
}
