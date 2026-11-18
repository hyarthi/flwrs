//! The web UI (feature `web-ui`): the Svelte app in `app/web/`, built by
//! `build.rs` into `$OUT_DIR/web` (with `.br` / `.gz` variants next to each
//! compressible file) and embedded here. Served from `/`, beside the API
//! under `/api`, whenever the HTTP server runs (`[http] enabled`).
//!
//! - A file that exists is served as is: the compressed variant the client
//!   accepts (brotli, then gzip), an `ETag` (`If-None-Match` → 304), and
//!   `immutable` caching for `assets/` (their names carry a content hash).
//! - Any other path without a file extension is a client-side route and gets
//!   `index.html` (`no-cache`), so a reload on `/scenes/by-id/…` works.
//! - A missing file (`assets/…`, or a name with an extension) is a 404.

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::{Embed, EmbeddedFile};

#[derive(Embed)]
#[folder = "$OUT_DIR/web"]
struct Assets;

/// The app's pages run nothing but its own files: no inline scripts, no
/// other origins. Inline styles are allowed: Svelte and the canvas library
/// set `style` attributes.
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self'; \
     style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self'; \
     connect-src 'self'; object-src 'none'; base-uri 'self'; form-action 'self'; \
     frame-ancestors 'none'";

/// The UI as a root router: a fallback, so every route the server defines
/// (`/api`, `/api-docs`, `/swagger-ui`) comes first.
pub(crate) fn router() -> Router {
    Router::new().fallback(serve)
}

async fn serve(method: Method, uri: Uri, headers: HeaderMap) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, "GET, HEAD")],
        )
            .into_response();
    }
    let Some((name, file)) = lookup(uri.path()) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    // The compressed variant the client accepts, if the build made one.
    let (encoding, body) = [("br", ".br"), ("gzip", ".gz")]
        .into_iter()
        .filter(|(coding, _)| accepts(&headers, coding))
        .find_map(|(coding, ext)| Assets::get(&format!("{name}{ext}")).map(|f| (Some(coding), f)))
        .unwrap_or((None, file.clone()));

    let etag = format!("\"{}\"", hex(&body.metadata.sha256_hash()[..16]));
    let cache = if name.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };

    let mut response = if not_modified(&headers, &etag) {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        let mut response = Response::new(Body::from(body.data.into_owned()));
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, content_type(&file));
        if let Some(coding) = encoding {
            response
                .headers_mut()
                .insert(header::CONTENT_ENCODING, HeaderValue::from_static(coding));
        }
        response
    };
    let h = response.headers_mut();
    if let Ok(etag) = HeaderValue::from_str(&etag) {
        h.insert(header::ETAG, etag);
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    h.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    response
}

/// The file for a request path: the file itself, `index.html` for a
/// client-side route, or `None` (404).
fn lookup(path: &str) -> Option<(String, EmbeddedFile)> {
    let path = path.trim_start_matches('/');
    // Compressed variants are only served through Accept-Encoding.
    let variant = path.ends_with(".br") || path.ends_with(".gz");
    if !path.is_empty()
        && !variant
        && let Some(file) = Assets::get(path)
    {
        return Some((path.to_string(), file));
    }
    let last = path.rsplit('/').next().unwrap_or("");
    if path.starts_with("assets/") || last.contains('.') {
        return None;
    }
    Assets::get("index.html").map(|file| ("index.html".to_string(), file))
}

/// Whether `Accept-Encoding` lists `coding` with a non-zero quality.
fn accepts(headers: &HeaderMap, coding: &str) -> bool {
    headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|part| {
            let mut parts = part.split(';');
            let name = parts.next().unwrap_or("").trim();
            let quality = parts
                .find_map(|p| p.trim().strip_prefix("q="))
                .and_then(|q| q.trim().parse::<f32>().ok())
                .unwrap_or(1.0);
            name.eq_ignore_ascii_case(coding) && quality > 0.0
        })
}

fn not_modified(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|tag| {
            let tag = tag.trim();
            tag == "*" || tag.trim_start_matches("W/") == etag
        })
}

fn content_type(file: &EmbeddedFile) -> HeaderValue {
    let mime = file.metadata.mimetype();
    let textual = mime.starts_with("text/")
        || mime == "application/javascript"
        || mime == "application/json"
        || mime == "image/svg+xml";
    let value = if textual && !mime.contains("charset") {
        format!("{mime}; charset=utf-8")
    } else {
        mime.to_string()
    };
    HeaderValue::from_str(&value).unwrap_or(HeaderValue::from_static("application/octet-stream"))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn get(path: &str, headers: &[(&str, &str)]) -> Response {
        let mut request = Request::get(path);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        router()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn text(response: Response) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// The hashed name of the built JS entry (`assets/index-<hash>.js`).
    fn script() -> String {
        Assets::iter()
            .find(|name| name.starts_with("assets/") && name.ends_with(".js"))
            .expect("a built script")
            .to_string()
    }

    #[tokio::test]
    async fn root_and_client_routes_get_the_app() {
        for path in [
            "/",
            "/scenes",
            "/scenes/by-id/01JABC/stats",
            "/plugins/new-remote",
        ] {
            let response = get(path, &[]).await;
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            let h = response.headers();
            assert_eq!(h[header::CONTENT_TYPE], "text/html; charset=utf-8");
            assert_eq!(h[header::CACHE_CONTROL], "no-cache");
            assert!(
                h[header::CONTENT_SECURITY_POLICY]
                    .to_str()
                    .unwrap()
                    .contains("script-src 'self'")
            );
            assert_eq!(h[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
            assert!(text(response).await.contains("<div id=\"app\">"));
        }
    }

    #[tokio::test]
    async fn assets_are_immutable_and_compressed_when_accepted() {
        let path = format!("/{}", script());

        let plain = get(&path, &[]).await;
        assert_eq!(plain.status(), StatusCode::OK);
        assert!(plain.headers().get(header::CONTENT_ENCODING).is_none());
        assert!(
            plain.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .contains("javascript")
        );
        assert_eq!(
            plain.headers()[header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );

        let br = get(&path, &[("accept-encoding", "gzip, deflate, br")]).await;
        assert_eq!(br.headers()[header::CONTENT_ENCODING], "br");
        assert_eq!(br.headers()[header::VARY], "Accept-Encoding");

        let gzip = get(&path, &[("accept-encoding", "gzip, br;q=0")]).await;
        assert_eq!(gzip.headers()[header::CONTENT_ENCODING], "gzip");
        assert_ne!(gzip.headers()[header::ETAG], br.headers()[header::ETAG]);
    }

    #[tokio::test]
    async fn etag_gives_not_modified() {
        let path = format!("/{}", script());
        let first = get(&path, &[]).await;
        let etag = first.headers()[header::ETAG].to_str().unwrap().to_string();
        let again = get(&path, &[("if-none-match", &etag)]).await;
        assert_eq!(again.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(again.headers()[header::ETAG], etag.as_str());
        assert!(text(again).await.is_empty());
    }

    #[tokio::test]
    async fn missing_files_are_not_found() {
        for path in ["/assets/nope.js", "/missing.png", "/favicon.svg.br"] {
            assert_eq!(
                get(path, &[]).await.status(),
                StatusCode::NOT_FOUND,
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn only_get_and_head() {
        let response = router()
            .oneshot(Request::post("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(response.headers()[header::ALLOW], "GET, HEAD");
    }
}
