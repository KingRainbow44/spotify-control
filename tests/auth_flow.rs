//! Token refresh and exchange, against a mock Spotify accounts server.

use spotify_control::auth::{exchange_code, now_unix, FileTokenProvider, Tokens};
use spotify_control::spotify::TokenProvider;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn write_tokens(dir: &tempfile::TempDir, expires_at: u64) -> std::path::PathBuf {
    let path = dir.path().join("tokens.json");
    Tokens {
        access_token: "stale-access".into(),
        refresh_token: "the-refresh-token".into(),
        expires_at,
    }
    .save(&path)
    .unwrap();
    path
}

fn provider(server: &MockServer, path: std::path::PathBuf) -> FileTokenProvider {
    FileTokenProvider::new("client-abc".into(), path).with_accounts_base(server.uri())
}

#[tokio::test]
async fn an_expired_token_is_refreshed_and_persisted() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let token_path = write_tokens(&dir, now_unix().saturating_sub(10));

    Mock::given(method("POST"))
        .and(path("/api/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=the-refresh-token"))
        .and(body_string_contains("client_id=client-abc"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "fresh-access",
            "token_type": "Bearer",
            "expires_in": 3600
        })))
        .expect(1)
        .mount(&server)
        .await;

    let p = provider(&server, token_path.clone());
    assert_eq!(p.access_token().await.unwrap(), "fresh-access");

    // The refreshed token must survive a restart.
    let on_disk = Tokens::load(&token_path).unwrap();
    assert_eq!(on_disk.access_token, "fresh-access");
    assert!(on_disk.expires_at > now_unix() + 3000);
}

#[tokio::test]
async fn a_refresh_without_a_new_refresh_token_keeps_the_old_one() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let token_path = write_tokens(&dir, now_unix().saturating_sub(10));

    // Spotify usually omits refresh_token on refresh; losing it would force a
    // full re-login, so it has to be carried forward.
    Mock::given(method("POST"))
        .and(path("/api/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "fresh-access",
            "token_type": "Bearer",
            "expires_in": 3600
        })))
        .mount(&server)
        .await;

    provider(&server, token_path.clone())
        .access_token()
        .await
        .unwrap();

    assert_eq!(
        Tokens::load(&token_path).unwrap().refresh_token,
        "the-refresh-token"
    );
}

#[tokio::test]
async fn a_rotated_refresh_token_replaces_the_stored_one() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let token_path = write_tokens(&dir, now_unix().saturating_sub(10));

    Mock::given(method("POST"))
        .and(path("/api/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "fresh-access",
            "token_type": "Bearer",
            "expires_in": 3600,
            "refresh_token": "rotated-refresh"
        })))
        .mount(&server)
        .await;

    provider(&server, token_path.clone())
        .access_token()
        .await
        .unwrap();

    assert_eq!(
        Tokens::load(&token_path).unwrap().refresh_token,
        "rotated-refresh"
    );
}

#[tokio::test]
async fn a_still_valid_token_never_touches_the_network() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let token_path = write_tokens(&dir, now_unix() + 3600);

    Mock::given(method("POST"))
        .and(path("/api/token"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let p = provider(&server, token_path);
    assert_eq!(p.access_token().await.unwrap(), "stale-access");
    // Second call should also be served from the in-memory cache.
    assert_eq!(p.access_token().await.unwrap(), "stale-access");
}

#[tokio::test]
async fn invalidate_forces_a_refresh_even_when_the_token_looks_fresh() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    // Nominally good for another hour. A token Spotify has rejected looks
    // exactly like this, so expiry alone must not decide whether to refresh.
    let token_path = write_tokens(&dir, now_unix() + 3600);

    Mock::given(method("POST"))
        .and(path("/api/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "fresh-access",
            "token_type": "Bearer",
            "expires_in": 3600
        })))
        .expect(1)
        .mount(&server)
        .await;

    let p = provider(&server, token_path);
    assert_eq!(p.access_token().await.unwrap(), "stale-access");

    p.invalidate().await;
    assert_eq!(
        p.access_token().await.unwrap(),
        "fresh-access",
        "invalidate must force a refresh, not replay the rejected token"
    );
    // And the refreshed token is cached rather than refreshed again.
    assert_eq!(p.access_token().await.unwrap(), "fresh-access");
}

#[tokio::test]
async fn a_forced_refresh_stays_pending_until_it_succeeds() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let token_path = write_tokens(&dir, now_unix() + 3600);

    // The accounts server is down; the rejected token must not come back.
    Mock::given(method("POST"))
        .and(path("/api/token"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let p = provider(&server, token_path);
    p.invalidate().await;

    assert!(p.access_token().await.is_err());
    assert!(
        p.access_token().await.is_err(),
        "a failed refresh must not clear the force flag"
    );
}

#[tokio::test]
async fn a_token_expiring_within_the_skew_window_refreshes_early() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    // Valid for another 30s — inside the 60s safety margin, so it must refresh
    // rather than risk expiring mid-request.
    let token_path = write_tokens(&dir, now_unix() + 30);

    Mock::given(method("POST"))
        .and(path("/api/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "fresh-access",
            "token_type": "Bearer",
            "expires_in": 3600
        })))
        .expect(1)
        .mount(&server)
        .await;

    assert_eq!(
        provider(&server, token_path).access_token().await.unwrap(),
        "fresh-access"
    );
}

#[tokio::test]
async fn a_revoked_refresh_token_produces_an_actionable_error() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let token_path = write_tokens(&dir, now_unix().saturating_sub(10));

    Mock::given(method("POST"))
        .and(path("/api/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": "invalid_grant",
            "error_description": "Refresh token revoked"
        })))
        .mount(&server)
        .await;

    let err = provider(&server, token_path)
        .access_token()
        .await
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("refresh"), "got: {msg}");
    assert!(msg.contains("invalid_grant"), "got: {msg}");
}

#[tokio::test]
async fn missing_tokens_tell_the_user_to_log_in() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();

    let err = provider(&server, dir.path().join("absent.json"))
        .access_token()
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("login"), "got: {err:#}");
}

#[tokio::test]
async fn code_exchange_sends_the_pkce_verifier() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/api/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .and(body_string_contains("code=auth-code-123"))
        .and(body_string_contains("code_verifier=the-verifier"))
        .and(body_string_contains("client_id=client-abc"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "first-access",
            "token_type": "Bearer",
            "expires_in": 3600,
            "refresh_token": "first-refresh"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let tokens = exchange_code(
        &server.uri(),
        "client-abc",
        "auth-code-123",
        "http://127.0.0.1:8888/callback",
        "the-verifier",
    )
    .await
    .unwrap();

    assert_eq!(tokens.access_token, "first-access");
    assert_eq!(tokens.refresh_token, "first-refresh");
}
