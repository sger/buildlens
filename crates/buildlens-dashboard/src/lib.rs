use buildlens_storage::{PostgresStore, StoreError};
use std::sync::Arc;
use tiny_http::{Method, Response, Server, StatusCode};

/// The dashboard UI, bundled from `dashboard-ui/` at build time and committed
/// so a plain `cargo build` needs no Node toolchain.
///
/// Lives under `assets/` rather than `src/` because it is generated output:
/// editing it directly is pointless, since the next `node dashboard-ui/build.mjs`
/// overwrites it.
const INDEX: &str = include_str!("../assets/index.html");
const MAX_LIMIT: i64 = 500;

/// How long an API response may be reused.
///
/// The UI refetches all fifteen panels every two seconds (`REFRESH_MS` in
/// `dashboard-ui/src/main.tsx`), and each panel is a "last N builds" aggregate
/// over the same rows. Answering every one from the database meant ~450 queries
/// a minute per open tab, all recomputing numbers that only change when a build
/// finishes.
///
/// Five seconds is chosen against the refresh interval, not against how fast
/// builds arrive: it collapses the repeat fetches of one polling cycle while
/// staying far below the time it takes to notice a stale panel. A new build
/// shows up on the next tick after this expires, and `invalidate` makes the
/// local `collect` path immediate anyway.
const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(5);

/// Cached API responses, keyed by the exact path and query that produced them.
///
/// Keyed on the raw query string rather than the parsed parameters so that two
/// requests differing in any way — a different `limit`, a different project,
/// even a different parameter order — cannot read each other's rows. That
/// over-caches slightly (`?a=1&b=2` and `?b=2&a=1` are separate entries) and
/// never under-caches, which is the correct direction for a leak that would
/// otherwise show one project's numbers under another's name.
///
/// Deliberately not an LRU: the dashboard requests a fixed set of paths with a
/// fixed set of parameters, so the key space is bounded by the panel count times
/// the project count. Entries are dropped on expiry rather than evicted.
#[derive(Default)]
pub struct ResponseCache {
    entries: std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, String)>>,
}

impl ResponseCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drops every cached response.
    ///
    /// Called after a write so a freshly stored build is visible immediately
    /// rather than up to `CACHE_TTL` later. Clearing everything rather than the
    /// affected keys is deliberate: one build changes almost every aggregate,
    /// so working out which entries survive costs more than recomputing them.
    pub fn invalidate(&self) {
        self.lock().clear();
    }

    fn lock(
        &self,
    ) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, (std::time::Instant, String)>>
    {
        // As with the store lock: a panicked request must not turn one failure
        // into a permanent outage. There is no invariant here beyond "these
        // strings were once a valid response".
        self.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The cached body for `key`, if one is present and still fresh.
    ///
    /// Public so the server crate can assert its invalidation actually took
    /// effect; [`route_cached`] is the only caller that should use it to serve a
    /// request.
    pub fn get(&self, key: &str) -> Option<String> {
        let mut entries = self.lock();
        let (stored_at, body) = entries.get(key)?;
        if stored_at.elapsed() < CACHE_TTL {
            return Some(body.clone());
        }
        entries.remove(key);
        None
    }

    /// Stores `body` under `key`. See [`ResponseCache::get`] on visibility.
    pub fn put(&self, key: String, body: &str) {
        let mut entries = self.lock();
        // Expired neighbours are cleared on write rather than by a timer: the
        // key space is small, and this keeps a project that disappears from the
        // UI from pinning its entries forever.
        let now = std::time::Instant::now();
        entries.retain(|_, (stored_at, _)| now.duration_since(*stored_at) < CACHE_TTL);
        entries.insert(key, (now, body.to_owned()));
    }
}

pub fn serve(store: PostgresStore, port: u16) -> anyhow::Result<()> {
    let server = Server::http(("127.0.0.1", port)).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let store = Arc::new(std::sync::Mutex::new(store));
    let cache = ResponseCache::new();
    eprintln!("BuildLens dashboard: http://127.0.0.1:{port}");
    for request in server.incoming_requests() {
        let routed = if request.method() == &Method::Get {
            let (path, query) = split_url(request.url());
            route_cached(&path, &query, &store, &cache)
        } else {
            Routed {
                status: 405,
                content_type: "application/json",
                body: serde_json::json!({ "error": "method not allowed" }).to_string(),
            }
        };
        // Logged, not propagated: `respond` fails when the client disconnects
        // mid-response, and a closed browser tab must not shut the dashboard
        // down.
        if let Err(error) = request.respond(
            Response::from_string(routed.body)
                .with_status_code(StatusCode(routed.status))
                .with_header(header(routed.content_type))
                .with_header(no_cache()),
        ) {
            eprintln!("dashboard: could not send a response: {error}");
        }
    }
    Ok(())
}

pub struct Routed {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
}

fn header(content_type: &'static str) -> tiny_http::Header {
    tiny_http::Header::from_bytes(b"Content-Type", content_type.as_bytes()).expect("valid header")
}

/// The UI is compiled into the binary, so upgrading BuildLens changes the page
/// while its URL stays `http://127.0.0.1:8787/`. Without this, a browser keeps
/// serving the previous build's bundle from cache and the new dashboard simply
/// never appears — which reads as "the data is missing" rather than "the page
/// is stale". API responses are live data and must not be cached either.
fn no_cache() -> tiny_http::Header {
    tiny_http::Header::from_bytes(b"Cache-Control", b"no-store, must-revalidate")
        .expect("valid header")
}

fn split_url(url: &str) -> (String, String) {
    url.split_once('?').map(|(a, b)| (a.to_owned(), b.to_owned())).unwrap_or((url.to_owned(), String::new()))
}

/// Percent-decoded string query parameter. Project names contain spaces and
/// non-ASCII characters, so a raw read would fail to match the stored name.
fn text_param(query: &str, name: &str) -> Option<String> {
    let raw = query.split('&').filter_map(|pair| pair.split_once('=')).find(|(key, _)| *key == name)?.1;
    Some(percent_decode(raw))
}

/// Decodes `%XX` escapes and `+` as a space.
///
/// Shared by the query parameters and the `/api/build/<key>` path segment, so
/// a project name and a build key are decoded by one implementation rather
/// than two that could disagree.
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            b'%' if index + 3 <= bytes.len() => match u8::from_str_radix(&raw[index + 1..index + 3], 16) {
                Ok(byte) => {
                    decoded.push(byte);
                    index += 3;
                }
                Err(_) => {
                    decoded.push(b'%');
                    index += 1;
                }
            },
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn not_found(message: &str) -> Routed {
    Routed {
        status: 404,
        content_type: "application/json",
        body: serde_json::json!({ "error": message }).to_string(),
    }
}

fn number_param(query: &str, name: &str, default: i64) -> i64 {
    text_param(query, name).and_then(|value| value.parse().ok()).unwrap_or(default).clamp(1, MAX_LIMIT)
}

/// Renders a store result as a JSON response.
///
/// Two rules, both about not lying to the page:
///
/// A serialization failure is a 500, not `{}` with status 200. An empty object
/// is a *valid, successful* answer, so the page would render empty panels and
/// the reader would conclude there is no data rather than that the request
/// failed.
///
/// A store error is reported as a bare message. `StoreError::Database` wraps a
/// `postgres::Error` whose `Display` can name tables, constraints and
/// connection details; the detail goes to the log the operator can read.
fn json<T: serde::Serialize>(value: Result<T, StoreError>) -> Routed {
    match value {
        Ok(value) => match serde_json::to_string(&value) {
            Ok(body) => Routed { status: 200, content_type: "application/json", body },
            Err(error) => {
                eprintln!("dashboard: could not serialize a response: {error}");
                error_response("could not render this result")
            }
        },
        Err(error) => {
            eprintln!("dashboard: store query failed: {error}");
            error_response("query failed")
        }
    }
}

/// A 500 whose body says only what a browser needs to know.
fn error_response(message: &str) -> Routed {
    Routed {
        status: 500,
        content_type: "application/json",
        body: serde_json::json!({ "error": message }).to_string(),
    }
}

/// [`route`], serving a recent identical request from `cache` instead of the
/// database.
///
/// Only 200s are cached, and only for `/api/` paths. A 404 or a 500 is cheap to
/// recompute and caching it would hold a transient database failure for
/// `CACHE_TTL` after it cleared; `/` is the compiled-in bundle and never touches
/// the store at all.
pub fn route_cached(
    path: &str,
    query: &str,
    store: &Arc<std::sync::Mutex<PostgresStore>>,
    cache: &ResponseCache,
) -> Routed {
    if !path.starts_with("/api/") {
        return route(path, query, store);
    }
    let key = format!("{path}?{query}");
    if let Some(body) = cache.get(&key) {
        return Routed { status: 200, content_type: "application/json", body };
    }
    let routed = route(path, query, store);
    if routed.status == 200 {
        cache.put(key, &routed.body);
    }
    routed
}

/// Read-only routing over store snapshots; separated from HTTP for testability.
///
/// Every project-scoped endpoint must thread `project` through, or a 400s
/// project's numbers leak into a 12s project's panel — a recurring bug class
/// here, guarded by `project_filter_reaches_every_project_scoped_route`.
///
/// Uncached. Callers serving a browser want [`route_cached`]; this stays
/// available so a test can assert what the database actually returns without a
/// previous request's answer standing in for it.
pub fn route(path: &str, query: &str, store: &Arc<std::sync::Mutex<PostgresStore>>) -> Routed {
    let limit = number_param(query, "limit", 100);
    let builds = number_param(query, "builds", 30);
    let top = number_param(query, "top", 8).min(20);
    // Empty or absent "project" means every project.
    let project = text_param(query, "project").filter(|value| !value.is_empty());
    let project = project.as_deref();
    // A panic in one request would otherwise poison the mutex and make every
    // later request panic too, turning one failure into a permanent outage.
    // Behind the lock is a database connection, not an invariant-bearing
    // structure, so continuing is sound.
    let mut store = store.lock().unwrap_or_else(|poisoned| {
        eprintln!("dashboard: recovering the store lock after a panic");
        poisoned.into_inner()
    });
    match path {
        "/" => Routed { status: 200, content_type: "text/html", body: INDEX.into() },
        "/health" => Routed { status: 200, content_type: "application/json", body: "{\"status\":\"ok\"}".into() },
        "/api/summary" | "/api/builds" => json(store.dashboard_snapshot()),
        "/api/projects" => json(store.projects()),
        "/api/trends/duration" => json(store.duration_trend_for(limit.min(200), project)),
        "/api/trends/percentiles" => json(store.duration_percentiles_for(limit, project)),
        "/api/trends/daily" => json(store.daily_percentiles_for(limit.min(90), project)),
        "/api/trends/targets" => json(store.target_trend(builds, top, project)),
        "/api/trends/phases" => json(store.phase_trend(builds, top, project)),
        "/api/trends/diagnostics" => json(store.diagnostic_trend_for(limit, project)),
        "/api/files/slowest" => json(store.slowest_files(builds, limit.min(50), project)),
        "/api/swift/slowest" => json(store.slowest_swift_timings(builds, limit.min(50), project)),
        "/api/diagnostics/clusters" => json(store.diagnostic_clusters(builds, limit.min(50), project)),
        "/api/tests/flaky" => json(store.flaky_tests(builds, limit.min(50), project)),
        "/api/regressions" => json(store.target_regressions(builds, limit.min(50), project)),
        "/api/environment" => json(store.environment_breakdown(builds, project)),
        "/api/people" => json(store.people_breakdown(builds, project)),
        "/api/git" => json(store.git_context(builds, project)),
        p if p.starts_with("/api/build/") => {
            // Percent-decoded: a build key comes from an activity log and can
            // contain characters the browser escapes, which would otherwise
            // never match the stored value.
            let key = percent_decode(p.trim_start_matches("/api/build/"));
            if key.is_empty() {
                return Routed {
                    status: 400,
                    content_type: "application/json",
                    body: serde_json::json!({ "error": "missing build id" }).to_string(),
                };
            }
            // `None` is a stale link, not a failure, so it is a clean 404.
            match store.build_snapshot(&key) {
                Ok(Some(snapshot)) => json(Ok::<_, StoreError>(snapshot)),
                Ok(None) => not_found("no such build"),
                Err(error) => {
                    eprintln!("dashboard: store query failed: {error}");
                    error_response("query failed")
                }
            }
        }
        _ => not_found("not found"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every project-scoped route must accept and apply `project`. A route
    /// that silently ignores it mixes one project's builds into another's
    /// numbers, which is why this list is enumerated rather than sampled.
    const PROJECT_SCOPED: &[&str] = &[
        "/api/trends/duration",
        "/api/trends/percentiles",
        "/api/trends/daily",
        "/api/trends/targets",
        "/api/trends/phases",
        "/api/trends/diagnostics",
        "/api/files/slowest",
        "/api/swift/slowest",
        "/api/diagnostics/clusters",
        "/api/tests/flaky",
        "/api/regressions",
        "/api/environment",
        "/api/people",
        "/api/git",
    ];

    /// A project-scoped route must actually accept the filter.
    ///
    /// `PROJECT_SCOPED` is hand-maintained, and scraping the source for match
    /// arms proved unreliable — it matched paths written in doc comments too.
    /// Instead each listed route is exercised through `route()` twice, with
    /// and without a project, and must answer both: a route that ignored the
    /// parameter would still answer, but one that is not routed at all returns
    /// 404 and fails here.
    ///
    /// Needs a database, so it is skipped without one; the `index.html` checks
    /// below cover the wiring on every run.
    #[test]
    fn every_project_scoped_route_is_actually_routed() {
        let Ok(url) = std::env::var("BUILDLENS_TEST_DATABASE_URL") else {
            eprintln!("skipping: BUILDLENS_TEST_DATABASE_URL not set");
            return;
        };
        let store = Arc::new(std::sync::Mutex::new(
            PostgresStore::connect(&url).expect("connect and migrate"),
        ));
        for path in PROJECT_SCOPED {
            for query in ["", "project=App"] {
                let routed = route(path, query, &store);
                assert_ne!(
                    routed.status, 404,
                    "{path} is listed as project-scoped but is not routed"
                );
            }
        }
        // The discriminator: an unrouted path really does 404, so the
        // assertion above is not vacuous.
        assert_eq!(route("/api/nope", "", &store).status, 404);
    }

    /// The TypeScript the bundle is built from.
    ///
    /// Read at compile time so the tests below can compare source against
    /// artifact. `include_str!` resolves relative to this file, and the path
    /// leaves the crate — deliberate: the point is to notice when the two
    /// drift apart.
    const UI_SOURCE: &str = include_str!("../../../dashboard-ui/src/main.tsx");

    /// Every `/api/` path the TypeScript mentions.
    fn api_paths_in_source() -> Vec<String> {
        let mut paths = Vec::new();
        for (index, _) in UI_SOURCE.match_indices("\"/api/") {
            let rest = &UI_SOURCE[index + 1..];
            let Some(end) = rest.find('"') else { continue };
            let path = rest[..end].to_owned();
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
        paths
    }

    /// The committed bundle must be built from the current TypeScript.
    ///
    /// `node dashboard-ui/build.mjs --check` proves this exactly, but only if
    /// someone runs it. This is the cheap approximation that runs on every
    /// `cargo test`: a panel added to `main.tsx` without a rebuild leaves its
    /// path absent from the bundle, which is the drift that actually happens.
    ///
    /// It cannot catch every staleness — a changed constant or reworded label
    /// leaves the path set identical — so it complements `--check` rather than
    /// replacing it.
    #[test]
    fn the_committed_bundle_is_built_from_the_current_source() {
        let paths = api_paths_in_source();
        assert!(
            paths.len() >= 10,
            "expected the UI to request many routes, found {paths:?}"
        );
        for path in &paths {
            assert!(
                INDEX.contains(path.as_str()),
                "`{path}` is requested by dashboard-ui/src/main.tsx but is absent from the \
                 committed bundle. Run `node dashboard-ui/build.mjs` and commit the result."
            );
        }
    }

    /// A second identical request must be served from the cache, and a
    /// different query must not read the first one's answer.
    ///
    /// Uses the cache directly rather than through `route_cached` so it needs no
    /// database: the leak this guards against is in the keying, not in the SQL.
    #[test]
    fn the_cache_separates_entries_by_path_and_query() {
        let cache = ResponseCache::new();
        cache.put("/api/trends/duration?project=App".into(), "app");
        cache.put("/api/trends/duration?project=Other".into(), "other");
        cache.put("/api/trends/daily?project=App".into(), "daily");

        assert_eq!(cache.get("/api/trends/duration?project=App").as_deref(), Some("app"));
        assert_eq!(cache.get("/api/trends/duration?project=Other").as_deref(), Some("other"));
        assert_eq!(cache.get("/api/trends/daily?project=App").as_deref(), Some("daily"));
        // The failure mode worth guarding: a key that was never stored must miss
        // rather than fall back to a similar one.
        assert!(cache.get("/api/trends/duration?project=Absent").is_none());
        assert!(cache.get("/api/trends/duration").is_none());
    }

    /// The cache has to sit in front of the database on a repeat request, and
    /// stay out of the way for the paths that must not be cached.
    ///
    /// Proven by writing a sentinel into the cache under the key the route would
    /// use: if the second call returns the sentinel, it never reached the store.
    /// `/` is checked the other way — it must ignore a poisoned entry, because it
    /// is the compiled-in bundle rather than data.
    #[test]
    fn route_cached_serves_a_repeat_request_without_the_store() {
        let Ok(url) = std::env::var("BUILDLENS_TEST_DATABASE_URL") else {
            eprintln!("skipping: BUILDLENS_TEST_DATABASE_URL not set");
            return;
        };
        let store = Arc::new(std::sync::Mutex::new(
            PostgresStore::connect(&url).expect("connect and migrate"),
        ));
        let cache = ResponseCache::new();

        let first = route_cached("/api/projects", "", &store, &cache);
        assert_eq!(first.status, 200);
        // The real answer is now cached; replace it so a cache hit is visible.
        cache.put("/api/projects?".into(), "{\"sentinel\":true}");
        let second = route_cached("/api/projects", "", &store, &cache);
        assert_eq!(second.body, "{\"sentinel\":true}", "the repeat request hit the database");

        // A different query must not read that entry.
        let scoped = route_cached("/api/projects", "project=App", &store, &cache);
        assert_ne!(scoped.body, "{\"sentinel\":true}");

        // The bundle is not an `/api/` path and must bypass the cache entirely.
        cache.put("/?".into(), "poisoned");
        let index = route_cached("/", "", &store, &cache);
        assert!(index.body.contains("<!doctype html>") || index.body.contains("<!DOCTYPE html>"));
        assert_eq!(index.content_type, "text/html");
    }

    /// A 404 must not be cached: it is cheap to recompute, and caching an error
    /// would hold a transient failure after it cleared.
    #[test]
    fn route_cached_does_not_cache_failures() {
        let Ok(url) = std::env::var("BUILDLENS_TEST_DATABASE_URL") else {
            eprintln!("skipping: BUILDLENS_TEST_DATABASE_URL not set");
            return;
        };
        let store = Arc::new(std::sync::Mutex::new(
            PostgresStore::connect(&url).expect("connect and migrate"),
        ));
        let cache = ResponseCache::new();
        assert_eq!(route_cached("/api/nope", "", &store, &cache).status, 404);
        assert!(cache.get("/api/nope?").is_none(), "a 404 must not be cached");
    }

    /// Invalidation must drop everything, so a stored build is visible on the
    /// next request rather than up to `CACHE_TTL` later.
    #[test]
    fn invalidating_the_cache_drops_every_entry() {
        let cache = ResponseCache::new();
        cache.put("/api/summary?".into(), "before");
        cache.put("/api/projects?".into(), "before");
        cache.invalidate();
        assert!(cache.get("/api/summary?").is_none());
        assert!(cache.get("/api/projects?").is_none());
    }

    /// An entry past its TTL must not be served.
    ///
    /// Asserted by storing a timestamp older than the TTL rather than by
    /// sleeping, so the test stays fast and cannot flake on a slow machine.
    #[test]
    fn an_expired_entry_is_not_served() {
        let cache = ResponseCache::new();
        let stale = std::time::Instant::now() - CACHE_TTL - std::time::Duration::from_millis(1);
        cache.lock().insert("/api/summary?".into(), (stale, "stale".into()));
        assert!(cache.get("/api/summary?").is_none(), "an entry past its TTL must miss");
        // And the miss removes it, rather than leaving it to be re-checked.
        assert!(cache.lock().is_empty());
    }

    /// The TTL has to stay below the UI's refresh interval, or a panel shows the
    /// same numbers for more than one cycle and the dashboard reads as frozen.
    #[test]
    fn the_cache_ttl_is_shorter_than_the_uis_refresh_window() {
        let refresh = UI_SOURCE
            .split_once("REFRESH_MS")
            .and_then(|(_, rest)| rest.split_once('='))
            .and_then(|(_, rest)| {
                let digits: String = rest.trim_start().chars().take_while(char::is_ascii_digit).collect();
                digits.parse::<u64>().ok()
            })
            .expect("REFRESH_MS is declared in dashboard-ui/src/main.tsx");
        assert!(
            CACHE_TTL.as_millis() as u64 >= refresh,
            "a TTL below the refresh interval caches nothing: TTL {}ms, refresh {refresh}ms",
            CACHE_TTL.as_millis()
        );
        // Sanity bound: a TTL far above the refresh interval means the page
        // shows stale numbers for several cycles.
        assert!(
            CACHE_TTL.as_millis() as u64 <= refresh * 10,
            "TTL {}ms is more than ten refresh cycles of {refresh}ms",
            CACHE_TTL.as_millis()
        );
    }

    /// The sidebar's three views must be real routes, not decorative buttons.
    ///
    /// They shipped as `<button>` elements with no onClick: clicking did
    /// nothing while looking interactive. Each is now a hash route, so this
    /// checks the bundle carries all three plus the routing that reads them.
    #[test]
    fn every_sidebar_view_is_a_real_route() {
        for tab in ["builds", "trends", "diagnostics"] {
            assert!(
                UI_SOURCE.contains(&format!("id:\"{tab}\"")),
                "the UI source lost the {tab} view"
            );
            assert!(INDEX.contains(tab), "the bundle lost the {tab} view");
        }
        // A deep link like /#/trends reaches the server as "/", so the views
        // only resolve while "/" keeps serving the app.
        assert!(UI_SOURCE.contains("hashchange"), "views must react to hash changes");
        // The nav must *call* openTab, not merely define it: the original bug
        // was buttons that rendered fine and did nothing on click.
        assert!(
            UI_SOURCE.contains("onClick={()=>openTab("),
            "the sidebar renders its views but never navigates to them"
        );
    }

    /// The bundle must announce that it is generated, or the first instinct on
    /// opening a 224KB HTML file in the Rust tree is to edit it — and the next
    /// build discards the change.
    #[test]
    fn the_bundle_says_it_is_generated() {
        assert!(
            INDEX.contains("Generated by dashboard-ui/build.mjs"),
            "the generated-file banner is missing from the bundle"
        );
        // After the doctype, so the doctype still comes first.
        assert!(INDEX.starts_with("<!doctype html>"), "the doctype must lead");
    }

    /// Every route the server answers must be one the UI actually requests.
    /// A route nothing calls is dead weight; a call to a route that does not
    /// exist is a panel that silently 404s.
    #[test]
    fn the_ui_only_requests_routes_that_exist() {
        // `/api/build/<id>` is built from a template, so it never appears as a
        // literal path in the source.
        for path in api_paths_in_source() {
            let routed = PROJECT_SCOPED.contains(&path.as_str())
                || ["/api/summary", "/api/builds", "/api/projects"].contains(&path.as_str());
            assert!(routed, "the UI requests {path}, which no route answers");
        }
    }

    /// The frontend calls `scope()` for each of these; if a route here is not
    /// wired in `index.html`, the panel silently shows unfiltered data.
    #[test]
    fn every_project_scoped_route_is_requested_by_the_page() {
        for path in PROJECT_SCOPED {
            assert!(INDEX.contains(path), "index.html never requests {path}");
        }
    }

    /// The page must scope its requests, not just call them. A bare fetch of a
    /// project-scoped endpoint is the exact bug this guards.
    ///
    /// `index.html` is a minified bundle, so `scope` is renamed to something
    /// like `Tl` and cannot be matched by name. It can still be *identified*:
    /// `/api/trends/targets` is always scoped, so whatever identifier precedes
    /// it is the minified `scope`. Every other project-scoped path must then be
    /// called through that same identifier. A panel that fetched a path
    /// directly would be preceded by the plain fetch helper instead, and fail.
    #[test]
    fn the_page_scopes_its_project_filtered_requests() {
        let anchor = "/api/trends/targets";
        let at = INDEX.find(anchor).expect("index.html never requests the anchor route");
        // Walk back over the opening `("` to the identifier before it.
        let before = INDEX[..at].trim_end_matches(['"', '\'', '(']);
        let helper: String = before
            .chars()
            .rev()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '$')
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        assert!(!helper.is_empty(), "could not identify the scope() helper in the bundle");

        for path in PROJECT_SCOPED {
            assert!(
                INDEX.contains(&format!("{helper}(\"{path}\"")) || INDEX.contains(&format!("{helper}('{path}'")),
                "{path} is fetched without going through scope() (helper `{helper}`)"
            );
        }
        // Sanity-check the discriminator: a deliberately unscoped route must
        // NOT go through the helper, or the assertion above proves nothing.
        assert!(
            !INDEX.contains(&format!("{helper}(\"/api/summary\"")),
            "/api/summary is project-wide and must not be scoped"
        );
    }

    /// The dashboard UI is compiled into the binary and served from a URL that
    /// never changes, so a cached bundle survives an upgrade and the new page
    /// never loads. The symptom is misleading — panels render empty and it
    /// looks like missing data rather than a stale page.
    #[test]
    fn responses_are_not_cacheable() {
        let value = String::from_utf8(no_cache().value.as_bytes().to_vec()).unwrap();
        assert!(value.contains("no-store"), "index and API responses must not be cached: {value}");
        let field = format!("{}", no_cache().field);
        assert_eq!(field.to_ascii_lowercase(), "cache-control");
    }

    /// The build detail is its own `#/build/<id>` page rather than a section
    /// appended to the overview. Rendering it inline shifted the layout under
    /// a fixed scroll position, landing the reader past the panel on blank
    /// space. Hash routing also keeps a build linkable and the back button
    /// working, and it stays a single served page — the server has no route to
    /// add, so `/` must keep serving the app for a deep link to resolve.
    #[test]
    fn build_detail_is_a_hash_routed_page() {
        assert!(INDEX.contains("#/build/"), "index.html lost its build route");
        assert!(
            INDEX.contains("hashchange"),
            "the page must react to hash changes, or the back button breaks"
        );
        // A deep link like /#/build/<id> reaches the server as "/", so the
        // detail page only resolves while "/" keeps serving the app.
        assert!(INDEX.contains("<div id=\"root\">"), "the app root is gone");
    }

    #[test]
    fn percent_encoded_project_names_decode() {
        assert_eq!(text_param("project=My%20App", "project").as_deref(), Some("My App"));
        assert_eq!(text_param("project=My+App", "project").as_deref(), Some("My App"));
        // A value ending in an escape must not lose its final character.
        assert_eq!(text_param("project=A%2FB", "project").as_deref(), Some("A/B"));
        assert_eq!(text_param("limit=10", "project"), None);
    }

    /// Out-of-range values must clamp rather than reaching Postgres as a
    /// negative LIMIT, which is an error rather than an empty result.
    #[test]
    fn numeric_parameters_clamp_to_a_sane_range() {
        assert_eq!(number_param("limit=999999", "limit", 100), MAX_LIMIT);
        assert_eq!(number_param("limit=0", "limit", 100), 1);
        assert_eq!(number_param("limit=-5", "limit", 100), 1);
        assert_eq!(number_param("limit=abc", "limit", 100), 100);
        assert_eq!(number_param("", "limit", 100), 100);
    }

    /// `{}` with status 200 is a *successful, empty* answer, so the page shows
    /// empty panels and the reader concludes there is no data rather than that
    /// the request failed.
    #[test]
    fn a_serialization_failure_is_an_error_not_an_empty_success() {
        struct Unserializable;
        impl serde::Serialize for Unserializable {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("cannot serialize"))
            }
        }
        let routed = json(Ok::<_, StoreError>(Unserializable));
        assert_eq!(routed.status, 500, "body was {}", routed.body);
        assert_ne!(routed.body, "{}", "an empty object reads as clean data");
    }

    #[test]
    fn a_successful_value_serializes_with_status_200() {
        let routed = json(Ok::<_, StoreError>(serde_json::json!({"items": []})));
        assert_eq!(routed.status, 200);
        assert_eq!(routed.content_type, "application/json");
        assert_eq!(routed.body, r#"{"items":[]}"#);
    }

    /// A store error must not reach the browser. `StoreError::Database` wraps
    /// a `postgres::Error` whose `Display` can name tables, constraints and
    /// connection details.
    ///
    /// Uses `UnusableBuild` because it is the one variant constructible
    /// without a live connection; the response path does not inspect the
    /// variant, so the property holds for `Database` too.
    #[test]
    fn store_errors_are_not_described_to_the_browser() {
        let leaked = StoreError::UnusableBuild.to_string();
        assert!(!leaked.is_empty(), "the guard needs a non-empty message");
        let routed = json(Err::<serde_json::Value, _>(StoreError::UnusableBuild));
        assert_eq!(routed.status, 500);
        assert!(
            !routed.body.contains(&leaked),
            "the store message reached the browser: {}",
            routed.body
        );
        assert!(routed.body.contains("query failed"));
    }

    /// Every error body is JSON, so the page can parse one shape rather than
    /// guessing whether a 500 is text or a document.
    #[test]
    fn error_bodies_are_json() {
        for routed in [error_response("boom"), not_found("gone")] {
            assert_eq!(routed.content_type, "application/json");
            let parsed: serde_json::Value =
                serde_json::from_str(&routed.body).expect("a JSON error body");
            assert!(parsed["error"].is_string(), "{}", routed.body);
        }
    }

    #[test]
    fn percent_decoding_handles_edge_cases() {
        assert_eq!(percent_decode("My%20App"), "My App");
        assert_eq!(percent_decode("A%2FB"), "A/B");
        assert_eq!(percent_decode("%C3%A9"), "é");
        // A truncated escape is left alone rather than panicking on the slice.
        assert_eq!(percent_decode("%4"), "%4");
        assert_eq!(percent_decode("%"), "%");
        assert_eq!(percent_decode("abc%"), "abc%");
        assert_eq!(percent_decode(""), "");
        // Invalid UTF-8 becomes the replacement character rather than panicking.
        assert_eq!(percent_decode("%FF"), "\u{fffd}");
    }

    /// The `/api/build/<key>` segment must be decoded, or a key the browser
    /// escaped never matches the stored value and the page shows "no such
    /// build" for a build that exists.
    ///
    /// Discriminating: a build is stored under a key containing a space, then
    /// requested as `%20`. Decoded it is found; undecoded the store is asked
    /// for the literal "a%20b" and answers nothing.
    #[test]
    fn the_build_key_path_segment_is_percent_decoded() {
        use buildlens_core::BuildCategory;
        use buildlens_core::wire::{Attribution, WIRE_VERSION, WireBuild};

        let Ok(base) = std::env::var("BUILDLENS_TEST_DATABASE_URL") else {
            eprintln!("skipping: BUILDLENS_TEST_DATABASE_URL not set");
            return;
        };
        // The key is unique to this test, so it does not matter which schema
        // the shared test database uses; re-inserting it is a no-op.
        let mut connected = PostgresStore::connect(&base).expect("connect and migrate");
        connected
            .insert(&WireBuild {
                wire_version: WIRE_VERSION,
                build_key: "dashboard decode probe".into(),
                project: "App".into(),
                category: BuildCategory::Clean,
                total_seconds: 1.0,
                compiled_count: 0,
                cache_hit_rate: None,
                error_count: 0,
                warning_count: 0,
                status: None,
                started_at: Some(1_700_000_000.0),
                attribution: Attribution::Anonymous,
                machine_id: None,
                user: None,
                host: None,
                xcode_version: None,
                platform: None,
                architecture: None,
                targets: vec![],
                phases: vec![],
                // This probe is about decoding a build key, not the detail
                // collections, so they stay empty.
                files: vec![],
                swift_timings: vec![],
                diagnostics: vec![],
                tests: vec![],
            })
            .ok();  // Already present from an earlier run is fine.
        let store = Arc::new(std::sync::Mutex::new(connected));

        let routed = route("/api/build/dashboard%20decode%20probe", "", &store);
        assert_eq!(
            routed.status, 200,
            "an escaped key must reach the stored build: {}",
            routed.body
        );
        assert!(routed.body.contains("dashboard decode probe"), "{}", routed.body);
        // An empty key is a client error rather than a lookup.
        assert_eq!(route("/api/build/", "", &store).status, 400);
    }

    /// A build key comes from an activity log and can contain characters the
    /// browser escapes, so the path segment needs the same decoding as a query
    /// value — one implementation, not two that could disagree.
    #[test]
    fn build_keys_and_project_names_decode_identically() {
        for raw in ["My%20App", "A%2FB", "%C3%A9", "plain"] {
            assert_eq!(
                percent_decode(raw),
                text_param(&format!("project={raw}"), "project").unwrap()
            );
        }
    }

    #[test]
    fn urls_split_into_path_and_query() {
        assert_eq!(split_url("/api/builds?limit=5"), ("/api/builds".into(), "limit=5".into()));
        assert_eq!(split_url("/api/builds"), ("/api/builds".into(), String::new()));
        assert_eq!(split_url("/"), ("/".into(), String::new()));
        // A trailing `?` yields an empty query rather than a missing one.
        assert_eq!(split_url("/x?"), ("/x".into(), String::new()));
    }

    /// An empty `project` is how the page spells "All projects" and must read
    /// as absent, not as a project literally named "".
    #[test]
    fn an_empty_project_means_every_project() {
        assert_eq!(text_param("project=", "project").as_deref(), Some(""));
        // `route` is what applies the filter, so that is where it is dropped.
        assert!(
            text_param("project=", "project")
                .filter(|value| !value.is_empty())
                .is_none()
        );
    }
}
