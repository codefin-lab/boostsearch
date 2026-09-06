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
use axum::routing::get;
use boostsearch::console::Console;

type Shared = Arc<Console>;

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
    let console = match Console::open(home.into(), std::path::Path::new("console"), base_path) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    println!(
        "boostsearch console: OpenSearch Dashboards {} ({} plugins) on {addr}",
        console.pinned.version,
        console.pinned.bundles.len()
    );

    let build = console.pinned.build_number;
    let routes: Router<Shared> = Router::new()
        .route("/", get(root))
        .route("/app/{app}", get(page))
        .route("/app/{app}/{*rest}", get(page))
        .route("/bootstrap.js", get(bootstrap))
        .route("/startup.js", get(startup))
        .route(&format!("/{build}/bundles/{{*rest}}"), get(bundle))
        .route("/ui/{*rest}", get(ui_asset))
        .route("/translations/{locale}", get(translations))
        .route("/api/status", get(status));
    // a base path is a prefix on every route, and the one route that is not
    // under it is the redirect that sends a reader to it
    let routes: Router<Shared> = match console.base_path.as_str() {
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
    redirect(&console.at("/app/home"))
}

fn redirect(to: &str) -> Response {
    (StatusCode::FOUND, [(header::LOCATION, to.to_string())]).into_response()
}

/// Every application is the same page. Which one it is is in the URL, and the
/// front end reads it from there.
async fn page(State(console): State<Shared>, _app: Path<String>) -> Response {
    let body = console.page();
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
    script(console.bootstrap())
}

async fn startup(State(console): State<Shared>) -> Response {
    script(console.startup())
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
    served(console.bundle(&rest, accepts(&headers)), "public, max-age=31536000")
}

async fn ui_asset(
    State(console): State<Shared>,
    Path(rest): Path<String>,
    headers: HeaderMap,
) -> Response {
    served(console.ui_asset(&rest, accepts(&headers)), "public, max-age=31536000")
}

async fn translations(State(console): State<Shared>, Path(locale): Path<String>) -> Response {
    let locale = locale.trim_end_matches(".json");
    served(Some(console.translations(locale)), "must-revalidate")
}

/// What the front end asks before it decides the server is there.
///
/// The whole of it is 13.2's; this is enough for the page to load and for a
/// reader to be told the difference between a server that is up and one that
/// is not.
async fn status(State(console): State<Shared>) -> Response {
    axum::Json(serde_json::json!({
        "name": "boostsearch-console",
        "uuid": "00000000-0000-0000-0000-000000000000",
        "version": {
            "number": console.pinned.version,
            "build_hash": "unknown",
            "build_number": console.pinned.build_number,
            "build_snapshot": false,
        },
        "status": {
            "overall": {"state": "green", "title": "Green", "nickname": "Looking good"},
            "statuses": [],
        },
        "metrics": serde_json::Value::Null,
    }))
    .into_response()
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
