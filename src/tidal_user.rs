use std::{
    collections::BTreeSet,
    env, fs, io,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::StreamExt;
use rand::{RngExt, distr::Alphanumeric};
use reqwest::{Client, Method, Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    time::sleep,
};
use url::Url;

const TIDAL_AUTHORIZE_URL: &str = "https://login.tidal.com/authorize";
const TIDAL_TOKEN_URL: &str = "https://auth.tidal.com/v1/oauth2/token";
const TIDAL_API_BASE_URL: &str = "https://openapi.tidal.com/v2/";
const TIDAL_USER_TOKEN_PATH: &str = "data/tidal-user-token.json";
const DEFAULT_REDIRECT_URI: &str = "http://127.0.0.1:8989/callback/tidal";
const TOKEN_EXPIRY_MARGIN_SECONDS: u64 = 60;
const JSON_API_MEDIA_TYPE: &str = "application/vnd.api+json";
const MAX_RESPONSE_BODY_BYTES: usize = 2 * 1024 * 1024;
const MAX_ERROR_DETAIL_CHARS: usize = 1_000;
const MAX_REQUEST_ATTEMPTS: usize = 4;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedPlaylist {
    pub id: String,
    pub name: Option<String>,
    pub access_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistItemIdentifier {
    pub id: String,
    pub resource_type: String,
}

/// Authenticated user-context client kept separate from the catalog client's
/// client-credentials token.
pub struct TidalUserClient {
    client: Client,
    token: StoredTidalUserToken,
    client_id: String,
    country_code: String,
    granted_scopes: BTreeSet<String>,
    expires_at: u64,
    api_base_url: Url,
    token_url: String,
    persist_refreshed_token: bool,
    retry_base_delay: Duration,
}

impl TidalUserClient {
    pub async fn from_env() -> Result<Self> {
        let requested_scopes = requested_scopes_from_env()?;
        let client_id =
            env::var("TIDAL_CLIENT_ID").context("TIDAL_CLIENT_ID is missing from .env")?;
        let client = http_client()?;
        let stored = load_token()?;
        let now = current_unix_timestamp()?;
        let stored = if token_is_expired_at(&stored, now) {
            refresh_token_at(&client, TIDAL_TOKEN_URL, &client_id, stored, now).await?
        } else {
            stored
        };

        let granted_scopes = parse_scopes(&stored.scope);
        validate_granted_scopes(&requested_scopes, &granted_scopes)?;
        let country_code = country_code_from_env()?;
        let expires_at = stored.obtained_at.saturating_add(stored.expires_in);

        Ok(Self {
            client,
            token: stored,
            client_id,
            country_code,
            granted_scopes,
            expires_at,
            api_base_url: Url::parse(TIDAL_API_BASE_URL)?,
            token_url: TIDAL_TOKEN_URL.to_owned(),
            persist_refreshed_token: true,
            retry_base_delay: Duration::from_secs(1),
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        api_base_url: Url,
        token_url: String,
        access_token: &str,
        refresh_token: Option<&str>,
        scopes: &str,
    ) -> Self {
        let token = StoredTidalUserToken {
            access_token: access_token.to_owned(),
            token_type: "Bearer".to_owned(),
            expires_in: 3_600,
            scope: scopes.to_owned(),
            refresh_token: refresh_token.map(ToOwned::to_owned),
            obtained_at: 1_700_000_000,
        };
        Self {
            client: Client::new(),
            granted_scopes: parse_scopes(scopes),
            expires_at: token.obtained_at.saturating_add(token.expires_in),
            token,
            client_id: "test-client".to_owned(),
            country_code: "PE".to_owned(),
            api_base_url,
            token_url,
            persist_refreshed_token: false,
            retry_base_delay: Duration::ZERO,
        }
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
        !self.token.access_token.is_empty()
    }

    pub fn require_scope(&self, scope: &str) -> Result<()> {
        if self.granted_scopes.contains(scope) {
            Ok(())
        } else {
            bail!(
                "The TIDAL user token does not include required scope `{scope}`; \
                 enable it in the dashboard, add it to TIDAL_SCOPES, and run \
                 `cargo run -- auth tidal` again"
            )
        }
    }

    pub async fn create_playlist(
        &mut self,
        name: &str,
        description: &str,
        idempotency_key: &str,
    ) -> Result<CreatedPlaylist> {
        self.require_scope("playlists.write")?;
        validate_idempotency_key(idempotency_key)?;
        if name.trim().is_empty() {
            bail!("The destination TIDAL playlist name cannot be empty");
        }

        let mut url = self.api_url(&["playlists"])?;
        url.query_pairs_mut()
            .append_pair("countryCode", &self.country_code);
        let body = json!({
            "data": {
                "type": "playlists",
                "attributes": {
                    "name": name,
                    "description": description,
                    "accessType": "UNLISTED"
                }
            }
        });
        let response = self
            .send_json_api(
                Method::POST,
                url,
                Some(body),
                Some(idempotency_key),
                &[StatusCode::CREATED],
            )
            .await?
            .context("TIDAL returned an empty playlist-creation response")?;
        let data = response
            .get("data")
            .and_then(Value::as_object)
            .context("TIDAL playlist-creation response has no data resource")?;
        let resource_type = data.get("type").and_then(Value::as_str).unwrap_or_default();
        if resource_type != "playlists" {
            bail!(
                "TIDAL playlist-creation response returned unexpected resource type `{resource_type}`"
            );
        }
        let id = data
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .context("TIDAL playlist-creation response has no playlist ID")?
            .to_owned();
        let attributes = data.get("attributes").and_then(Value::as_object);

        Ok(CreatedPlaylist {
            id,
            name: attributes
                .and_then(|value| value.get("name"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            access_type: attributes
                .and_then(|value| value.get("accessType"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        })
    }

    pub async fn add_playlist_items(
        &mut self,
        playlist_id: &str,
        track_ids: &[String],
        idempotency_key: &str,
    ) -> Result<()> {
        self.require_scope("playlists.write")?;
        validate_resource_id("playlist", playlist_id)?;
        validate_idempotency_key(idempotency_key)?;
        if track_ids.is_empty() || track_ids.len() > 50 {
            bail!("A TIDAL playlist add request must contain between 1 and 50 tracks");
        }
        for track_id in track_ids {
            validate_resource_id("track", track_id)?;
        }

        let mut url = self.api_url(&["playlists", playlist_id, "relationships", "items"])?;
        url.query_pairs_mut()
            .append_pair("countryCode", &self.country_code);
        let data = track_ids
            .iter()
            .map(|id| json!({ "type": "tracks", "id": id }))
            .collect::<Vec<_>>();
        self.send_json_api(
            Method::POST,
            url,
            Some(json!({ "data": data })),
            Some(idempotency_key),
            &[StatusCode::OK],
        )
        .await?;

        Ok(())
    }

    pub async fn playlist_items(
        &mut self,
        playlist_id: &str,
    ) -> Result<Vec<PlaylistItemIdentifier>> {
        validate_resource_id("playlist", playlist_id)?;
        let mut next_url = self.api_url(&["playlists", playlist_id, "relationships", "items"])?;
        next_url
            .query_pairs_mut()
            .append_pair("countryCode", &self.country_code)
            .append_pair("sort", "itemIndex");
        let mut items = Vec::new();
        let mut visited_pages = BTreeSet::new();

        loop {
            if !visited_pages.insert(next_url.as_str().to_owned()) {
                bail!("TIDAL playlist-items pagination repeated the same page URL");
            }
            let response = self
                .send_json_api(Method::GET, next_url.clone(), None, None, &[StatusCode::OK])
                .await?
                .context("TIDAL returned an empty playlist-items response")?;
            let data = response
                .get("data")
                .and_then(Value::as_array)
                .context("TIDAL playlist-items response has no data array")?;
            for item in data {
                let id = item
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    .context("TIDAL playlist item has no resource ID")?;
                let resource_type = item
                    .get("type")
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    .context("TIDAL playlist item has no resource type")?;
                items.push(PlaylistItemIdentifier {
                    id: id.to_owned(),
                    resource_type: resource_type.to_owned(),
                });
            }

            let next = response
                .get("links")
                .and_then(|links| links.get("next"))
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty());
            let Some(next) = next else {
                break;
            };
            next_url = self.resolve_next_url(next)?;
        }

        Ok(items)
    }

    fn api_url(&self, segments: &[&str]) -> Result<Url> {
        let mut url = self.api_base_url.clone();
        {
            let mut path = url
                .path_segments_mut()
                .map_err(|_| anyhow::anyhow!("The configured TIDAL API base URL is invalid"))?;
            path.pop_if_empty();
            path.extend(segments.iter().copied());
        }
        Ok(url)
    }

    fn resolve_next_url(&self, next: &str) -> Result<Url> {
        let url = match Url::parse(next) {
            Ok(url) => url,
            Err(url::ParseError::RelativeUrlWithoutBase) => {
                // TIDAL currently returns links such as `/playlists/...` even
                // though the public API is rooted at `/v2/`. Treat those as
                // API-root-relative, while still accepting `/v2/...` links.
                let base_path = self.api_base_url.path();
                let relative = if next.starts_with('/') && !next.starts_with(base_path) {
                    next.trim_start_matches('/')
                } else {
                    next
                };
                self.api_base_url
                    .join(relative)
                    .context("TIDAL returned an invalid pagination URL")?
            }
            Err(error) => return Err(error).context("TIDAL returned an invalid pagination URL"),
        };
        if url.scheme() != self.api_base_url.scheme()
            || url.host_str() != self.api_base_url.host_str()
            || url.port_or_known_default() != self.api_base_url.port_or_known_default()
        {
            bail!("TIDAL returned a pagination URL for an unexpected origin");
        }
        let base_path = self.api_base_url.path().trim_end_matches('/');
        if url.path() != base_path
            && !url
                .path()
                .strip_prefix(base_path)
                .is_some_and(|suffix| suffix.starts_with('/'))
        {
            bail!("TIDAL returned a pagination URL outside the public API base path");
        }
        Ok(url)
    }

    async fn send_json_api(
        &mut self,
        method: Method,
        url: Url,
        body: Option<Value>,
        idempotency_key: Option<&str>,
        expected_statuses: &[StatusCode],
    ) -> Result<Option<Value>> {
        let mut refreshed_after_unauthorized = false;

        for attempt in 0..MAX_REQUEST_ATTEMPTS {
            let mut request = self
                .client
                .request(method.clone(), url.clone())
                .bearer_auth(&self.token.access_token)
                .header("Accept", JSON_API_MEDIA_TYPE);
            if let Some(idempotency_key) = idempotency_key {
                request = request.header("Idempotency-Key", idempotency_key);
            }
            if let Some(body) = body.as_ref() {
                request = request
                    .header("Content-Type", JSON_API_MEDIA_TYPE)
                    .json(body);
            }

            let response = match request.send().await {
                Ok(response) => response,
                Err(error)
                    if is_temporary_network_error(&error) && attempt + 1 < MAX_REQUEST_ATTEMPTS =>
                {
                    sleep(self.retry_delay(attempt, None)).await;
                    continue;
                }
                Err(error) => {
                    return Err(error).context("Could not contact the TIDAL user API");
                }
            };
            let status = response.status();
            if expected_statuses.contains(&status) {
                let body = read_limited_body(response, MAX_RESPONSE_BODY_BYTES).await?;
                if body.is_empty() {
                    return Ok(None);
                }
                let value = serde_json::from_slice(&body)
                    .context("TIDAL returned an invalid JSON:API response")?;
                return Ok(Some(value));
            }

            if status == StatusCode::UNAUTHORIZED && !refreshed_after_unauthorized {
                self.refresh_access_token().await?;
                refreshed_after_unauthorized = true;
                continue;
            }

            if (status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
                && attempt + 1 < MAX_REQUEST_ATTEMPTS
            {
                let retry_after = retry_after(&response);
                // Consume a bounded body before retrying so the connection can be reused.
                let _ = read_limited_body(response, MAX_ERROR_DETAIL_CHARS * 4).await;
                sleep(self.retry_delay(attempt, retry_after)).await;
                continue;
            }

            let body = read_limited_body(response, MAX_ERROR_DETAIL_CHARS * 4).await?;
            bail!("{}", sanitized_api_error(status, &body));
        }

        bail!("TIDAL request exhausted its retry budget")
    }

    async fn refresh_access_token(&mut self) -> Result<()> {
        let now = current_unix_timestamp()?;
        let refreshed = refresh_token_value_at(
            &self.client,
            &self.token_url,
            &self.client_id,
            self.token.clone(),
            now,
        )
        .await?;
        if self.persist_refreshed_token {
            save_token(&refreshed)?;
        }
        self.expires_at = refreshed.obtained_at.saturating_add(refreshed.expires_in);
        self.granted_scopes = parse_scopes(&refreshed.scope);
        self.token = refreshed;
        Ok(())
    }

    fn retry_delay(&self, attempt: usize, retry_after: Option<Duration>) -> Duration {
        let jitter = Duration::from_millis(rand::rng().random_range(0..=250));
        if let Some(delay) = retry_after {
            return delay.saturating_add(jitter);
        }
        let multiplier = 1_u32 << attempt.min(5);
        let base = self.retry_base_delay.saturating_mul(multiplier);
        base.saturating_add(jitter)
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

fn validate_resource_id(description: &str, id: &str) -> Result<()> {
    if id.trim().is_empty() || id.chars().any(char::is_control) {
        bail!("The TIDAL {description} ID is empty or contains control characters");
    }
    Ok(())
}

fn validate_idempotency_key(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value.is_ascii()
        || value.chars().any(char::is_control)
    {
        bail!("A TIDAL idempotency key must be 1 to 128 printable ASCII characters");
    }
    Ok(())
}

fn is_temporary_network_error(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.is_request()
}

fn retry_after(response: &Response) -> Option<Duration> {
    let value = response.headers().get("Retry-After")?.to_str().ok()?;
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let retry_at = httpdate::parse_http_date(value).ok()?;
    retry_at.duration_since(SystemTime::now()).ok()
}

async fn read_limited_body(response: Response, limit: usize) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        bail!("TIDAL response body exceeded the configured {limit}-byte limit");
    }

    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("Could not read the TIDAL response body")?;
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(io::Error::other("response body limit exceeded")).context(format!(
                "TIDAL response body exceeded the configured {limit}-byte limit"
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn sanitized_api_error(status: StatusCode, body: &[u8]) -> String {
    let mut details = Vec::new();
    if let Ok(value) = serde_json::from_slice::<Value>(body)
        && let Some(errors) = value.get("errors").and_then(Value::as_array)
    {
        for error in errors.iter().take(5) {
            let code = error
                .get("code")
                .and_then(Value::as_str)
                .map(sanitize_error_text);
            let detail = error
                .get("detail")
                .and_then(Value::as_str)
                .map(sanitize_error_text);
            match (code, detail) {
                (Some(code), Some(detail)) => details.push(format!("{code}: {detail}")),
                (Some(code), None) => details.push(code),
                (None, Some(detail)) => details.push(detail),
                (None, None) => {}
            }
        }
    }

    if details.is_empty() {
        format!("TIDAL user API request failed with HTTP {status}")
    } else {
        format!(
            "TIDAL user API request failed with HTTP {status}: {}",
            details.join("; ")
        )
    }
}

fn sanitize_error_text(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_ERROR_DETAIL_CHARS)
        .collect()
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
    client_id: &str,
    stored: StoredTidalUserToken,
    now: u64,
) -> Result<StoredTidalUserToken> {
    let merged = refresh_token_value_at(client, token_url, client_id, stored, now).await?;
    save_token(&merged)?;
    Ok(merged)
}

async fn refresh_token_value_at(
    client: &Client,
    token_url: &str,
    client_id: &str,
    stored: StoredTidalUserToken,
    now: u64,
) -> Result<StoredTidalUserToken> {
    let refresh_token = stored
        .refresh_token
        .as_deref()
        .context("No TIDAL refresh token is stored; run `cargo run -- auth tidal` again")?;
    let refreshed = request_refresh_token_at(client, token_url, client_id, refresh_token).await?;
    Ok(merge_refreshed_token(stored, refreshed, now))
}

async fn request_refresh_token_at(
    client: &Client,
    token_url: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<TidalUserTokenResponse> {
    let response = client
        .post(token_url)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
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
    use std::{
        collections::{BTreeSet, VecDeque},
        sync::Arc,
    };

    use axum::{
        Router,
        body::{Body, to_bytes},
        extract::{Request, State},
        http::{HeaderName, HeaderValue, StatusCode},
        response::Response,
        routing::{any, post},
    };
    use tokio::sync::Mutex;
    use url::Url;

    use super::{
        ScopeResponse, StoredTidalUserToken, TidalUserClient, TidalUserTokenResponse,
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
        let response =
            request_refresh_token_at(&reqwest::Client::new(), &url, "client-id", "old-refresh")
                .await
                .unwrap();
        let refreshed = merge_refreshed_token(stored_token(), response, 2_000);
        assert_eq!(refreshed.refresh_token.as_deref(), Some("old-refresh"));
        assert_eq!(refreshed.access_token, "new-access");
        let body = bodies.lock().await.join("");
        assert!(body.contains("grant_type=refresh_token"));
        assert!(body.contains("client_id=client-id"));
        task.abort();
    }

    #[tokio::test]
    async fn creates_playlist_with_json_api_and_idempotency() {
        let mock = ApiMock::start(vec![MockResponse::json(
            StatusCode::CREATED,
            r#"{"data":{"type":"playlists","id":"playlist-1","attributes":{"name":"Lista","accessType":"UNLISTED"}}}"#,
        )])
        .await;
        let mut client = mock.client("old-access", Some("refresh"));
        let created = client
            .create_playlist("Lista", "Descripción", "create-key")
            .await
            .unwrap();
        assert_eq!(created.id, "playlist-1");
        assert_eq!(created.name.as_deref(), Some("Lista"));

        let requests = mock.requests.lock().await;
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].path, "/v2/playlists?countryCode=PE");
        assert_eq!(
            requests[0].content_type.as_deref(),
            Some("application/vnd.api+json")
        );
        assert_eq!(requests[0].idempotency_key.as_deref(), Some("create-key"));
        assert!(requests[0].body.contains("\"type\":\"playlists\""));
        mock.task.abort();
    }

    #[tokio::test]
    async fn retries_429_and_temporary_500_before_adding_items() {
        let mock = ApiMock::start(vec![
            MockResponse::json(StatusCode::TOO_MANY_REQUESTS, error_json("rate limited"))
                .with_header("Retry-After", "0"),
            MockResponse::json(StatusCode::INTERNAL_SERVER_ERROR, error_json("temporary")),
            MockResponse::json(
                StatusCode::OK,
                r#"{"data":[{"type":"tracks","id":"track-1"}],"links":{"self":"/items"}}"#,
            ),
        ])
        .await;
        let mut client = mock.client("access", Some("refresh"));
        client
            .add_playlist_items(
                "playlist-1",
                &["track-1".to_owned(), "track-2".to_owned()],
                "batch-key",
            )
            .await
            .unwrap();
        assert_eq!(mock.requests.lock().await.len(), 3);
        mock.task.abort();
    }

    #[tokio::test]
    async fn submits_multiple_batches_sequentially() {
        let mock = ApiMock::start(vec![
            MockResponse::json(
                StatusCode::OK,
                r#"{"data":[{"type":"tracks","id":"track-1"}],"links":{"next":null}}"#,
            ),
            MockResponse::json(
                StatusCode::OK,
                r#"{"data":[{"type":"tracks","id":"track-3"}],"links":{"next":null}}"#,
            ),
        ])
        .await;
        let mut client = mock.client("access", Some("refresh"));
        client
            .add_playlist_items(
                "playlist-1",
                &["track-1".to_owned(), "track-2".to_owned()],
                "batch-0",
            )
            .await
            .unwrap();
        client
            .add_playlist_items("playlist-1", &["track-3".to_owned()], "batch-1")
            .await
            .unwrap();

        let requests = mock.requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].idempotency_key.as_deref(), Some("batch-0"));
        assert_eq!(requests[1].idempotency_key.as_deref(), Some("batch-1"));
        mock.task.abort();
    }

    #[tokio::test]
    async fn does_not_retry_permanent_400() {
        let mock = ApiMock::start(vec![MockResponse::json(
            StatusCode::BAD_REQUEST,
            error_json("bad payload"),
        )])
        .await;
        let mut client = mock.client("access", Some("refresh"));
        let error = client
            .add_playlist_items("playlist-1", &["track-1".to_owned()], "batch-key")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("bad payload"));
        assert_eq!(mock.requests.lock().await.len(), 1);
        mock.task.abort();
    }

    #[tokio::test]
    async fn refreshes_once_after_401_and_retries_creation() {
        let mock = ApiMock::start(vec![
            MockResponse::json(StatusCode::UNAUTHORIZED, error_json("expired")),
            MockResponse::json(
                StatusCode::OK,
                r#"{"access_token":"new-access","token_type":"Bearer","expires_in":3600,"scope":"playlists.write"}"#,
            ),
            MockResponse::json(
                StatusCode::CREATED,
                r#"{"data":{"type":"playlists","id":"playlist-1","attributes":{"name":"Lista"}}}"#,
            ),
        ])
        .await;
        let mut client = mock.client("old-access", Some("refresh"));
        let created = client
            .create_playlist("Lista", "Descripción", "create-key")
            .await
            .unwrap();
        assert_eq!(created.id, "playlist-1");

        let requests = mock.requests.lock().await;
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[1].path, "/token");
        assert!(requests[1].body.contains("client_id=test-client"));
        assert!(requests[1].body.contains("grant_type=refresh_token"));
        assert_eq!(
            requests[2].authorization.as_deref(),
            Some("Bearer new-access")
        );
        mock.task.abort();
    }

    #[tokio::test]
    async fn follows_playlist_item_pagination_in_item_order() {
        let mock = ApiMock::start(vec![
            MockResponse::json(
                StatusCode::OK,
                r#"{"data":[{"type":"tracks","id":"track-a"},{"type":"tracks","id":"track-b"}],"links":{"next":"/playlists/playlist-1/relationships/items?page[cursor]=next"}}"#,
            ),
            MockResponse::json(
                StatusCode::OK,
                r#"{"data":[{"type":"tracks","id":"track-a"}],"links":{"next":null}}"#,
            ),
        ])
        .await;
        let mut client = mock.client("access", Some("refresh"));
        let items = client.playlist_items("playlist-1").await.unwrap();
        assert_eq!(
            items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["track-a", "track-b", "track-a"]
        );
        let requests = mock.requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1].path,
            "/v2/playlists/playlist-1/relationships/items?page[cursor]=next"
        );
        mock.task.abort();
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

    #[derive(Debug, Clone)]
    struct RecordedRequest {
        method: String,
        path: String,
        body: String,
        content_type: Option<String>,
        authorization: Option<String>,
        idempotency_key: Option<String>,
    }

    #[derive(Clone)]
    struct MockResponse {
        status: StatusCode,
        body: &'static str,
        headers: Vec<(HeaderName, HeaderValue)>,
    }

    impl MockResponse {
        fn json(status: StatusCode, body: &'static str) -> Self {
            Self {
                status,
                body,
                headers: vec![(
                    HeaderName::from_static("content-type"),
                    HeaderValue::from_static("application/json"),
                )],
            }
        }

        fn with_header(mut self, name: &'static str, value: &'static str) -> Self {
            self.headers.push((
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_static(value),
            ));
            self
        }
    }

    #[derive(Clone)]
    struct ApiMockState {
        responses: Arc<Mutex<VecDeque<MockResponse>>>,
        requests: Arc<Mutex<Vec<RecordedRequest>>>,
    }

    struct ApiMock {
        base_url: Url,
        token_url: String,
        requests: Arc<Mutex<Vec<RecordedRequest>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl ApiMock {
        async fn start(responses: Vec<MockResponse>) -> Self {
            async fn handler(
                State(state): State<ApiMockState>,
                request: Request,
            ) -> Response<Body> {
                let method = request.method().to_string();
                let path = request
                    .uri()
                    .path_and_query()
                    .map(ToString::to_string)
                    .unwrap_or_default();
                let content_type = request
                    .headers()
                    .get("content-type")
                    .and_then(|value| value.to_str().ok())
                    .map(ToOwned::to_owned);
                let authorization = request
                    .headers()
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .map(ToOwned::to_owned);
                let idempotency_key = request
                    .headers()
                    .get("idempotency-key")
                    .and_then(|value| value.to_str().ok())
                    .map(ToOwned::to_owned);
                let body = to_bytes(request.into_body(), 64 * 1024).await.unwrap();
                state.requests.lock().await.push(RecordedRequest {
                    method,
                    path,
                    body: String::from_utf8_lossy(&body).into_owned(),
                    content_type,
                    authorization,
                    idempotency_key,
                });

                let response = state
                    .responses
                    .lock()
                    .await
                    .pop_front()
                    .expect("unexpected mock API request");
                let mut builder = Response::builder().status(response.status);
                for (name, value) in response.headers {
                    builder = builder.header(name, value);
                }
                builder.body(Body::from(response.body)).unwrap()
            }

            let requests = Arc::new(Mutex::new(Vec::new()));
            let state = ApiMockState {
                responses: Arc::new(Mutex::new(responses.into())),
                requests: requests.clone(),
            };
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let app = Router::new().fallback(any(handler)).with_state(state);
            let task = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            Self {
                base_url: Url::parse(&format!("http://{address}/v2/")).unwrap(),
                token_url: format!("http://{address}/token"),
                requests,
                task,
            }
        }

        fn client(&self, access_token: &str, refresh_token: Option<&str>) -> TidalUserClient {
            TidalUserClient::for_test(
                self.base_url.clone(),
                self.token_url.clone(),
                access_token,
                refresh_token,
                "playlists.write",
            )
        }
    }

    fn error_json(detail: &'static str) -> &'static str {
        match detail {
            "rate limited" => r#"{"errors":[{"status":"429","detail":"rate limited"}]}"#,
            "temporary" => r#"{"errors":[{"status":"500","detail":"temporary"}]}"#,
            "bad payload" => r#"{"errors":[{"status":"400","detail":"bad payload"}]}"#,
            "expired" => r#"{"errors":[{"status":"401","detail":"expired"}]}"#,
            _ => r#"{"errors":[{"detail":"error"}]}"#,
        }
    }
}
