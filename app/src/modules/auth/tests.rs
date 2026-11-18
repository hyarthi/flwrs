use super::config::{AuthConfig, AuthSettings, normalize_origin};
use super::session::{Claims, Sealer};
use super::*;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Method, Request};
use axum::routing::get;
use axum::{Extension, Json};
use http_body_util::BodyExt;
use std::net::SocketAddr;
use tower::ServiceExt;

const KEY: &str = "0123456789abcdef-test-key";
const LOCAL: &str = "127.0.0.1:5000";
const REMOTE: &str = "203.0.113.7:5000";

fn settings(key: Option<&str>) -> AuthSettings {
    AuthSettings {
        secret_key: key.map(str::to_string),
        session_ttl: Duration::from_secs(3_600),
        allowed_origins: vec!["https://ui.example.com".into()],
        secure_cookie: None,
    }
}

fn leak(auth: Auth) -> &'static Auth {
    Box::leak(Box::new(auth))
}

/// The API as `http.rs` builds it, with a protected `/ping` (GET and POST)
/// that echoes the principal's provider, and a fallback.
fn app(auth: &'static Auth, peer: &str) -> Router {
    async fn ping(Extension(p): Extension<Principal>) -> Json<String> {
        Json(p.provider)
    }
    let protected = Router::new()
        .route("/ping", get(ping).post(ping))
        .merge(api::Api::protected_router())
        .fallback(|| async { StatusCode::NOT_FOUND });
    let peer: SocketAddr = peer.parse().unwrap();
    Router::new()
        .nest(
            "/api",
            auth.protect(protected).merge(api::Api::public_router(auth)),
        )
        .layer(MockConnectInfo(peer))
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    json: serde_json::Value,
}

async fn call(app: &Router, request: Request<Body>) -> Reply {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    Reply {
        status,
        headers,
        json,
    }
}

fn request(method: Method, uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("host"))
    {
        builder = builder.header(header::HOST, "flwrs.lan:8080");
    }
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::empty()).unwrap()
}

fn login_request(key: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::post("/api/auth/login")
        .header(header::HOST, "flwrs.lan:8080")
        .header(header::CONTENT_TYPE, "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder
        .body(Body::from(serde_json::json!({ "key": key }).to_string()))
        .unwrap()
}

/// Logs in and returns the `Cookie` header value for the session.
async fn log_in(app: &Router) -> String {
    let reply = call(app, login_request(KEY, &[])).await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json);
    let set = reply.headers[header::SET_COOKIE].to_str().unwrap();
    set.split(';').next().unwrap().to_string()
}

// ---- config ----

#[test]
fn key_sources_and_checks() {
    let resolve = |c: AuthConfig, env: Option<&str>| c.resolve(env.map(str::to_string));

    // Nothing anywhere: loopback only, defaults.
    let s = resolve(AuthConfig::default(), None).unwrap();
    assert!(s.secret_key.is_none());
    assert_eq!(s.session_ttl, Duration::from_secs(7 * 86_400));

    // The environment when the config names none; the config wins otherwise.
    let s = resolve(AuthConfig::default(), Some(KEY)).unwrap();
    assert_eq!(s.secret_key.as_deref(), Some(KEY));
    let c = AuthConfig {
        secret_key: Some(format!("  {KEY}x\n")),
        ..Default::default()
    };
    assert_eq!(
        resolve(c, Some(KEY)).unwrap().secret_key,
        Some(format!("{KEY}x"))
    );

    // A file, trimmed.
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("key");
    std::fs::write(&file, format!("{KEY}\n")).unwrap();
    let c = AuthConfig {
        secret_key_file: Some(file.clone()),
        ..Default::default()
    };
    assert_eq!(resolve(c, None).unwrap().secret_key.as_deref(), Some(KEY));

    // Every problem at once.
    let c = AuthConfig {
        secret_key: Some("short".into()),
        session_ttl: Some("forever".into()),
        allowed_origins: vec![
            "https://ok.example.com".into(),
            "ftp://x".into(),
            "https://x.com/path".into(),
        ],
        ..Default::default()
    };
    let problems = resolve(c, None).unwrap_err();
    assert_eq!(problems.len(), 4, "{problems:?}");
    assert!(
        problems[0].contains("shorter than 16 bytes"),
        "{problems:?}"
    );

    let both = AuthConfig {
        secret_key: Some(KEY.into()),
        secret_key_file: Some(file),
        ..Default::default()
    };
    assert!(resolve(both, None).unwrap_err()[0].contains("not both"));
    let missing = AuthConfig {
        secret_key_file: Some(dir.path().join("nope")),
        ..Default::default()
    };
    assert!(resolve(missing, None).unwrap_err()[0].contains("secret_key_file"));
    let long = AuthConfig {
        session_ttl: Some("400d".into()),
        ..Default::default()
    };
    assert!(resolve(long, None).unwrap_err()[0].contains("longer than a year"));
}

#[test]
fn origins_are_normalized() {
    assert_eq!(
        normalize_origin("HTTPS://UI.Example.com:443/").as_deref(),
        Some("https://ui.example.com")
    );
    assert_eq!(normalize_origin("http://h:80").as_deref(), Some("http://h"));
    assert_eq!(
        normalize_origin("http://h:8080").as_deref(),
        Some("http://h:8080")
    );
    for bad in [
        "h",
        "ftp://h",
        "https://",
        "https://h/x",
        "https://u@h",
        "null",
    ] {
        assert_eq!(normalize_origin(bad), None, "{bad}");
    }
}

// ---- the sealed cookie ----

#[test]
fn sessions_open_only_with_their_key_and_untouched() {
    let sealer = Sealer::new(KEY.as_bytes());
    let claims = Claims::new("key", "admin", 1, 2);
    let sealed = sealer.seal(&claims).unwrap();
    assert_eq!(sealer.open(&sealed), Some(claims.clone()));
    // Fresh nonces: the same claims never seal to the same text.
    assert_ne!(sealer.seal(&claims).unwrap(), sealed);

    assert_eq!(
        Sealer::new(b"another key, just as long").open(&sealed),
        None
    );
    let mut tampered = sealed.into_bytes();
    let i = tampered.len() / 2;
    tampered[i] = if tampered[i] == b'A' { b'B' } else { b'A' };
    assert_eq!(sealer.open(&String::from_utf8(tampered).unwrap()), None);
    for junk in ["", "!!!", "AAAA", &"A".repeat(5000)] {
        assert_eq!(sealer.open(junk), None);
    }
}

// ---- with a key ----

#[tokio::test]
async fn without_credentials_everything_is_401() {
    let app = app(leak(Auth::new(settings(Some(KEY)))), REMOTE);
    for uri in ["/api/ping", "/api/auth/session", "/api/no/such/route"] {
        let reply = call(&app, request(Method::GET, uri, &[])).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(reply.json["error"], "not logged in");
        assert_eq!(
            reply.headers[header::WWW_AUTHENTICATE],
            "Bearer realm=\"flwrs\""
        );
    }
    // Even from loopback: a key means everyone logs in.
    let local = self::app(leak(Auth::new(settings(Some(KEY)))), LOCAL);
    let reply = call(
        &local,
        request(Method::GET, "/api/ping", &[("host", "localhost")]),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn bearer_key() {
    let app = app(leak(Auth::new(settings(Some(KEY)))), REMOTE);
    let auth = format!("Bearer {KEY}");
    let reply = call(
        &app,
        request(
            Method::POST,
            "/api/ping",
            &[("authorization", &auth), ("origin", "https://evil.example")],
        ),
    )
    .await;
    assert_eq!(
        reply.status,
        StatusCode::OK,
        "bearer requests skip the origin check"
    );
    assert_eq!(reply.json, "key");

    let reply = call(
        &app,
        request(
            Method::GET,
            "/api/auth/session",
            &[("authorization", &auth)],
        ),
    )
    .await;
    assert_eq!(reply.json["can_log_out"], false);
    assert!(reply.json["expire_time"].is_null());

    for wrong in [
        format!("Bearer {KEY}x"),
        "Basic abc".into(),
        "Bearer".into(),
    ] {
        let reply = call(
            &app,
            request(Method::GET, "/api/ping", &[("authorization", &wrong)]),
        )
        .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{wrong}");
    }
    // A wrong bearer isn't rescued by a valid cookie.
    let cookie = log_in(&app).await;
    let reply = call(
        &app,
        request(
            Method::GET,
            "/api/ping",
            &[("authorization", "Bearer nope"), ("cookie", &cookie)],
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn login_session_and_logout() {
    let app = app(leak(Auth::new(settings(Some(KEY)))), REMOTE);

    let reply = call(&app, login_request("wrong key, long enough", &[])).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert!(reply.headers.get(header::SET_COOKIE).is_none());

    let reply = call(&app, login_request(KEY, &[])).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json["provider"], "key");
    assert_eq!(reply.json["subject"], "admin");
    assert_eq!(reply.json["can_log_out"], true);
    assert!(reply.json["expire_time"].as_str().unwrap().ends_with('Z'));
    let set = reply.headers[header::SET_COOKIE].to_str().unwrap();
    assert!(set.starts_with("flwrs_session="), "{set}");
    for attribute in ["Path=/", "HttpOnly", "SameSite=Strict", "Max-Age=3600"] {
        assert!(set.contains(attribute), "{set}");
    }
    assert!(!set.contains("Secure"), "{set}");
    assert!(!set.contains(KEY), "the cookie never holds the key");
    assert_eq!(reply.headers[header::CACHE_CONTROL], "no-store");

    let cookie = set.split(';').next().unwrap();
    let reply = call(
        &app,
        request(Method::GET, "/api/auth/session", &[("cookie", cookie)]),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json["can_log_out"], true);
    let reply = call(
        &app,
        request(Method::POST, "/api/ping", &[("cookie", cookie)]),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);

    let reply = call(
        &app,
        request(Method::POST, "/api/auth/logout", &[("cookie", cookie)]),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    let set = reply.headers[header::SET_COOKIE].to_str().unwrap();
    assert!(
        set.starts_with("flwrs_session=;") && set.contains("Max-Age=0"),
        "{set}"
    );

    // Behind an HTTPS proxy the cookie is Secure.
    let reply = call(&app, login_request(KEY, &[("x-forwarded-proto", "https")])).await;
    assert!(
        reply.headers[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("; Secure")
    );
}

#[tokio::test]
async fn expired_or_foreign_sessions_are_refused() {
    let auth = leak(Auth::new(settings(Some(KEY))));
    let app = app(auth, REMOTE);
    let sealer = &auth.key.as_ref().unwrap().sealer;
    let now = chrono::Utc::now().timestamp();

    let expired = sealer
        .seal(&Claims::new("key", "admin", now - 100, now - 1))
        .unwrap();
    let foreign = Sealer::new(b"some other server's key")
        .seal(&Claims::new("key", "admin", now, now + 100))
        .unwrap();
    for value in [expired, foreign, "garbage".into()] {
        let cookie = format!("{COOKIE}={value}");
        let reply = call(
            &app,
            request(Method::GET, "/api/ping", &[("cookie", &cookie)]),
        )
        .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        assert!(
            reply.json["error"]
                .as_str()
                .unwrap()
                .contains("log in again")
        );
    }

    // A changed key logs everyone out.
    let cookie = log_in(&app).await;
    let rotated = self::app(
        leak(Auth::new(settings(Some("a brand new secret key")))),
        REMOTE,
    );
    let reply = call(
        &rotated,
        request(Method::GET, "/api/ping", &[("cookie", &cookie)]),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn cross_origin_changes_are_refused() {
    let app = app(leak(Auth::new(settings(Some(KEY)))), REMOTE);
    let cookie = log_in(&app).await;
    let post = |origin: &'static str| {
        request(
            Method::POST,
            "/api/ping",
            &[("cookie", &cookie), ("origin", origin)],
        )
    };

    let reply = call(&app, post("https://evil.example")).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(reply.json["error"], "cross-origin request refused");
    assert_eq!(call(&app, post("null")).await.status, StatusCode::FORBIDDEN);
    assert_eq!(
        call(&app, post("http://flwrs.lan:9999")).await.status,
        StatusCode::FORBIDDEN
    );

    // Its own origin, a listed one, or none (not a browser).
    assert_eq!(
        call(&app, post("http://flwrs.lan:8080")).await.status,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, post("https://UI.example.com")).await.status,
        StatusCode::OK
    );
    let reply = call(
        &app,
        request(Method::POST, "/api/ping", &[("cookie", &cookie)]),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    // Reads aren't checked.
    let reply = call(
        &app,
        request(
            Method::GET,
            "/api/ping",
            &[("cookie", &cookie), ("origin", "https://evil.example")],
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
}

#[tokio::test]
async fn failed_logins_are_limited_per_address() {
    let mut auth = Auth::new(settings(Some(KEY)));
    auth.limiter = limiter::LoginLimiter::with(2, Duration::from_secs(60));
    let auth = leak(auth);
    let app = app(auth, REMOTE);

    for _ in 0..2 {
        assert_eq!(
            call(&app, login_request("wrong key, long enough", &[]))
                .await
                .status,
            StatusCode::UNAUTHORIZED
        );
    }
    // Even the right key waits now.
    let reply = call(&app, login_request(KEY, &[])).await;
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
    let wait: u64 = reply.headers[header::RETRY_AFTER]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=60).contains(&wait));

    // Another address isn't affected; a success clears an address's count.
    let other = self::app(auth, "198.51.100.1:1");
    assert_eq!(
        call(&other, login_request("wrong key, long enough", &[]))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&other, login_request(KEY, &[])).await.status,
        StatusCode::OK
    );
    assert_eq!(
        call(&other, login_request("wrong key, long enough", &[]))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&other, login_request(KEY, &[])).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn providers() {
    let app = app(leak(Auth::new(settings(Some(KEY)))), REMOTE);
    let reply = call(&app, request(Method::GET, "/api/auth/providers", &[])).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json["loopback_only"], false);
    assert_eq!(reply.json["providers"][0]["id"], "key");
}

// ---- without a key ----

#[tokio::test]
async fn without_a_key_only_loopback_gets_in() {
    let auth = leak(Auth::new(settings(None)));

    let remote = app(auth, REMOTE);
    let reply = call(&remote, request(Method::GET, "/api/ping", &[])).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert!(reply.json["error"].as_str().unwrap().contains("loopback"));

    let local = app(auth, LOCAL);
    for host in ["localhost:8080", "127.0.0.1:8080", "[::1]:8080"] {
        let reply = call(
            &local,
            request(Method::GET, "/api/auth/session", &[("host", host)]),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{host}");
        assert_eq!(reply.json["provider"], "loopback");
        assert_eq!(reply.json["can_log_out"], false);
    }
    // A loopback peer reached by another name: DNS rebinding.
    let reply = call(&local, request(Method::GET, "/api/ping", &[])).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    // IPv4-mapped loopback counts.
    let mapped = app(auth, "[::ffff:127.0.0.1]:5000");
    let reply = call(
        &mapped,
        request(Method::GET, "/api/ping", &[("host", "localhost")]),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);

    // Cross-origin changes are refused here too (there's no cookie to protect,
    // but a page could still send requests to localhost).
    let reply = call(
        &local,
        request(
            Method::POST,
            "/api/ping",
            &[
                ("host", "localhost:8080"),
                ("origin", "https://evil.example"),
            ],
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    let reply = call(
        &local,
        request(
            Method::POST,
            "/api/ping",
            &[
                ("host", "localhost:8080"),
                ("origin", "http://localhost:8080"),
            ],
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);

    let reply = call(&local, login_request(KEY, &[])).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    let reply = call(&local, request(Method::GET, "/api/auth/providers", &[])).await;
    assert_eq!(reply.json["loopback_only"], true);
    assert_eq!(reply.json["providers"], serde_json::json!([]));
}
