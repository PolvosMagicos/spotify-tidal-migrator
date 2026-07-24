use std::{
    collections::BTreeSet,
    env, fs,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{RngExt, distr::Alphanumeric};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use url::Url;

const TIDAL_AUTHORIZE_URL: &str = "https://login.tidal.com/authorize";
const TIDAL_TOKEN_URL: &str = "https://auth.tidal.com/v1/oauth2/token";
const TIDAL_USER_TOKEN_PATH: &str = "data/tidal-user-token.json";
const DEFAULT_REDIRECT_URI: &str = "http://127.0.0.1:8989/callback/tidal";
const TOKEN_EXPIRY_MARGIN_SECONDS: u64 = 60;

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ScopeResponse {
    Text(String),
    List(Vec<String>),
}

impl Default for ScopeResponse {
    fn default() -> Self {
        Self::Text(String::new())
    }
}

impl ScopeResponse {
    fn into_text(self) -> String {
        match self {
            Self::Text(value) => value,
            Self::List(values) => values.join(" "),
        }
    }
}

#[derive(Debug, Deserialize)]
struct TidalUserTokenResponse {
    access_token: String,
    token_type: String,
    expires_in: u64,

    #[serde(default)]
    scope: ScopeResponse,

    #[serde(default)]
    refresh_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredTidalUserToken {
    access_token: String,
    token_type: String,
    expires_in: u64,
    scope: String,
    refresh_token: Option<String>,
    obtained_at: u64,
}

/// Authenticated user-context client kept separate from the catalog client's
/// client-credentials token. Mutation methods are intentionally absent until
/// every required third-party scope is officially grantable.
pub struct TidalUserClient {
    _client: Client,
    access_token: String,
    country_code: String,
    granted_scopes: BTreeSet<String>,
    expires_at: u64,
}

impl TidalUserClient {
    pub async fn from_env() -> Result<Self> {
        let requested_scopes = requested_scopes_from_env()?;
        let client = http_client()?;
        let stored = load_token()?;
        let now = current_unix_timestamp()?;
        let stored = if token_is_expired_at(&stored, now) {
            refresh_token_at(&client, TIDAL_TOKEN_URL, stored, now).await?
        } else {
            stored
        };

        let granted_scopes = parse_scopes(&stored.scope);
        validate_granted_scopes(&requested_scopes, &granted_scopes)?;
        let country_code = country_code_from_env()?;
        let expires_at = stored.obtained_at.saturating_add(stored.expires_in);

        Ok(Self {
            _client: client,
            access_token: stored.access_token,
            country_code,
            granted_scopes,
            expires_at,
        })
    }

    pub fn country_code(&self) -> &str {
        &self.country_code
    }

    pub fn granted_scopes(&self) -> &BTreeSet<String> {
        &self.granted_scopes
    }

    pub fn expires_at(&self) -> u64 {
        self.expires_at
    }

    pub fn is_authenticated(&self) -> bool {
        !self.access_token.is_empty()
    }
}

pub async fn authenticate() -> Result<()> {
    let client_id = env::var("TIDAL_CLIENT_ID").context("TIDAL_CLIENT_ID is missing from .env")?;
    let requested_scopes = requested_scopes_from_env()?;
    let scopes = requested_scopes
        .iter()
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    let redirect_uri =
        env::var("TIDAL_REDIRECT_URI").unwrap_or_else(|_| DEFAULT_REDIRECT_URI.to_owned());
    let redirect_url = Url::parse(&redirect_uri).context("Invalid TIDAL_REDIRECT_URI")?;
    validate_redirect_url(&redirect_url)?;

    let host = redirect_url
        .host_str()
        .context("The TIDAL redirect URI has no host")?;
    let port = redirect_url
        .port()
        .context("The TIDAL redirect URI must contain an explicit port")?;

    // Listen before opening the browser so an immediate callback cannot race
    // the local server startup.
    let listener = TcpListener::bind((host, port))
        .await
        .with_context(|| format!("Could not listen on {host}:{port}"))?;

    let verifier = generate_pkce_verifier();
    let challenge = pkce_challenge(&verifier);
    let expected_state = generate_oauth_state();
    let mut authorization_url = Url::parse(TIDAL_AUTHORIZE_URL)?;
    authorization_url
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("scope", &scopes)
        .append_pair("code_challenge_method", "S256")
        .append_pair("code_challenge", &challenge)
        .append_pair("state", &expected_state);

    println!("Opening TIDAL authorization in your browser...");
    if let Err(error) = open::that(authorization_url.as_str()) {
        eprintln!("Could not open the browser automatically: {error}");
        println!("\nOpen this URL manually:\n\n{authorization_url}\n");
    }

    let authorization_code = wait_for_callback(listener, &redirect_url, &expected_state).await?;
    let client = http_client()?;
    let response = exchange_authorization_code_at(
        &client,
        TIDAL_TOKEN_URL,
        &client_id,
        &authorization_code,
        &redirect_uri,
        &verifier,
    )
    .await?;
    let granted_scope_text = response.scope.into_text();
    let granted_scopes = parse_scopes(&granted_scope_text);
    validate_granted_scopes(&requested_scopes, &granted_scopes)?;

    let token = StoredTidalUserToken {
        access_token: response.access_token,
        token_type: response.token_type,
        expires_in: response.expires_in,
        scope: granted_scope_text,
        refresh_token: response.refresh_token,
        obtained_at: current_unix_timestamp()?,
    };
    save_token(&token)?;

    // Load through the reusable path immediately, ensuring the persisted token
    // and configured scopes are valid without exposing any credential value.
    let user_client = TidalUserClient::from_env().await?;
    debug_assert!(user_client.is_authenticated());

    println!("TIDAL user authentication completed.");
    println!("Token saved to {TIDAL_USER_TOKEN_PATH}");
    println!(
        "Granted scopes: {}",
        user_client
            .granted_scopes()
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(" ")
    );
    println!(
        "Token expires at Unix timestamp: {}",
        user_client.expires_at()
    );
    println!("Country: {}", user_client.country_code());
    println!(
        "User identity verification is not attempted: the official /users/me operation currently also requires the INTERNAL r_usr scope."
    );

    Ok(())
}

fn http_client() -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("Could not create the TIDAL user HTTP client")
}

fn requested_scopes_from_env() -> Result<BTreeSet<String>> {
    let value = env::var("TIDAL_SCOPES").unwrap_or_default();
    let scopes = parse_scopes(&value);

    if scopes.is_empty() {
        bail!(
            "TIDAL_SCOPES is empty. Enable the required user-resource scopes in the TIDAL developer dashboard, then set TIDAL_SCOPES in .env to the exact space-separated scope names shown by the official API reference."
        );
    }

    Ok(scopes)
}

fn country_code_from_env() -> Result<String> {
    let country_code = env::var("TIDAL_COUNTRY_CODE").unwrap_or_else(|_| "PE".to_owned());
    let country_code = country_code.trim().to_ascii_uppercase();

    if country_code.len() != 2
        || !country_code
            .chars()
            .all(|character| character.is_ascii_alphabetic())
    {
        bail!("TIDAL_COUNTRY_CODE must be a two-letter country code");
    }

    Ok(country_code)
}

fn parse_scopes(value: &str) -> BTreeSet<String> {
    value
        .split_whitespace()
        .filter(|scope| !scope.is_empty())
        .map(str::to_owned)
        .collect()
}

fn validate_granted_scopes(requested: &BTreeSet<String>, granted: &BTreeSet<String>) -> Result<()> {
    let missing: Vec<_> = requested.difference(granted).cloned().collect();

    if !missing.is_empty() {
        bail!(
            "TIDAL did not grant all configured scopes. Missing: {}. Check the scopes enabled for this app in the TIDAL developer dashboard.",
            missing.join(" ")
        );
    }

    Ok(())
}

fn generate_pkce_verifier() -> String {
    random_alphanumeric(64)
}

fn generate_oauth_state() -> String {
    random_alphanumeric(48)
}

fn random_alphanumeric(length: usize) -> String {
    rand::rng()
        .sample_iter(&Alphanumeric)
        .take(length)
        .map(char::from)
        .collect()
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn validate_oauth_state(expected: &str, returned: Option<&str>) -> Result<()> {
    let returned = returned.context("TIDAL callback did not contain state")?;
    if returned != expected {
        bail!("OAuth state validation failed");
    }
    Ok(())
}

fn validate_redirect_url(redirect_url: &Url) -> Result<()> {
    if redirect_url.scheme() != "http" {
        bail!("The local TIDAL callback must use http");
    }
    if redirect_url.host_str() != Some("127.0.0.1") {
        bail!("Use 127.0.0.1 rather than localhost for TIDAL_REDIRECT_URI");
    }
    if redirect_url.port().is_none() {
        bail!("TIDAL_REDIRECT_URI must contain an explicit port");
    }
    if redirect_url.path() != "/callback/tidal" {
        bail!(
            "Expected TIDAL callback path /callback/tidal, found {}",
            redirect_url.path()
        );
    }
    Ok(())
}

async fn wait_for_callback(
    listener: TcpListener,
    redirect_url: &Url,
    expected_state: &str,
) -> Result<String> {
    loop {
        let (mut socket, _) = listener.accept().await?;
        let mut buffer = vec![0_u8; 16 * 1024];
        let bytes_read = socket.read(&mut buffer).await?;
        if bytes_read == 0 {
            continue;
        }

        let request = String::from_utf8_lossy(&buffer[..bytes_read]);
        let Some(request_target) = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
        else {
            send_browser_response(&mut socket, "400 Bad Request", "Invalid callback.").await?;
            continue;
        };
        let callback_url = Url::parse(&format!(
            "http://127.0.0.1:{}{}",
            redirect_url.port().context("Missing callback port")?,
            request_target
        ))?;

        if callback_url.path() != redirect_url.path() {
            send_browser_response(&mut socket, "404 Not Found", "Not found.").await?;
            continue;
        }

        let parameters: std::collections::HashMap<String, String> =
            callback_url.query_pairs().into_owned().collect();

        if let Some(error) = parameters.get("error") {
            send_browser_response(
                &mut socket,
                "400 Bad Request",
                "TIDAL authorization was denied. You may close this tab.",
            )
            .await?;
            bail!("TIDAL authorization failed: {error}");
        }

        if let Err(error) =
            validate_oauth_state(expected_state, parameters.get("state").map(String::as_str))
        {
            send_browser_response(
                &mut socket,
                "400 Bad Request",
                "Invalid authorization state. You may close this tab.",
            )
            .await?;
            return Err(error);
        }

        let code = parameters
            .get("code")
            .context("TIDAL callback did not contain an authorization code")?
            .to_owned();
        send_browser_response(
            &mut socket,
            "200 OK",
            "TIDAL authorization completed. You may close this tab.",
        )
        .await?;
        return Ok(code);
    }
}

async fn send_browser_response(
    socket: &mut tokio::net::TcpStream,
    status: &str,
    message: &str,
) -> Result<()> {
    let body = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>TIDAL authorization</title></head><body><h1>{message}</h1></body></html>"
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).await?;
    socket.shutdown().await?;
    Ok(())
}

async fn exchange_authorization_code_at(
    client: &Client,
    token_url: &str,
    client_id: &str,
    authorization_code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<TidalUserTokenResponse> {
    let response = client
        .post(token_url)
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("code", authorization_code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .context("Could not contact TIDAL's user token endpoint")?;
    let status = response.status();
    if !status.is_success() {
        // OAuth error bodies are intentionally omitted because an
        // authorization server may echo submitted credential material.
        bail!("TIDAL authorization-code exchange failed with HTTP {status}");
    }
    response
        .json()
        .await
        .context("TIDAL returned an invalid user token response")
}

async fn refresh_token_at(
    client: &Client,
    token_url: &str,
    stored: StoredTidalUserToken,
    now: u64,
) -> Result<StoredTidalUserToken> {
    let refresh_token = stored
        .refresh_token
        .as_deref()
        .context("No TIDAL refresh token is stored; run `cargo run -- auth tidal` again")?;
    let refreshed = request_refresh_token_at(client, token_url, refresh_token).await?;
    let merged = merge_refreshed_token(stored, refreshed, now);
    save_token(&merged)?;
    Ok(merged)
}

async fn request_refresh_token_at(
    client: &Client,
    token_url: &str,
    refresh_token: &str,
) -> Result<TidalUserTokenResponse> {
    let response = client
        .post(token_url)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await
        .context("Could not contact TIDAL's refresh-token endpoint")?;
    let status = response.status();
    if !status.is_success() {
        bail!("TIDAL token refresh failed with HTTP {status}; run `cargo run -- auth tidal` again");
    }
    response
        .json()
        .await
        .context("TIDAL returned an invalid refresh-token response")
}

fn merge_refreshed_token(
    stored: StoredTidalUserToken,
    refreshed: TidalUserTokenResponse,
    obtained_at: u64,
) -> StoredTidalUserToken {
    let refreshed_scope = refreshed.scope.into_text();
    StoredTidalUserToken {
        access_token: refreshed.access_token,
        token_type: refreshed.token_type,
        expires_in: refreshed.expires_in,
        scope: if refreshed_scope.trim().is_empty() {
            stored.scope
        } else {
            refreshed_scope
        },
        refresh_token: refreshed.refresh_token.or(stored.refresh_token),
        obtained_at,
    }
}

fn token_is_expired_at(token: &StoredTidalUserToken, now: u64) -> bool {
    now >= token
        .obtained_at
        .saturating_add(token.expires_in)
        .saturating_sub(TOKEN_EXPIRY_MARGIN_SECONDS)
}

fn save_token(token: &StoredTidalUserToken) -> Result<()> {
    let path = Path::new(TIDAL_USER_TOKEN_PATH);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_vec_pretty(token)?)
        .with_context(|| format!("Could not write {TIDAL_USER_TOKEN_PATH}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn load_token() -> Result<StoredTidalUserToken> {
    let bytes = fs::read(TIDAL_USER_TOKEN_PATH).with_context(|| {
        format!("Could not read {TIDAL_USER_TOKEN_PATH}; run `cargo run -- auth tidal` first")
    })?;
    serde_json::from_slice(&bytes).context("The stored TIDAL user token file is invalid")
}

fn current_unix_timestamp() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, sync::Arc};

    use axum::{Router, extract::State, http::StatusCode, routing::post};
    use tokio::sync::Mutex;

    use super::{
        ScopeResponse, StoredTidalUserToken, TidalUserTokenResponse,
        exchange_authorization_code_at, generate_pkce_verifier, merge_refreshed_token,
        parse_scopes, pkce_challenge, request_refresh_token_at, token_is_expired_at,
        validate_granted_scopes, validate_oauth_state,
    };

    #[test]
    fn generates_valid_pkce_verifier_and_rfc_challenge() {
        let verifier = generate_pkce_verifier();
        assert_eq!(verifier.len(), 64);
        assert!(
            verifier
                .chars()
                .all(|character| character.is_ascii_alphanumeric())
        );
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn validates_oauth_state() {
        assert!(validate_oauth_state("expected", Some("expected")).is_ok());
        assert!(validate_oauth_state("expected", Some("wrong")).is_err());
        assert!(validate_oauth_state("expected", None).is_err());
    }

    #[test]
    fn calculates_token_expiration_with_margin() {
        let token = stored_token();
        assert!(!token_is_expired_at(&token, 1_039));
        assert!(token_is_expired_at(&token, 1_040));
    }

    #[test]
    fn preserves_refresh_token_and_scope_when_omitted() {
        let merged = merge_refreshed_token(
            stored_token(),
            TidalUserTokenResponse {
                access_token: "new-access".to_owned(),
                token_type: "Bearer".to_owned(),
                expires_in: 3_600,
                scope: ScopeResponse::Text(String::new()),
                refresh_token: None,
            },
            2_000,
        );
        assert_eq!(merged.refresh_token.as_deref(), Some("old-refresh"));
        assert_eq!(merged.scope, "playlists.read playlists.write");
        assert_eq!(merged.obtained_at, 2_000);
    }

    #[test]
    fn parses_and_validates_scopes() {
        let parsed = parse_scopes("playlists.write  playlists.read playlists.write");
        assert_eq!(
            parsed,
            BTreeSet::from(["playlists.read".to_owned(), "playlists.write".to_owned()])
        );
        assert!(validate_granted_scopes(&parsed, &parsed).is_ok());
        assert!(
            validate_granted_scopes(&parsed, &BTreeSet::from(["playlists.read".to_owned()]))
                .is_err()
        );
    }

    #[tokio::test]
    async fn exchanges_authorization_code_with_mock_server() {
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let (url, task) = mock_token_server(
            bodies.clone(),
            r#"{"access_token":"access","token_type":"Bearer","expires_in":3600,"scope":"playlists.read","refresh_token":"refresh"}"#,
        )
        .await;
        let response = exchange_authorization_code_at(
            &reqwest::Client::new(),
            &url,
            "client-id",
            "synthetic-code",
            "http://127.0.0.1/callback/tidal",
            "synthetic-verifier",
        )
        .await
        .unwrap();
        assert_eq!(response.access_token, "access");
        let body = bodies.lock().await.join("");
        assert!(body.contains("grant_type=authorization_code"));
        assert!(body.contains("code_verifier=synthetic-verifier"));
        task.abort();
    }

    #[tokio::test]
    async fn refreshes_with_mock_server_and_preserves_refresh_token() {
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let (url, task) = mock_token_server(
            bodies.clone(),
            r#"{"access_token":"new-access","token_type":"Bearer","expires_in":3600,"scope":"playlists.read playlists.write"}"#,
        )
        .await;
        let response = request_refresh_token_at(&reqwest::Client::new(), &url, "old-refresh")
            .await
            .unwrap();
        let refreshed = merge_refreshed_token(stored_token(), response, 2_000);
        assert_eq!(refreshed.refresh_token.as_deref(), Some("old-refresh"));
        assert_eq!(refreshed.access_token, "new-access");
        assert!(
            bodies
                .lock()
                .await
                .join("")
                .contains("grant_type=refresh_token")
        );
        task.abort();
    }

    fn stored_token() -> StoredTidalUserToken {
        StoredTidalUserToken {
            access_token: "old-access".to_owned(),
            token_type: "Bearer".to_owned(),
            expires_in: 100,
            scope: "playlists.read playlists.write".to_owned(),
            refresh_token: Some("old-refresh".to_owned()),
            obtained_at: 1_000,
        }
    }

    async fn mock_token_server(
        bodies: Arc<Mutex<Vec<String>>>,
        response: &'static str,
    ) -> (String, tokio::task::JoinHandle<()>) {
        async fn handler(
            State((bodies, response)): State<(Arc<Mutex<Vec<String>>>, &'static str)>,
            body: String,
        ) -> (StatusCode, [(&'static str, &'static str); 1], &'static str) {
            bodies.lock().await.push(body);
            (
                StatusCode::OK,
                [("content-type", "application/json")],
                response,
            )
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/token", post(handler))
            .with_state((bodies, response));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}/token"), task)
    }
}
