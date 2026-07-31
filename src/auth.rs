use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::Rng as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const ACCOUNTS_BASE: &str = "https://accounts.spotify.com";

/// Refresh this far before actual expiry so an in-flight request can't race it.
const EXPIRY_SKEW: u64 = 60;

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds.
    pub expires_at: u64,
}

impl Tokens {
    pub fn is_expired(&self, now: u64) -> bool {
        now + EXPIRY_SKEW >= self.expires_at
    }

    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("not logged in (no tokens at {})", path.display()))?;
        Ok(serde_json::from_str(&raw)?)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        restrict_permissions(path);
        Ok(())
    }
}

/// Tokens are bearer credentials; keep them owner-readable where the OS lets us.
#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {
    // Windows inherits the user-profile ACL, which is already owner-only.
}

/// Raw token response from Spotify's `/api/token`.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
    /// Absent on refresh unless Spotify chooses to rotate it.
    refresh_token: Option<String>,
}

impl TokenResponse {
    fn into_tokens(self, previous_refresh: Option<&str>, now: u64) -> Result<Tokens> {
        let refresh_token = self
            .refresh_token
            .or_else(|| previous_refresh.map(str::to_string))
            .context("Spotify returned no refresh token and none was cached")?;
        Ok(Tokens {
            access_token: self.access_token,
            refresh_token,
            expires_at: now + self.expires_in,
        })
    }
}

/// A PKCE verifier/challenge pair (RFC 7636).
#[derive(Debug, Clone)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        // 32 random bytes -> 43 base64url chars, the RFC's minimum verifier length.
        let verifier = URL_SAFE_NO_PAD.encode(bytes);
        Self::from_verifier(verifier)
    }

    pub fn from_verifier(verifier: String) -> Self {
        let challenge = challenge_for(&verifier);
        Self { verifier, challenge }
    }
}

/// S256 challenge: base64url(sha256(ascii(verifier))), no padding.
pub fn challenge_for(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

pub fn random_state() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn build_authorize_url(
    accounts_base: &str,
    client_id: &str,
    redirect_uri: &str,
    scopes: &str,
    challenge: &str,
    state: &str,
) -> Result<String> {
    let url = reqwest::Url::parse_with_params(
        &format!("{}/authorize", accounts_base.trim_end_matches('/')),
        &[
            ("response_type", "code"),
            ("client_id", client_id),
            ("redirect_uri", redirect_uri),
            ("scope", scopes),
            ("code_challenge_method", "S256"),
            ("code_challenge", challenge),
            ("state", state),
        ],
    )?;
    Ok(url.to_string())
}

/// What the loopback redirect handed back.
#[derive(Debug, PartialEq)]
pub struct Callback {
    pub code: String,
    pub state: String,
}

/// Parse the request target of the browser's callback, e.g.
/// `GET /callback?code=abc&state=xyz HTTP/1.1`.
pub fn parse_callback_request_line(line: &str) -> Result<Callback> {
    let target = line
        .split_whitespace()
        .nth(1)
        .context("malformed HTTP request line from the browser")?;

    // Relative target; the base is only there to satisfy the URL parser.
    let url = reqwest::Url::parse("http://127.0.0.1")?.join(target)?;

    let mut code = None;
    let mut state = None;
    let mut error = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            "error" => error = Some(v.into_owned()),
            _ => {}
        }
    }

    if let Some(e) = error {
        bail!("Spotify denied the authorization request: {e}");
    }
    Ok(Callback {
        code: code.context("callback had no ?code parameter")?,
        state: state.context("callback had no ?state parameter")?,
    })
}

const SUCCESS_PAGE: &str = "<!doctype html><meta charset=utf-8><title>spotify-control</title>\
<body style=\"font-family:system-ui;display:grid;place-items:center;height:100vh;margin:0;background:#121212;color:#fff\">\
<div style=\"text-align:center\"><h1 style=\"color:#1db954\">Connected</h1>\
<p>You can close this tab and return to your terminal.</p></div>";

/// The failure page echoes a `?error=` value straight from the request, and it
/// renders on the 127.0.0.1 origin — where cookies are shared across every local
/// port — so it has to be escaped.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Did this request actually come from the OAuth redirect, as opposed to a
/// favicon fetch, a browser preconnect, or something else probing the port?
fn carries_oauth_result(line: &str) -> bool {
    let Some(target) = line.split_whitespace().nth(1) else {
        return false;
    };
    let Ok(url) = reqwest::Url::parse("http://127.0.0.1").and_then(|b| b.join(target)) else {
        return false;
    };
    url.query_pairs().any(|(k, _)| k == "code" || k == "error")
}

/// Block until the browser hits the loopback redirect, then return the callback.
fn wait_for_callback(listener: &TcpListener, timeout: Duration) -> Result<Callback> {
    let deadline = std::time::Instant::now() + timeout;
    listener.set_nonblocking(true)?;

    loop {
        if std::time::Instant::now() > deadline {
            bail!("timed out waiting for the Spotify authorization redirect");
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                // Windows hands back an accepted socket that inherits the
                // listener's non-blocking flag, which makes the read timeout
                // below a no-op and fails the read outright if the request
                // bytes haven't landed yet.
                stream.set_nonblocking(false)?;
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;

                let mut line = String::new();
                if BufReader::new(&stream).read_line(&mut line).is_err() {
                    continue;
                }

                let result = parse_callback_request_line(&line);
                // Answer the browser either way so the user isn't left on a
                // spinner wondering what happened.
                let (status, page) = match &result {
                    Ok(_) => ("200 OK", SUCCESS_PAGE.to_string()),
                    Err(e) => (
                        "400 Bad Request",
                        format!(
                            "<h1>Authorization failed</h1><p>{}</p>",
                            html_escape(&e.to_string())
                        ),
                    ),
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
                    page.len()
                );
                let _ = stream.flush();

                // Only a real callback ends the wait. Anything else that
                // reached the port would otherwise kill the login while the
                // browser's genuine redirect is still queued behind it.
                if result.is_ok() || carries_oauth_result(&line) {
                    return result;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Run the full PKCE authorization-code flow and persist the resulting tokens.
pub async fn login(
    accounts_base: &str,
    client_id: &str,
    redirect_port: u16,
    scopes: &str,
    token_path: &Path,
) -> Result<Tokens> {
    let pkce = Pkce::generate();
    let state = random_state();
    let redirect_uri = format!("http://127.0.0.1:{redirect_port}/callback");

    // Bind before opening the browser so we can't miss a fast redirect.
    let listener = TcpListener::bind(("127.0.0.1", redirect_port)).with_context(|| {
        format!("could not bind 127.0.0.1:{redirect_port} — is another instance running?")
    })?;

    let auth_url = build_authorize_url(
        accounts_base,
        client_id,
        &redirect_uri,
        scopes,
        &pkce.challenge,
        &state,
    )?;

    println!("Opening your browser to authorize with Spotify...");
    println!("If it doesn't open, visit:\n  {auth_url}\n");
    let _ = webbrowser::open(&auth_url);

    let callback = tokio::task::spawn_blocking(move || {
        wait_for_callback(&listener, Duration::from_secs(300))
    })
    .await??;

    if callback.state != state {
        bail!("authorization state mismatch — possible CSRF, aborting");
    }

    let tokens = exchange_code(
        accounts_base,
        client_id,
        &callback.code,
        &redirect_uri,
        &pkce.verifier,
    )
    .await?;

    tokens.save(token_path)?;
    Ok(tokens)
}

pub async fn exchange_code(
    accounts_base: &str,
    client_id: &str,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<Tokens> {
    let resp = reqwest::Client::new()
        .post(format!("{}/api/token", accounts_base.trim_end_matches('/')))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", client_id),
            ("code_verifier", verifier),
        ])
        .send()
        .await?;

    parse_token_response(resp, None).await
}

pub async fn refresh_tokens(
    accounts_base: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<Tokens> {
    let resp = reqwest::Client::new()
        .post(format!("{}/api/token", accounts_base.trim_end_matches('/')))
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
        ])
        .send()
        .await?;

    parse_token_response(resp, Some(refresh_token)).await
}

async fn parse_token_response(
    resp: reqwest::Response,
    previous_refresh: Option<&str>,
) -> Result<Tokens> {
    let status = resp.status();
    let body = resp.text().await?;

    if !status.is_success() {
        bail!("Spotify token endpoint returned {status}: {body}");
    }
    let parsed: TokenResponse =
        serde_json::from_str(&body).with_context(|| format!("unexpected token response: {body}"))?;
    parsed.into_tokens(previous_refresh, now_unix())
}

/// Reads tokens from disk and refreshes them on demand.
pub struct FileTokenProvider {
    accounts_base: String,
    client_id: String,
    token_path: PathBuf,
    cached: Mutex<Option<Tokens>>,
    /// Set by `invalidate`. A token that Spotify rejected still looks perfectly
    /// fresh by its `expires_at`, so expiry alone can't decide when to refresh.
    force_refresh: AtomicBool,
}

impl FileTokenProvider {
    pub fn new(client_id: String, token_path: PathBuf) -> Self {
        Self {
            accounts_base: ACCOUNTS_BASE.to_string(),
            client_id,
            token_path,
            cached: Mutex::new(None),
            force_refresh: AtomicBool::new(false),
        }
    }

    pub fn with_accounts_base(mut self, base: String) -> Self {
        self.accounts_base = base;
        self
    }

    fn cached_valid(&self) -> Option<Tokens> {
        let guard = self.cached.lock().ok()?;
        guard.clone().filter(|t| !t.is_expired(now_unix()))
    }
}

impl crate::spotify::TokenProvider for FileTokenProvider {
    async fn access_token(&self) -> Result<String> {
        let forced = self.force_refresh.load(std::sync::atomic::Ordering::SeqCst);
        if !forced && let Some(t) = self.cached_valid() {
            return Ok(t.access_token);
        }

        let stored = Tokens::load(&self.token_path)
            .context("run `spotify-control login` to authorize this machine")?;

        let fresh = if forced || stored.is_expired(now_unix()) {
            tracing::debug!(forced, "refreshing the access token");
            let t = refresh_tokens(&self.accounts_base, &self.client_id, &stored.refresh_token)
                .await
                .context("could not refresh the Spotify access token")?;
            t.save(&self.token_path)?;
            // Stays set until a refresh actually lands, so a transient network
            // failure can't drop us back to serving the rejected token.
            self.force_refresh
                .store(false, std::sync::atomic::Ordering::SeqCst);
            t
        } else {
            stored
        };

        let access = fresh.access_token.clone();
        if let Ok(mut guard) = self.cached.lock() {
            *guard = Some(fresh);
        }
        Ok(access)
    }

    async fn invalidate(&self) {
        self.force_refresh
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut guard) = self.cached.lock() {
            *guard = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_matches_rfc7636_test_vector() {
        // RFC 7636 Appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            challenge_for(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn generated_verifier_meets_rfc_length_rules() {
        let pkce = Pkce::generate();
        assert!(
            (43..=128).contains(&pkce.verifier.len()),
            "verifier length {} out of RFC range",
            pkce.verifier.len()
        );
        // Must be unreserved characters only — no padding or +/ from standard base64.
        assert!(pkce
            .verifier
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c)));
    }

    #[test]
    fn generated_verifiers_are_unique() {
        assert_ne!(Pkce::generate().verifier, Pkce::generate().verifier);
        assert_ne!(random_state(), random_state());
    }

    #[test]
    fn authorize_url_carries_every_required_param() {
        let url = build_authorize_url(
            "https://accounts.spotify.com",
            "client123",
            "http://127.0.0.1:8888/callback",
            "user-read-playback-state user-modify-playback-state",
            "chal",
            "state42",
        )
        .unwrap();

        assert!(url.starts_with("https://accounts.spotify.com/authorize?"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=client123"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("code_challenge=chal"));
        assert!(url.contains("state=state42"));
        // Redirect URI and scopes must be percent-encoded.
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A8888%2Fcallback"));
        assert!(url.contains("user-read-playback-state+user-modify-playback-state"));
    }

    #[test]
    fn parses_a_successful_callback() {
        let cb =
            parse_callback_request_line("GET /callback?code=abc123&state=xyz HTTP/1.1").unwrap();
        assert_eq!(
            cb,
            Callback {
                code: "abc123".into(),
                state: "xyz".into()
            }
        );
    }

    #[test]
    fn percent_decodes_callback_values() {
        let cb = parse_callback_request_line("GET /callback?code=a%2Bb%2Fc&state=s HTTP/1.1").unwrap();
        assert_eq!(cb.code, "a+b/c");
    }

    #[test]
    fn surfaces_user_denial() {
        let err = parse_callback_request_line("GET /callback?error=access_denied&state=x HTTP/1.1")
            .unwrap_err();
        assert!(format!("{err}").contains("access_denied"));
    }

    #[test]
    fn rejects_callback_missing_code_or_state() {
        assert!(parse_callback_request_line("GET /callback?state=x HTTP/1.1").is_err());
        assert!(parse_callback_request_line("GET /callback?code=x HTTP/1.1").is_err());
        assert!(parse_callback_request_line("garbage").is_err());
    }

    #[test]
    fn failure_page_escapes_the_reflected_error() {
        // `?error=` is attacker-controlled and the page renders on 127.0.0.1,
        // an origin whose cookies are shared with every other local port.
        let err =
            parse_callback_request_line("GET /callback?error=%3Cscript%3Ex%3C/script%3E HTTP/1.1")
                .unwrap_err();
        let page = html_escape(&err.to_string());
        assert!(!page.contains("<script>"), "got: {page}");
        assert!(page.contains("&lt;script&gt;"), "got: {page}");
    }

    #[test]
    fn only_real_callbacks_end_the_wait() {
        assert!(carries_oauth_result(
            "GET /callback?code=abc&state=xyz HTTP/1.1"
        ));
        assert!(carries_oauth_result(
            "GET /callback?error=access_denied HTTP/1.1"
        ));
        // Stray traffic on the port must not be mistaken for the redirect.
        assert!(!carries_oauth_result("GET /favicon.ico HTTP/1.1"));
        assert!(!carries_oauth_result("GET / HTTP/1.1"));
        assert!(!carries_oauth_result(""));
    }

    #[test]
    fn callback_survives_a_connection_that_never_sends_anything() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            // A browser preconnect: opens a socket, says nothing, goes away.
            let idle = std::net::TcpStream::connect(addr).unwrap();
            std::thread::sleep(Duration::from_millis(200));
            drop(idle);

            let mut real = std::net::TcpStream::connect(addr).unwrap();
            let _ = real.write_all(b"GET /callback?code=abc&state=xyz HTTP/1.1\r\n\r\n");
            let _ = real.flush();
            std::thread::sleep(Duration::from_millis(200));
        });

        let cb = wait_for_callback(&listener, Duration::from_secs(20)).unwrap();
        assert_eq!(cb.code, "abc");
    }

    #[test]
    fn callback_is_read_when_the_request_lands_after_the_connection() {
        // On Windows the accepted socket inherits the listener's non-blocking
        // flag, so without resetting it this read fails instantly instead of
        // waiting for the browser's request bytes.
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            let mut s = std::net::TcpStream::connect(addr).unwrap();
            std::thread::sleep(Duration::from_millis(400));
            let _ = s.write_all(b"GET /callback?code=late&state=s HTTP/1.1\r\n\r\n");
            let _ = s.flush();
            std::thread::sleep(Duration::from_millis(200));
        });

        let cb = wait_for_callback(&listener, Duration::from_secs(20)).unwrap();
        assert_eq!(cb.code, "late");
    }

    #[test]
    fn expiry_accounts_for_skew() {
        let t = Tokens {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: 1000,
        };
        assert!(!t.is_expired(900), "should still be valid well before expiry");
        // Inside the skew window it must already count as expired.
        assert!(t.is_expired(1000 - EXPIRY_SKEW));
        assert!(t.is_expired(1200));
    }

    #[test]
    fn refresh_response_keeps_previous_refresh_token_when_omitted() {
        let resp = TokenResponse {
            access_token: "new-access".into(),
            expires_in: 3600,
            refresh_token: None,
        };
        let tokens = resp.into_tokens(Some("old-refresh"), 1_000).unwrap();
        assert_eq!(tokens.refresh_token, "old-refresh");
        assert_eq!(tokens.expires_at, 4_600);
    }

    #[test]
    fn refresh_response_prefers_a_rotated_refresh_token() {
        let resp = TokenResponse {
            access_token: "a".into(),
            expires_in: 10,
            refresh_token: Some("rotated".into()),
        };
        let tokens = resp.into_tokens(Some("old"), 0).unwrap();
        assert_eq!(tokens.refresh_token, "rotated");
    }

    #[test]
    fn errors_when_no_refresh_token_available_at_all() {
        let resp = TokenResponse {
            access_token: "a".into(),
            expires_in: 10,
            refresh_token: None,
        };
        assert!(resp.into_tokens(None, 0).is_err());
    }

    #[test]
    fn tokens_roundtrip_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tokens.json");
        let t = Tokens {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: 123,
        };
        t.save(&path).unwrap();
        assert_eq!(Tokens::load(&path).unwrap(), t);
    }
}
