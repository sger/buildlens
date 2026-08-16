//! Optional team server: receives already-parsed build metrics and stores them
//! in Postgres. Deliberately minimal — no queue, no object storage, no raw logs.
//! Clients parse locally and send a small JSON payload, so there is no slow
//! work to defer to a worker. That is why there is no Redis here and no
//! background worker: nothing arriving on this endpoint is slow enough to be
//! worth deferring, and a queue would only add a path where a client is told
//! "stored" before the row exists.
//!
//! Requests are served by a fixed pool of worker threads over a shared
//! connection pool, so an analytical query on one thread does not block ingest
//! on another. Concurrency is bounded at both ends on purpose — see
//! [`WORKER_THREADS`] and [`POOL_SIZE`] — because the backstop for overload
//! should be a queue in the accept loop, not an unbounded thread spawn.

mod store;

use anyhow::{Context, Result};
use buildlens_core::wire::{WIRE_VERSION, WireBuild};
use std::io::Read;
use std::sync::Arc;
use store::{Pool, PooledConnection, PostgresStore, ServerError};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

mod ratelimit;
use ratelimit::RateLimiter;

/// Refuse payloads larger than this; a metrics document is a few KB.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Worker threads serving requests, and connections in the pool.
///
/// Threads never exceed pool size: a thread that cannot get a connection is
/// blocked anyway, so extra threads would add context switching and no
/// throughput. Both are overridable for a machine with a different Postgres
/// `max_connections` — a NAS running other services wants a smaller pool.
const WORKER_THREADS: usize = 8;
const POOL_SIZE: u32 = 8;

/// Ingest attempts allowed per client per minute.
///
/// Generous for CI — a busy pipeline pushes one document per build — but low
/// enough that a misconfigured retry loop cannot fill the database.
const INGEST_PER_MINUTE: u32 = 120;

/// An outcome ready to send: HTTP status plus a JSON body.
struct Reply {
    status: u16,
    body: String,
}

impl Reply {
    fn ok(body: String) -> Self {
        Self { status: 200, body }
    }

    fn error(status: u16, message: &str) -> Self {
        Self {
            status,
            body: serde_json::json!({ "error": message }).to_string(),
        }
    }
}

/// Why a request could not be served.
///
/// The split exists so the client is told what it can act on and nothing more:
/// a bad payload is echoed back, but a database failure is logged locally and
/// reported as a bare 500. `ServerError::Database` wraps a `postgres::Error`
/// whose `Display` can carry table names, constraint names and connection
/// details — none of which a client should receive.
#[derive(Debug)]
enum Failure {
    /// The client's fault, and safe to describe.
    BadRequest(String),
    /// The payload exceeded [`MAX_BODY_BYTES`].
    TooLarge,
    /// The client exceeded [`INGEST_PER_MINUTE`].
    TooManyRequests,
    /// Ours. The detail goes to the log, not the response.
    Internal(anyhow::Error),
}

impl Failure {
    /// Splits into the reply the client sees and the detail only the log gets.
    fn into_reply(self) -> (Reply, Option<anyhow::Error>) {
        match self {
            Self::BadRequest(message) => (Reply::error(400, &message), None),
            Self::TooLarge => (
                Reply::error(413, "payload exceeds the 1 MiB limit"),
                None,
            ),
            // Rate limiting is a client-visible condition it can act on by
            // backing off, so unlike an internal failure it is described.
            Self::TooManyRequests => (
                Reply::error(429, "too many requests, slow down"),
                None,
            ),
            Self::Internal(error) => (
                Reply::error(500, "internal error"),
                Some(error),
            ),
        }
    }
}

/// Store failures are always internal: validation runs before the store, so
/// anything reaching here is a database problem the client cannot act on.
impl From<ServerError> for Failure {
    fn from(error: ServerError) -> Self {
        Self::Internal(error.into())
    }
}

/// Reads a positive integer from the environment, falling back to `default`.
///
/// A value that is present but unparsable is an error rather than a silent
/// fallback: someone who sets `BUILDLENS_THREADS=eight` has a mistaken belief
/// about their configuration, and starting anyway preserves it.
fn env_size(name: &str, default: usize) -> Result<usize> {
    match std::env::var(name) {
        Err(_) => Ok(default),
        Ok(raw) => {
            let value: usize = raw
                .trim()
                .parse()
                .with_context(|| format!("{name} must be a positive integer, got {raw:?}"))?;
            if value == 0 {
                anyhow::bail!("{name} must be greater than zero");
            }
            Ok(value)
        }
    }
}

fn main() -> Result<()> {
    let database_url = std::env::var("DATABASE_URL")
        .context("DATABASE_URL must be set (postgres://user:pass@host/db)")?;
    let bind = std::env::var("BUILDLENS_BIND").unwrap_or_else(|_| "127.0.0.1:8788".into());
    // Optional shared secret. Trimmed once here so the configured value and a
    // presented one are compared on equal terms — a token that picked up a
    // trailing newline from a .env file or CI variable would otherwise be
    // impossible to present correctly.
    let token = std::env::var("BUILDLENS_TOKEN")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    // Running without a token has to be chosen, not defaulted into.
    //
    // A loopback check would be the obvious rule, but it is the wrong one for
    // a container: the server must bind 0.0.0.0 for Docker's port mapping to
    // reach it, so every containerised run would trip it. An explicit opt-out
    // instead means a forgotten variable fails closed rather than silently
    // accepting writes from anyone who can route to the box.
    let allow_anonymous = std::env::var("BUILDLENS_ALLOW_ANONYMOUS").is_ok_and(|v| v.trim() == "1");
    if token.is_none() && !allow_anonymous {
        anyhow::bail!(
            "BUILDLENS_TOKEN is not set, so this server would accept writes from anyone who \
             can reach it. Set BUILDLENS_TOKEN, or set BUILDLENS_ALLOW_ANONYMOUS=1 to run \
             without authentication on a trusted network."
        );
    }
    if token.is_none() {
        eprintln!(
            "warning: running without authentication (BUILDLENS_ALLOW_ANONYMOUS=1). Anyone who \
             can reach {bind} can write builds."
        );
    }

    let pool_size = env_size("BUILDLENS_POOL_SIZE", POOL_SIZE as usize)?;
    let threads = env_size("BUILDLENS_THREADS", WORKER_THREADS)?;
    if threads > pool_size {
        // Not fatal, but it buys nothing: the surplus threads block waiting for
        // a connection that a busy pool cannot hand out.
        eprintln!(
            "warning: BUILDLENS_THREADS ({threads}) exceeds BUILDLENS_POOL_SIZE ({pool_size}); \
             the extra threads will block waiting for connections."
        );
    }

    // Migrations run once, here, on their own connection — not per pooled
    // connection, which would re-apply every ALTER for each one the pool opens.
    store::connect_and_migrate(&database_url).context("connecting to Postgres")?;
    let pool = store::pool(&database_url, pool_size as u32).context("building the connection pool")?;

    let server =
        Arc::new(Server::http(&bind).map_err(|error| anyhow::anyhow!(error.to_string()))?);
    let limiter = Arc::new(RateLimiter::new(INGEST_PER_MINUTE));
    eprintln!(
        "BuildLens server listening on {bind} ({threads} threads, {pool_size} connections)"
    );

    // Every worker runs the same accept loop; `recv` hands each request to
    // whichever thread asks first. Scoped threads so the pool and limiter can
    // be borrowed rather than cloned into each worker.
    std::thread::scope(|scope| {
        for _ in 0..threads {
            let server = Arc::clone(&server);
            let limiter = Arc::clone(&limiter);
            let pool = &pool;
            let token = token.as_deref();
            scope.spawn(move || {
                loop {
                    match server.recv() {
                        Ok(request) => {
                            if let Err(error) = handle(request, pool, token, &limiter) {
                                eprintln!("request failed: {error}");
                            }
                        }
                        // The server socket is closed or broken; this worker
                        // has nothing left to do. Other workers see the same
                        // and wind down too.
                        Err(error) => {
                            eprintln!("accept failed, worker stopping: {error}");
                            break;
                        }
                    }
                }
            });
        }
    });
    Ok(())
}

fn handle(
    mut request: Request,
    pool: &Pool,
    token: Option<&str>,
    limiter: &RateLimiter,
) -> Result<()> {
    let (path, query) = match request.url().split_once('?') {
        Some((path, query)) => (path.to_owned(), query.to_owned()),
        None => (request.url().to_owned(), String::new()),
    };

    // The health check is exempt, and only the health check. A container
    // healthcheck has no credential to present, so gating it behind the token
    // marks the server unhealthy and restart-loops a working deployment. It
    // reveals nothing: a fixed string, no database access, and reachability is
    // already observable to anyone who can open the port.
    let exempt_from_auth = path == "/health";
    let presented = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Authorization"))
        .map(|header| header.value.as_str().to_owned());
    if !exempt_from_auth && !is_authorized(token, presented.as_deref()) {
        return respond(request, Reply::error(401, "unauthorized"));
    }

    let method = request.method().clone();
    let client_key = client_key(&request);

    let reply = match route(&method, &path, &query, &mut request, pool, limiter, &client_key) {
        Ok(reply) => reply,
        Err(failure) => {
            let (reply, detail) = failure.into_reply();
            // Logged here rather than returned to the client, and never
            // dropped: a 500 with no local record is unactionable.
            if let Some(detail) = detail {
                eprintln!("{path}: {detail:#}");
            }
            reply
        }
    };
    respond(request, reply)
}

/// Dispatches one request. Every arm returns a [`Reply`], so no path can end
/// with the connection closed and nothing sent — a database error on a read
/// endpoint previously did exactly that.
fn route(
    method: &Method,
    path: &str,
    query: &str,
    request: &mut Request,
    pool: &Pool,
    limiter: &RateLimiter,
    client_key: &str,
) -> Result<Reply, Failure> {
    match (method, path) {
        // Deliberately before any database work: a health check that fails
        // when Postgres is briefly unreachable would make the container
        // restart loop, taking down an endpoint that was about to recover.
        (Method::Get, "/health") => Ok(Reply::ok(
            serde_json::json!({ "status": "ok" }).to_string(),
        )),
        (Method::Post, "/v1/metrics") => {
            // Checked before reading the body, so a client in a retry loop is
            // refused without the server buffering a megabyte per attempt.
            if !limiter.allow(client_key) {
                return Err(Failure::TooManyRequests);
            }
            let payload = read_body(request)?;
            ingest(&payload, pool).map(Reply::ok)
        }
        (Method::Get, "/v1/builds") => {
            let limit = param(query, "limit").unwrap_or(100).clamp(1, 1000);
            let mut connection = connection(pool)?;
            let snapshot = PostgresStore::new(&mut connection).builds(limit)?;
            Ok(Reply::ok(snapshot.to_string()))
        }
        (Method::Get, "/v1/builds/detail") => {
            let Some(key) = text_param(query, "build_key") else {
                return Err(Failure::BadRequest("build_key is required".to_owned()));
            };
            let mut connection = connection(pool)?;
            match PostgresStore::new(&mut connection).build_detail(&key)? {
                Some(detail) => Ok(Reply::ok(detail.to_string())),
                None => Err(Failure::BadRequest("no such build".to_owned())),
            }
        }
        (Method::Get, "/v1/stats") => {
            let days = param(query, "days").unwrap_or(30).clamp(1, 365);
            let mut connection = connection(pool)?;
            let snapshot = PostgresStore::new(&mut connection).stats(days)?;
            Ok(Reply::ok(snapshot.to_string()))
        }
        (Method::Get, "/v1/stats/daily") => {
            let limit = param(query, "limit").unwrap_or(30).clamp(1, 365);
            let mut connection = connection(pool)?;
            let snapshot = PostgresStore::new(&mut connection)
                .daily(limit, text_param(query, "project").as_deref())?;
            Ok(Reply::ok(snapshot.to_string()))
        }
        (Method::Get, "/v1/stats/percentiles") => {
            let limit = param(query, "limit").unwrap_or(100).clamp(1, 1000);
            let mut connection = connection(pool)?;
            let snapshot = PostgresStore::new(&mut connection)
                .percentiles(limit, text_param(query, "project").as_deref())?;
            Ok(Reply::ok(snapshot.to_string()))
        }
        (Method::Get, "/v1/targets/slowest") => {
            let days = param(query, "days").unwrap_or(30).clamp(1, 365);
            let limit = param(query, "limit").unwrap_or(20).clamp(1, 200);
            let mut connection = connection(pool)?;
            let snapshot = PostgresStore::new(&mut connection).slowest_targets(days, limit)?;
            Ok(Reply::ok(snapshot.to_string()))
        }
        (Method::Get, "/v1/targets/ranked") => {
            let builds = param(query, "builds").unwrap_or(100).clamp(1, 1000);
            let top = param(query, "top").unwrap_or(20).clamp(1, 200);
            let mut connection = connection(pool)?;
            let snapshot = PostgresStore::new(&mut connection)
                .ranked_targets(builds, top, text_param(query, "project").as_deref())?;
            Ok(Reply::ok(snapshot.to_string()))
        }
        (Method::Get, "/v1/phases/ranked") => {
            let builds = param(query, "builds").unwrap_or(100).clamp(1, 1000);
            let top = param(query, "top").unwrap_or(20).clamp(1, 200);
            let mut connection = connection(pool)?;
            let snapshot = PostgresStore::new(&mut connection)
                .ranked_phases(builds, top, text_param(query, "project").as_deref())?;
            Ok(Reply::ok(snapshot.to_string()))
        }
        _ => Ok(Reply::error(404, "not found")),
    }
}

/// Borrows a connection for one query.
///
/// Returns the guard rather than a `PostgresStore` because the store borrows
/// the connection: the caller holds this alive for the length of its query and
/// the connection returns to the pool at the end of the statement.
fn connection(pool: &Pool) -> Result<PooledConnection, Failure> {
    Ok(pool.get().map_err(ServerError::from)?)
}

/// Identifies a client for rate limiting.
///
/// Uses the source address, which is what is available without trusting a
/// header. Behind a reverse proxy every request appears to come from the proxy
/// and the limit becomes global — correct to know before putting one in front.
/// `X-Forwarded-For` is deliberately *not* consulted: unvalidated, it lets any
/// client mint an identity per request and bypass the limit entirely.
fn client_key(request: &Request) -> String {
    request
        .remote_addr()
        .map(|address| address.ip().to_string())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Reads the request body, rejecting anything past the limit.
///
/// `take` alone would silently truncate at the limit and hand a half payload
/// to the parser, which then fails with a confusing syntax error instead of
/// saying the body was too large. Reading one byte past the cap distinguishes
/// "exactly at the limit" from "over it".
fn read_body(request: &mut Request) -> Result<String, Failure> {
    read_capped(request.as_reader(), MAX_BODY_BYTES)
}

/// Reads at most `limit` bytes, failing if the source has more.
///
/// Split from [`read_body`] over a plain `Read` so the boundary is testable: a
/// `Request` cannot be constructed outside a live server.
fn read_capped(source: &mut (impl Read + ?Sized), limit: usize) -> Result<String, Failure> {
    let mut payload = Vec::new();
    // One byte past the cap, so "exactly at the limit" and "over it" are
    // distinguishable. Reading exactly `limit` would leave both cases looking
    // like a full buffer.
    source
        .take(limit as u64 + 1)
        .read_to_end(&mut payload)
        .map_err(|error| Failure::Internal(error.into()))?;
    if payload.len() > limit {
        return Err(Failure::TooLarge);
    }
    String::from_utf8(payload)
        .map_err(|_| Failure::BadRequest("payload is not valid UTF-8".to_owned()))
}

fn respond(request: Request, reply: Reply) -> Result<()> {
    let response = Response::from_string(reply.body)
        .with_status_code(StatusCode(reply.status))
        .with_header(
            Header::from_bytes(b"Content-Type", b"application/json").expect("valid header"),
        )
        // Without this a browser caches the JSON and keeps showing yesterday's
        // numbers after a fresh build has been ingested.
        .with_header(
            Header::from_bytes(b"Cache-Control", b"no-store, must-revalidate")
                .expect("valid header"),
        );
    request.respond(response).map_err(Into::into)
}

/// Parses and validates a payload without touching the database.
///
/// Separate from [`ingest`] so every rejection rule is testable without a
/// Postgres connection — and so it is evident that all of them run before any
/// database work is attempted.
fn parse_payload(payload: &str) -> Result<WireBuild, Failure> {
    let build: WireBuild = serde_json::from_str(payload).map_err(|error| {
        Failure::BadRequest(format!(
            "payload is not a BuildLens metrics document: {error}"
        ))
    })?;
    if build.wire_version != WIRE_VERSION {
        return Err(Failure::BadRequest(format!(
            "unsupported wire version {} (this server speaks {WIRE_VERSION})",
            build.wire_version
        )));
    }
    if build.build_key.is_empty() {
        return Err(Failure::BadRequest("build_key must not be empty".to_owned()));
    }
    Ok(build)
}

fn ingest(payload: &str, pool: &Pool) -> Result<String, Failure> {
    let build = parse_payload(payload)?;
    let mut connection = connection(pool)?;
    let stored = PostgresStore::new(&mut connection).insert(&build)?;
    Ok(serde_json::json!({
        "stored": stored,
        "build_key": build.build_key,
        "duplicate": !stored,
    })
    .to_string())
}

fn param(query: &str, name: &str) -> Option<i64> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == name)
        .and_then(|(_, value)| value.parse().ok())
}

/// The string counterpart of [`param`]. Query values are percent-encoded, so a
/// project with a space would otherwise never match. An empty value means
/// "All projects" and reads as absent.
fn text_param(query: &str, name: &str) -> Option<String> {
    let raw = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| percent_decode(value))?;
    (!raw.is_empty()).then_some(raw)
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            // `i + 3 <= len` is the slice bound: an escape needs two hex
            // digits after the '%'.
            b'%' if i + 3 <= bytes.len() => {
                match u8::from_str_radix(&value[i + 1..i + 3], 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Accepts "Bearer <token>" or a bare token. With no token configured every
/// request is allowed, which the server warns about at startup.
///
/// The comparison is constant-time. `==` on `str` stops at the first differing
/// byte, so response timing leaks how much of a guess was correct and an
/// attacker recovers the token byte by byte. This is the server's only
/// authentication control, so the comparison must not depend on the data.
fn is_authorized(expected: Option<&str>, presented: Option<&str>) -> bool {
    let Some(expected) = expected else {
        return true;
    };
    presented.is_some_and(|value| {
        let offered = value.strip_prefix("Bearer ").unwrap_or(value).trim();
        constant_time_eq(offered.as_bytes(), expected.as_bytes())
    })
}

/// Compares two byte strings without short-circuiting.
///
/// Lengths are compared up front and the loop always runs over the longer of
/// the two, so the time taken reveals only the presented length — which the
/// attacker already knows, having chosen it.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut difference = (a.len() ^ b.len()) as u8;
    for index in 0..a.len().max(b.len()) {
        // Out-of-range indices fold in a fixed value rather than breaking, so
        // a length mismatch costs the same as a content mismatch.
        let left = a.get(index).copied().unwrap_or(0);
        let right = b.get(index).copied().unwrap_or(0);
        difference |= left ^ right;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- authorization ---

    #[test]
    fn accepts_the_configured_token_in_either_form() {
        assert!(is_authorized(Some("secret"), Some("Bearer secret")));
        assert!(is_authorized(Some("secret"), Some("secret")));
    }

    /// Whitespace around the presented value is tolerated, because headers
    /// pick it up in transit.
    #[test]
    fn surrounding_whitespace_in_the_header_is_ignored() {
        assert!(is_authorized(Some("secret"), Some("Bearer secret  ")));
        assert!(is_authorized(Some("secret"), Some("  secret")));
    }

    #[test]
    fn rejects_missing_and_wrong_tokens() {
        assert!(!is_authorized(Some("secret"), None));
        assert!(!is_authorized(Some("secret"), Some("Bearer wrong")));
        assert!(!is_authorized(Some("secret"), Some("")));
        // A prefix must not pass for the whole token.
        assert!(!is_authorized(Some("secret"), Some("Bearer sec")));
        // Nor an extension of it.
        assert!(!is_authorized(Some("secret"), Some("secretsecret")));
    }

    #[test]
    fn allows_everything_when_no_token_is_configured() {
        assert!(is_authorized(None, None));
        assert!(is_authorized(None, Some("anything")));
    }

    /// `main` trims the configured token so a value carrying a stray newline
    /// from a .env file is still presentable. Without that, the header side
    /// was trimmed and the config side was not, making the token unusable.
    #[test]
    fn a_configured_token_is_compared_after_trimming() {
        let configured = " secret\n".trim();
        assert!(is_authorized(Some(configured), Some("Bearer secret")));
    }

    // --- constant-time comparison ---

    #[test]
    fn constant_time_eq_matches_equality() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(constant_time_eq(b"", b""));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"sec"));
        assert!(!constant_time_eq(b"sec", b"secret"));
        assert!(!constant_time_eq(b"", b"a"));
    }

    /// A zero byte inside one side must not collide with the padding used for
    /// out-of-range indices on the other.
    #[test]
    fn a_nul_byte_does_not_alias_the_padding() {
        assert!(!constant_time_eq(b"a\0", b"a"));
        assert!(!constant_time_eq(b"a", b"a\0"));
    }

    /// The comparison must examine every byte regardless of where the first
    /// difference is. A short-circuiting `==` would stop at index 0 for the
    /// first case and index 5 for the second, and the difference in work is
    /// what an attacker times.
    ///
    /// Counted rather than timed: wall-clock in a test is far too noisy to
    /// assert on, but the number of byte comparisons is exactly the quantity
    /// that must not depend on the input.
    #[test]
    fn the_comparison_examines_every_byte_wherever_the_difference_is() {
        fn comparisons(a: &[u8], b: &[u8]) -> usize {
            let mut count = 0;
            let mut difference = (a.len() ^ b.len()) as u8;
            for index in 0..a.len().max(b.len()) {
                count += 1;
                difference |= a.get(index).copied().unwrap_or(0)
                    ^ b.get(index).copied().unwrap_or(0);
            }
            let _ = difference;
            count
        }
        // Differs at the first byte versus the last: identical work.
        assert_eq!(comparisons(b"Xecret", b"secret"), 6);
        assert_eq!(comparisons(b"secreX", b"secret"), 6);
        assert_eq!(comparisons(b"secret", b"secret"), 6);
        // And all three agree with the real implementation's verdict.
        assert!(!constant_time_eq(b"Xecret", b"secret"));
        assert!(!constant_time_eq(b"secreX", b"secret"));
        assert!(constant_time_eq(b"secret", b"secret"));
    }

    /// Every near-miss must be rejected.
    ///
    /// Note what this does *not* prove: `constant_time_eq` and `==` agree on
    /// every input, differing only in how long they take. No unit test can
    /// tell them apart, so swapping the call here for `==` would keep this
    /// green. The constant-time property is enforced by review of
    /// [`is_authorized`], not by this assertion.
    #[test]
    fn no_near_miss_authenticates() {
        assert!(!is_authorized(Some("secret"), Some("secret\0")));
        assert!(!is_authorized(Some("secret\0"), Some("secret")));
        // The lengths are folded in, so no prefix or extension can pass.
        for candidate in ["s", "se", "sec", "secr", "secre", "secretx"] {
            assert!(
                !is_authorized(Some("secret"), Some(candidate)),
                "{candidate:?} must not authenticate"
            );
        }
        // Surrounding whitespace is stripped before comparing, so this is the
        // token itself rather than an extension of it.
        assert!(is_authorized(Some("secret"), Some("secret ")));
    }

    // --- the health-check exemption ---

    /// `/health` must answer without a token, or a container healthcheck —
    /// which has no credential to present — marks a working server unhealthy
    /// and restart-loops it.
    ///
    /// Mirrors the predicate in [`handle`] rather than calling it, because
    /// `handle` needs a live `Request`. The duplication is the point: if the
    /// exemption there is ever widened, this test still describes what the
    /// exemption is supposed to be, and the two disagreeing is the signal.
    #[test]
    fn only_the_health_path_skips_authentication() {
        let exempt = |path: &str| path == "/health";
        assert!(exempt("/health"));
        // Every data path stays gated, including the ones that only read.
        for path in [
            "/v1/metrics",
            "/v1/builds",
            "/v1/builds/detail",
            "/v1/stats",
            "/v1/stats/daily",
            "/v1/stats/percentiles",
            "/v1/targets/slowest",
            "/v1/targets/ranked",
            "/v1/phases/ranked",
            // Near-misses must not slip through a prefix or suffix match.
            "/health/../v1/builds",
            "/healthz",
            "/v1/health",
            "",
            "/",
        ] {
            assert!(!exempt(path), "{path} must require a token");
        }
    }

    // --- numeric query parameters ---

    #[test]
    fn reads_a_numeric_parameter() {
        assert_eq!(param("limit=50", "limit"), Some(50));
        assert_eq!(param("days=7&limit=50", "limit"), Some(50));
        assert_eq!(param("days=7&limit=50", "days"), Some(7));
    }

    #[test]
    fn a_missing_or_unparsable_parameter_is_absent() {
        assert_eq!(param("", "limit"), None);
        assert_eq!(param("days=7", "limit"), None);
        assert_eq!(param("limit=abc", "limit"), None);
        assert_eq!(param("limit=", "limit"), None);
    }

    /// A key must match whole, or `limit` would be satisfied by `xlimit`.
    #[test]
    fn parameter_keys_match_exactly() {
        assert_eq!(param("xlimit=5", "limit"), None);
        assert_eq!(param("limits=5", "limit"), None);
    }

    // --- string query parameters ---

    #[test]
    fn reads_and_decodes_a_string_parameter() {
        assert_eq!(text_param("project=App", "project").as_deref(), Some("App"));
        assert_eq!(
            text_param("project=My%20App", "project").as_deref(),
            Some("My App")
        );
        assert_eq!(
            text_param("project=My+App", "project").as_deref(),
            Some("My App")
        );
    }

    /// An empty value is how the dashboard spells "All projects", and must
    /// read as absent rather than as a project literally named "".
    #[test]
    fn an_empty_string_parameter_is_absent() {
        assert_eq!(text_param("project=", "project"), None);
        assert_eq!(text_param("", "project"), None);
    }

    #[test]
    fn percent_decoding_handles_edge_cases() {
        assert_eq!(percent_decode("%41"), "A");
        assert_eq!(percent_decode("a%2Fb"), "a/b");
        // Multi-byte UTF-8 survives a byte-at-a-time decode.
        assert_eq!(percent_decode("%C3%A9"), "é");
        // A truncated escape is left alone rather than panicking on the slice.
        assert_eq!(percent_decode("%4"), "%4");
        assert_eq!(percent_decode("%"), "%");
        assert_eq!(percent_decode("abc%"), "abc%");
        // Non-hex after '%' is not an escape.
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    /// Bytes that are not valid UTF-8 must not panic; they become the
    /// replacement character.
    #[test]
    fn invalid_utf8_decodes_lossily() {
        assert_eq!(percent_decode("%FF"), "\u{fffd}");
    }

    // --- body limits ---

    /// The cap must reject rather than truncate: a body cut at the limit and
    /// handed to the parser fails with a syntax error that blames the client
    /// for something the server did.
    #[test]
    fn an_oversized_body_is_rejected_not_truncated() {
        let mut source = std::io::Cursor::new(vec![b'x'; 11]);
        let failure = read_capped(&mut source, 10).expect_err("11 bytes over a 10-byte cap");
        assert!(matches!(failure, Failure::TooLarge));
        assert_eq!(failure.into_reply().0.status, 413);
    }

    /// A body of exactly the limit is valid and must not be mistaken for one
    /// that overflowed.
    #[test]
    fn a_body_at_exactly_the_limit_is_accepted() {
        let mut source = std::io::Cursor::new(vec![b'x'; 10]);
        assert_eq!(read_capped(&mut source, 10).unwrap().len(), 10);
    }

    #[test]
    fn an_empty_body_reads_as_empty() {
        let mut source = std::io::Cursor::new(Vec::new());
        assert_eq!(read_capped(&mut source, 10).unwrap(), "");
    }

    /// Bytes that are not UTF-8 are the client's fault and must not be
    /// reported as an internal error.
    #[test]
    fn a_non_utf8_body_is_a_bad_request() {
        let mut source = std::io::Cursor::new(vec![0xff, 0xfe]);
        let failure = read_capped(&mut source, 10).expect_err("invalid UTF-8");
        assert!(matches!(failure, Failure::BadRequest(_)));
        assert_eq!(failure.into_reply().0.status, 400);
    }

    #[test]
    fn the_configured_limit_is_a_mebibyte() {
        assert_eq!(MAX_BODY_BYTES, 1024 * 1024);
    }

    // --- failure mapping ---

    /// A database error must not reach the client: its `Display` can carry
    /// table names, constraint names and connection details.
    #[test]
    fn internal_errors_are_not_described_to_the_client() {
        let failure = Failure::Internal(anyhow::anyhow!(
            "database error: relation \"builds_secret\" does not exist"
        ));
        let (reply, detail) = failure.into_reply();
        assert_eq!(reply.status, 500);
        assert!(!reply.body.contains("builds_secret"), "{}", reply.body);
        assert!(reply.body.contains("internal error"));
        assert!(detail.is_some(), "the detail must still reach the log");
    }

    /// A client-caused rejection names what was wrong, so it is safe to echo.
    #[test]
    fn a_rejected_payload_is_described_to_the_client() {
        let failure = Failure::BadRequest("build_key must not be empty".to_owned());
        let (reply, detail) = failure.into_reply();
        assert_eq!(reply.status, 400);
        assert!(reply.body.contains("build_key"), "{}", reply.body);
        assert!(detail.is_none(), "a client fault needs no server-side log");
    }

    /// Anything from the store is a database problem the client cannot act
    /// on, so it must land on the internal path — and its text, which can name
    /// tables and constraints, must not reach the response.
    #[test]
    fn store_errors_map_to_internal_failures() {
        let store_error = store::connect_error_for_tests();
        let leaked = store_error.to_string();
        let failure = Failure::from(store_error);
        assert!(matches!(failure, Failure::Internal(_)));
        let (reply, detail) = failure.into_reply();
        assert_eq!(reply.status, 500);
        assert_eq!(reply.body, r#"{"error":"internal error"}"#);
        assert!(
            !reply.body.contains(leaked.trim()),
            "the database message reached the client: {}",
            reply.body
        );
        assert!(detail.is_some(), "the detail must still reach the log");
    }

    /// Pool exhaustion is our problem, not the client's, and its message names
    /// the host and port it failed to reach — which must not be echoed.
    #[test]
    fn pool_errors_map_to_internal_failures() {
        let failure = Failure::from(store::pool_error_for_tests());
        assert!(matches!(failure, Failure::Internal(_)));
        let (reply, detail) = failure.into_reply();
        assert_eq!(reply.status, 500);
        assert_eq!(reply.body, r#"{"error":"internal error"}"#);
        assert!(detail.is_some(), "the detail must still reach the log");
    }

    /// A throttled client is told to back off — unlike an internal failure,
    /// this is a condition it can act on.
    #[test]
    fn rate_limiting_is_described_to_the_client() {
        let (reply, detail) = Failure::TooManyRequests.into_reply();
        assert_eq!(reply.status, 429);
        assert!(reply.body.contains("too many requests"), "{}", reply.body);
        assert!(detail.is_none(), "a throttled client needs no server-side log");
    }

    // --- configuration ---

    /// A malformed size must stop startup rather than silently fall back: the
    /// operator who set it believes it took effect.
    #[test]
    fn an_unparsable_size_is_rejected() {
        // SAFETY: single-threaded test, and the variable is removed before it
        // returns so no other test observes it.
        unsafe { std::env::set_var("BUILDLENS_TEST_SIZE_BAD", "eight") };
        let error = env_size("BUILDLENS_TEST_SIZE_BAD", 4).expect_err("not a number");
        unsafe { std::env::remove_var("BUILDLENS_TEST_SIZE_BAD") };
        assert!(error.to_string().contains("positive integer"), "{error}");
    }

    #[test]
    fn zero_is_not_a_usable_size() {
        unsafe { std::env::set_var("BUILDLENS_TEST_SIZE_ZERO", "0") };
        let error = env_size("BUILDLENS_TEST_SIZE_ZERO", 4).expect_err("zero is unusable");
        unsafe { std::env::remove_var("BUILDLENS_TEST_SIZE_ZERO") };
        assert!(error.to_string().contains("greater than zero"), "{error}");
    }

    #[test]
    fn an_absent_size_falls_back_to_the_default() {
        assert_eq!(env_size("BUILDLENS_TEST_SIZE_ABSENT", 7).unwrap(), 7);
    }

    #[test]
    fn a_configured_size_overrides_the_default() {
        unsafe { std::env::set_var("BUILDLENS_TEST_SIZE_OK", " 16 ") };
        let value = env_size("BUILDLENS_TEST_SIZE_OK", 4).unwrap();
        unsafe { std::env::remove_var("BUILDLENS_TEST_SIZE_OK") };
        assert_eq!(value, 16, "surrounding whitespace must be tolerated");
    }

    #[test]
    fn replies_carry_the_status_they_were_built_with() {
        assert_eq!(Reply::ok("{}".to_owned()).status, 200);
        assert_eq!(Reply::error(404, "not found").status, 404);
        assert!(Reply::error(404, "not found").body.contains("not found"));
    }

    // --- ingest validation ---

    /// Every validation rule runs before the store is reached, so these need
    /// no database.
    fn reject(payload: &str) -> String {
        match parse_payload(payload) {
            Err(Failure::BadRequest(message)) => message,
            Err(_) => panic!("expected a bad-request failure"),
            Ok(_) => panic!("expected {payload} to be rejected"),
        }
    }

    fn valid_payload() -> String {
        serde_json::json!({
            "wire_version": WIRE_VERSION,
            "build_key": "abc123",
            "project": "App",
            "category": "clean",
            "total_seconds": 12.5,
            "compiled_count": 10,
            "cache_hit_rate": null,
            "error_count": 0,
            "warning_count": 0,
            "status": "succeeded",
            "started_at": 1_700_000_000.0,
            "attribution": "anonymous",
            "machine_id": null,
            "xcode_version": "16.2",
            "platform": "iOS",
            "architecture": "arm64",
            "targets": [],
            "phases": [],
        })
        .to_string()
    }

    #[test]
    fn a_well_formed_payload_is_accepted() {
        let build = parse_payload(&valid_payload()).expect("valid payload");
        assert_eq!(build.build_key, "abc123");
        assert_eq!(build.wire_version, WIRE_VERSION);
    }

    #[test]
    fn malformed_json_is_rejected() {
        assert!(reject("{not json").contains("not a BuildLens metrics document"));
    }

    #[test]
    fn an_empty_body_is_rejected() {
        assert!(!reject("").is_empty());
    }

    /// A future client must be told the server is behind, not have its
    /// payload silently stored under today's interpretation.
    #[test]
    fn a_mismatched_wire_version_is_rejected() {
        let payload = valid_payload().replace(
            &format!("\"wire_version\":{WIRE_VERSION}"),
            "\"wire_version\":9999",
        );
        let message = reject(&payload);
        assert!(message.contains("unsupported wire version 9999"), "{message}");
    }

    /// `build_key` is the idempotency key; an empty one would make every
    /// retry collide with every other build sent without a key.
    #[test]
    fn an_empty_build_key_is_rejected() {
        let payload = valid_payload().replace("\"abc123\"", "\"\"");
        assert!(reject(&payload).contains("build_key must not be empty"));
    }

    /// Validation must reject before any database work — asserted by the fact
    /// that `parse_payload` has no store parameter at all, and exercised here
    /// on the payloads that would otherwise reach it.
    #[test]
    fn validation_needs_no_database() {
        for payload in ["", "{}", "{not json"] {
            assert!(parse_payload(payload).is_err());
        }
    }
}
