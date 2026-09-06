//! The server the OpenSearch Dashboards front end talks to.
//!
//! It is a separate program from the engine for the same reason the one it
//! replaces is: they are deployed apart, on different machines as often as
//! not, and a console that has to run beside its engine is a worse console.
//!
//!   BOOSTSEARCH_CONSOLE_ADDR       where to listen (default 127.0.0.1:5601)
//!   BOOSTSEARCH_CONSOLE_PATH       an OpenSearch Dashboards distribution
//!   BOOSTSEARCH_CONSOLE_BASE_PATH  the path everything is served under
//!   BOOSTSEARCH_ENGINE             the engine behind it
//!   BOOSTSEARCH_CONSOLE_OVERRIDE   `key=value` pairs, comma separated: settings
//!                                  an operator fixes and no reader may change
//!
//! The distribution is pointed at rather than carried, the way the geoip
//! databases are: it is the OpenSearch project's to publish, it is a gigabyte,
//! and which one is in front of this server is an operator's decision.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use boostsearch::console::Console;
use boostsearch::console::engine::{Engine, Failed};
use boostsearch::console::settings::Settings;
use serde_json::Value;

/// Everything a handler needs: what to serve, and what to serve it from.
struct Serving {
    console: Console,
    engine: Engine,
}

type Shared = Arc<Serving>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let addr =
        std::env::var("BOOSTSEARCH_CONSOLE_ADDR").unwrap_or_else(|_| "127.0.0.1:5601".to_string());
    let home = std::env::var("BOOSTSEARCH_CONSOLE_PATH").unwrap_or_default();
    if home.is_empty() {
        eprintln!(
            "BOOSTSEARCH_CONSOLE_PATH is not set. It is an OpenSearch Dashboards\n\
             distribution -- the front end this serves, which is theirs rather than\n\
             ours. In a container it is /usr/share/opensearch-dashboards."
        );
        std::process::exit(2);
    }
    let base_path = std::env::var("BOOSTSEARCH_CONSOLE_BASE_PATH").unwrap_or_default();
    let base_path = base_path.trim_end_matches('/').to_string();
    let console = match Console::open(
        home.into(),
        std::path::Path::new("console"),
        base_path,
        boostsearch::console::overrides_from(
            &std::env::var("BOOSTSEARCH_CONSOLE_OVERRIDE").unwrap_or_default(),
        ),
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let engine_url =
        std::env::var("BOOSTSEARCH_ENGINE").unwrap_or_else(|_| "http://127.0.0.1:9200".into());
    println!(
        "boostsearch console: OpenSearch Dashboards {} ({} bundles) on {addr}, engine {engine_url}",
        console.pinned.version,
        console.pinned.bundles.len()
    );

    let build = console.pinned.build_number;
    let base = console.base_path.clone();
    let console = Arc::new(Serving { console, engine: Engine::at(&engine_url) });
    let routes: Router<Shared> = Router::new()
        .route("/", get(root))
        .route("/app/{app}", get(page))
        .route("/app/{app}/{*rest}", get(page))
        .route("/bootstrap.js", get(bootstrap))
        .route("/startup.js", get(startup))
        .route(&format!("/{build}/bundles/{{*rest}}"), get(bundle))
        .route("/ui/{*rest}", get(ui_asset))
        .route("/translations/{locale}", get(translations))
        .route("/api/status", get(status))
        .route("/api/core/capabilities", post(capabilities))
        .route("/api/opensearch-dashboards/settings", get(read_settings).post(write_settings))
        .route(
            "/api/opensearch-dashboards/settings/{key}",
            post(write_setting).delete(reset_setting),
        );
    // a base path is a prefix on every route, and the one route that is not
    // under it is the redirect that sends a reader to it
    let routes: Router<Shared> = match base.as_str() {
        "" => routes,
        base => Router::new().nest(base, routes).route("/", get(root)),
    };
    let app = routes.with_state(console);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// A reader who asked for nothing in particular is sent to the home page.
async fn root(State(console): State<Shared>) -> Response {
    redirect(&console.console.at("/app/home"))
}

fn redirect(to: &str) -> Response {
    (StatusCode::FOUND, [(header::LOCATION, to.to_string())]).into_response()
}

/// Every application is the same page. Which one it is is in the URL, and the
/// front end reads it from there.
async fn page(State(serving): State<Shared>, _app: Path<String>) -> Response {
    // the settings are read for the page rather than fetched by it: a console
    // that drew itself with the default theme and then redrew with the chosen
    // one would flash white at every reader who did not want it. An engine
    // that cannot be reached is a page that still loads, with the defaults --
    // which is better than no page at all.
    let user = {
        let serving = serving.clone();
        tokio::task::spawn_blocking(move || settings_of(&serving).read())
            .await
            .ok()
            .and_then(|r| r.ok())
            .and_then(|found| found.get("settings").cloned())
            .unwrap_or_else(|| serde_json::json!({}))
    };
    let console = &serving.console;
    let body = console.page(user);
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8".to_string()),
            (header::CONTENT_SECURITY_POLICY, console.content_security_policy()),
        ],
        body,
    )
        .into_response()
}

async fn bootstrap(State(console): State<Shared>) -> Response {
    script(console.console.bootstrap())
}

async fn startup(State(console): State<Shared>) -> Response {
    script(console.console.startup())
}

fn script(body: String) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/javascript; charset=utf-8"),
            // the boot script names the bundles, and the bundles are named
            // after the build they came from, so a stale one is a page that
            // loads files which are no longer there
            (header::CACHE_CONTROL, "must-revalidate"),
        ],
        body,
    )
        .into_response()
}

async fn bundle(
    State(console): State<Shared>,
    Path(rest): Path<String>,
    headers: HeaderMap,
) -> Response {
    // a bundle's name carries the build it came from, so it can be kept for
    // as long as the reader likes: a new build asks for a different URL
    served(console.console.bundle(&rest, accepts(&headers)), "public, max-age=31536000")
}

async fn ui_asset(
    State(console): State<Shared>,
    Path(rest): Path<String>,
    headers: HeaderMap,
) -> Response {
    served(console.console.ui_asset(&rest, accepts(&headers)), "public, max-age=31536000")
}

async fn translations(State(console): State<Shared>, Path(locale): Path<String>) -> Response {
    let locale = locale.trim_end_matches(".json");
    served(Some(console.console.translations(locale)), "must-revalidate")
}

/// Whether the console can do its job, which is a question about the engine.
///
/// A console with no engine behind it can still serve every page and answer
/// nothing useful on any of them, so saying green because this process is
/// running would be the least helpful true statement available.
async fn status(State(serving): State<Shared>) -> Response {
    let console = &serving.console;
    let reachable = tokio::task::spawn_blocking({
        let engine = serving.engine.clone();
        move || engine.reachable()
    })
    .await;
    let (state, message) = match reachable {
        Ok(Ok(_)) => ("green", "OpenSearch is available".to_string()),
        Ok(Err(e)) => ("red", format!("OpenSearch is not available: {}", e.message)),
        Err(e) => ("red", format!("the check could not be run: {e}")),
    };
    let since = boostsearch::console::now();
    let colour = |state: &str| match state {
        "green" => ("success", "secondary"),
        _ => ("alert", "danger"),
    };
    let (icon, ui_colour) = colour(state);
    axum::Json(serde_json::json!({
        "name": "boostsearch-console",
        "uuid": console.uuid(),
        "version": {
            "number": console.pinned.version,
            "build_hash": console.pinned.env.pointer("/packageInfo/buildSha")
                .and_then(|v| v.as_str()).unwrap_or("unknown"),
            "build_number": console.pinned.build_number,
            "build_snapshot": false,
        },
        "status": {
            "overall": {
                "since": since,
                "state": state,
                "title": if state == "green" { "Green" } else { "Red" },
                "nickname": if state == "green" { "Looking good" } else { "Danger Will Robinson" },
                "icon": icon,
                "uiColor": ui_colour,
            },
            "statuses": [{
                "id": format!("core:opensearch@{}", console.pinned.version),
                "message": message,
                "since": since,
                "state": state,
                "icon": icon,
                "uiColor": ui_colour,
            }],
        },
        "metrics": serde_json::Value::Null,
    }))
    .into_response()
}

/// What a caller may do.
///
/// Most of it is what the plugins between them decided, which is pinned.
/// `navLinks` is not: it is one entry per application the caller asked about,
/// so it is the request's shape rather than the server's.
async fn capabilities(State(serving): State<Shared>, body: axum::Json<Value>) -> Response {
    let asked: Vec<String> = body
        .get("applications")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    axum::Json(serving.console.capabilities(&asked)).into_response()
}

fn settings_of(serving: &Serving) -> Settings<'_> {
    Settings::new(
        &serving.engine,
        &serving.console.pinned.version,
        serving.console.pinned.build_number,
        &serving.console.overrides,
    )
}

/// Reading and writing settings waits on the engine, and waiting on a socket
/// inside a handler holds a worker of the runtime -- so it happens off it.
async fn on_engine<F>(serving: Shared, work: F) -> Response
where
    F: FnOnce(&Serving) -> Result<Value, Failed> + Send + 'static,
{
    match tokio::task::spawn_blocking(move || work(&serving)).await {
        Ok(Ok(found)) => axum::Json(found).into_response(),
        Ok(Err(e)) => refused(e),
        Err(e) => refused(Failed { status: 500, message: format!("{e}") }),
    }
}

fn refused(e: Failed) -> Response {
    let status = StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (
        status,
        axum::Json(serde_json::json!({
            "statusCode": e.status,
            "error": status.canonical_reason().unwrap_or("Error"),
            "message": e.message,
        })),
    )
        .into_response()
}

async fn read_settings(State(serving): State<Shared>) -> Response {
    on_engine(serving, |s| settings_of(s).read()).await
}

async fn write_settings(State(serving): State<Shared>, body: axum::Json<Value>) -> Response {
    let changes = body.get("changes").and_then(|v| v.as_object()).cloned().unwrap_or_default();
    on_engine(serving, move |s| settings_of(s).write(&changes)).await
}

async fn write_setting(
    State(serving): State<Shared>,
    Path(key): Path<String>,
    body: axum::Json<Value>,
) -> Response {
    let mut changes = serde_json::Map::new();
    changes.insert(key, body.get("value").cloned().unwrap_or(Value::Null));
    on_engine(serving, move |s| settings_of(s).write(&changes)).await
}

async fn reset_setting(State(serving): State<Shared>, Path(key): Path<String>) -> Response {
    on_engine(serving, move |s| settings_of(s).reset(&key)).await
}

fn accepts(headers: &HeaderMap) -> &str {
    headers.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok()).unwrap_or("")
}

fn served(found: Option<boostsearch::console::assets::Served>, cache: &str) -> Response {
    let Some(found) = found else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, found.kind)
        .header(header::CACHE_CONTROL, cache);
    if let Some(encoding) = found.encoding {
        response = response.header(header::CONTENT_ENCODING, HeaderValue::from_static(encoding));
    }
    response.body(Body::from(found.bytes)).expect("a response with a body")
}
