use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use clap::{Parser, Subcommand, ValueEnum};
use rand::{RngExt, distr::Alphanumeric};
use reqwest::Client;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use url::Url;

mod matching;
mod model;
mod tidal;
mod tidal_user;

use matching::{failed_match, match_candidates, search_query};
use model::{
    ExportedPlaylistMetadata, MatchReport, MatchSummary, SkippedPlaylistItem, SourceTrack,
    SpotifyPlaylistExport,
};

const SPOTIFY_AUTHORIZE_URL: &str = "https://accounts.spotify.com/authorize";
const SPOTIFY_TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const SPOTIFY_API_URL: &str = "https://api.spotify.com/v1";
const TOKEN_PATH: &str = "data/spotify-token.json";

#[derive(Debug, Parser)]
#[command(name = "spotify-tidal-migrator")]
#[command(about = "Migrate playlists from Spotify to TIDAL")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Authenticate with a music provider.
    Auth {
        #[arg(value_enum)]
        provider: Provider,
    },

    /// Export one owned or collaborative Spotify playlist to JSON.
    ExportSpotify {
        /// Spotify playlist URL, URI, or raw playlist ID.
        playlist: String,

        /// Optional destination path.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Match tracks from a Spotify export against the public TIDAL catalog.
    MatchTidal {
        /// Spotify playlist export JSON created by export-spotify.
        input: PathBuf,

        /// Only process the first N exported tracks.
        #[arg(long)]
        limit: Option<usize>,

        /// Optional report destination.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Verify TIDAL authentication and catalog access.
    TidalTest,
}

#[derive(Debug, Clone, ValueEnum)]
enum Provider {
    Spotify,
    Tidal,
}

#[derive(Debug, Deserialize)]
struct SpotifyTokenResponse {
    access_token: String,
    token_type: String,
    expires_in: u64,

    #[serde(default)]
    scope: String,

    refresh_token: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredSpotifyToken {
    access_token: String,
    token_type: String,
    expires_in: u64,
    scope: String,
    refresh_token: Option<String>,
    obtained_at: u64,
}

#[derive(Debug, Deserialize)]
struct SpotifyPlaylistMetadataResponse {
    id: String,
    name: String,

    #[serde(default)]
    description: Option<String>,

    snapshot_id: String,

    #[serde(default)]
    external_urls: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct SpotifyPlaylistItemsPage {
    items: Vec<SpotifyPlaylistEntry>,
    next: Option<String>,
    total: usize,
}

#[derive(Debug, Deserialize)]
struct SpotifyPlaylistEntry {
    added_at: Option<String>,

    #[serde(default)]
    is_local: bool,

    item: Option<SpotifyPlaylistItem>,
}

#[derive(Debug, Deserialize)]
struct SpotifyPlaylistItem {
    id: Option<String>,
    name: String,
    uri: String,
    duration_ms: u64,

    #[serde(default)]
    explicit: bool,

    #[serde(default)]
    artists: Vec<SpotifyArtist>,

    album: Option<SpotifyAlbum>,
    external_ids: Option<SpotifyExternalIds>,

    #[serde(rename = "type")]
    item_type: String,
}

#[derive(Debug, Deserialize)]
struct SpotifyArtist {
    name: String,
}

#[derive(Debug, Deserialize)]
struct SpotifyAlbum {
    name: String,
}

#[derive(Debug, Deserialize)]
struct SpotifyExternalIds {
    isrc: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let cli = Cli::parse();

    match cli.command {
        Command::Auth {
            provider: Provider::Spotify,
        } => authenticate_spotify().await,
        Command::Auth {
            provider: Provider::Tidal,
        } => tidal_user::authenticate().await,

        Command::ExportSpotify { playlist, output } => {
            export_spotify_playlist(&playlist, output).await
        }
        Command::MatchTidal {
            input,
            limit,
            output,
        } => match_tidal_playlist(&input, limit, output).await,
        Command::TidalTest => tidal::test_catalog().await,
    }
}

async fn match_tidal_playlist(
    input: &Path,
    limit: Option<usize>,
    output: Option<PathBuf>,
) -> Result<()> {
    let bytes = fs::read(input)
        .with_context(|| format!("Could not read Spotify export {}", input.display()))?;
    let export: SpotifyPlaylistExport = serde_json::from_slice(&bytes)
        .with_context(|| format!("Invalid Spotify export JSON in {}", input.display()))?;

    if export.schema_version != 1 {
        bail!(
            "Unsupported Spotify export schema version {}; expected version 1",
            export.schema_version
        );
    }

    let track_count = limit
        .unwrap_or(export.tracks.len())
        .min(export.tracks.len());
    let destination = output.unwrap_or_else(|| default_match_report_path(input));

    println!("Playlist: {}", export.playlist.name);
    println!("Tracks selected: {track_count}/{}", export.tracks.len());
    println!("Authenticating with TIDAL...");

    // Authentication happens exactly once; this client and its bearer token
    // are reused for every catalog request in the run.
    let tidal_client = tidal::TidalClient::from_env().await?;
    println!("TIDAL authentication succeeded.");
    println!("Country: {}", tidal_client.country_code());
    println!();

    let mut results = Vec::with_capacity(track_count);
    let mut summary = MatchSummary::default();

    for (index, track) in export.tracks.iter().take(track_count).enumerate() {
        let query = search_query(track);
        print!(
            "[{}/{}] {} — {} ... ",
            index + 1,
            track_count,
            track.title,
            track
                .artists
                .first()
                .map_or("Unknown artist", String::as_str)
        );

        let result = match tidal_client
            .search_tracks(&track.title, &track.artists)
            .await
        {
            Ok(candidates) => match_candidates(track, query, candidates),
            Err(error) => failed_match(track, query, format!("{error:#}")),
        };

        match result.score {
            Some(score) => println!("{} ({score}/100)", result.status),
            None => println!("{}", result.status),
        }
        if let Some(error) = &result.error {
            eprintln!("  Search error: {error}");
        }

        summary.record(result.status);
        results.push(result);

        if index + 1 < track_count {
            tokio::time::sleep(tidal::search_delay()).await;
        }
    }

    let report = MatchReport {
        schema_version: 1,
        generated_at_unix: current_unix_timestamp()?,
        source_playlist: export.playlist,
        country_code: tidal_client.country_code().to_owned(),
        processed_tracks: results.len(),
        summary,
        results,
    };

    write_json(&destination, &report)?;

    println!();
    println!("Playlist: {}", report.source_playlist.name);
    println!("Processed: {}", report.processed_tracks);
    println!("Exact: {}", report.summary.exact);
    println!("Probable: {}", report.summary.probable);
    println!("Review: {}", report.summary.review);
    println!("Missing: {}", report.summary.missing);
    println!("Saved to: {}", destination.display());

    Ok(())
}

async fn authenticate_spotify() -> Result<()> {
    let client_id =
        env::var("SPOTIFY_CLIENT_ID").context("SPOTIFY_CLIENT_ID is missing from .env")?;

    let redirect_uri = env::var("SPOTIFY_REDIRECT_URI")
        .unwrap_or_else(|_| "http://127.0.0.1:8989/callback/spotify".to_owned());

    let redirect_url = Url::parse(&redirect_uri).context("Invalid SPOTIFY_REDIRECT_URI")?;

    validate_redirect_url(&redirect_url)?;

    let host = redirect_url
        .host_str()
        .context("The redirect URI has no host")?;

    let port = redirect_url
        .port()
        .context("The redirect URI must contain an explicit port")?;

    // Start listening before opening the browser so the callback cannot arrive
    // before our local server is ready.
    let listener = TcpListener::bind((host, port))
        .await
        .with_context(|| format!("Could not listen on {host}:{port}"))?;

    let code_verifier = random_string(64);

    let code_challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes()));

    let expected_state = random_string(32);

    let scopes = [
        "playlist-read-private",
        "playlist-read-collaborative",
        "user-library-read",
    ]
    .join(" ");

    let mut authorization_url = Url::parse(SPOTIFY_AUTHORIZE_URL)?;

    authorization_url
        .query_pairs_mut()
        .append_pair("client_id", &client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("scope", &scopes)
        .append_pair("state", &expected_state)
        .append_pair("code_challenge_method", "S256")
        .append_pair("code_challenge", &code_challenge);

    println!("Opening Spotify authorization in your browser...");

    if let Err(error) = open::that(authorization_url.as_str()) {
        eprintln!("Could not open the browser automatically: {error}");
        println!("\nOpen this URL manually:\n\n{authorization_url}\n");
    }

    let authorization_code =
        wait_for_spotify_callback(listener, &redirect_url, &expected_state).await?;

    let token_response = exchange_authorization_code(
        &client_id,
        &redirect_uri,
        &authorization_code,
        &code_verifier,
    )
    .await?;

    let token = StoredSpotifyToken {
        access_token: token_response.access_token,
        token_type: token_response.token_type,
        expires_in: token_response.expires_in,
        scope: token_response.scope,
        refresh_token: token_response.refresh_token,
        obtained_at: current_unix_timestamp()?,
    };

    save_token(&token)?;

    println!("Spotify authentication completed.");
    println!("Token saved to {TOKEN_PATH}");
    println!("Granted scopes: {}", token.scope);

    Ok(())
}

async fn export_spotify_playlist(playlist_input: &str, output: Option<PathBuf>) -> Result<()> {
    let playlist_id = extract_spotify_playlist_id(playlist_input)?;
    let token = valid_spotify_token().await?;
    let client = Client::new();

    let metadata_url = format!("{SPOTIFY_API_URL}/playlists/{playlist_id}");

    let metadata: SpotifyPlaylistMetadataResponse =
        spotify_get_json(&client, &metadata_url, &token.access_token).await?;

    println!("Exporting: {}", metadata.name);

    let mut first_page_url =
        Url::parse(&format!("{SPOTIFY_API_URL}/playlists/{playlist_id}/items"))?;

    first_page_url
        .query_pairs_mut()
        .append_pair("limit", "50")
        .append_pair("offset", "0");

    let mut next_url = Some(first_page_url.to_string());
    let mut tracks = Vec::new();
    let mut skipped_items = Vec::new();
    let mut total_reported = 0_usize;
    let mut processed = 0_usize;

    while let Some(page_url) = next_url {
        let page: SpotifyPlaylistItemsPage =
            spotify_get_json(&client, &page_url, &token.access_token).await?;

        total_reported = page.total;

        for entry in page.items {
            processed += 1;
            let position = processed;

            let Some(item) = entry.item else {
                skipped_items.push(SkippedPlaylistItem {
                    position,
                    reason: "Spotify returned a null or unavailable item".to_owned(),
                    title: None,
                    spotify_uri: None,
                });

                continue;
            };

            if item.item_type != "track" {
                skipped_items.push(SkippedPlaylistItem {
                    position,
                    reason: format!("Unsupported Spotify item type: {}", item.item_type),
                    title: Some(item.name),
                    spotify_uri: Some(item.uri),
                });

                continue;
            }

            tracks.push(SourceTrack {
                position,
                added_at: entry.added_at,
                spotify_id: item.id,
                spotify_uri: item.uri,
                title: item.name,
                artists: item.artists.into_iter().map(|artist| artist.name).collect(),
                album: item.album.map(|album| album.name),
                duration_ms: item.duration_ms,
                isrc: item.external_ids.and_then(|ids| ids.isrc),
                explicit: item.explicit,
                is_local: entry.is_local,
            });
        }

        println!("Processed {processed}/{total_reported} playlist items...");
        next_url = page.next;
    }

    let spotify_url = metadata.external_urls.get("spotify").cloned();

    let destination = output.unwrap_or_else(|| default_export_path(&metadata));

    let export = SpotifyPlaylistExport {
        schema_version: 1,
        exported_at_unix: current_unix_timestamp()?,
        playlist: ExportedPlaylistMetadata {
            spotify_id: metadata.id,
            name: metadata.name,
            description: metadata.description,
            spotify_url,
            snapshot_id: metadata.snapshot_id,
            total_reported_by_spotify: total_reported,
        },
        tracks,
        skipped_items,
    };

    write_json(&destination, &export)?;

    println!();
    println!("Export completed.");
    println!("Tracks exported: {}", export.tracks.len());
    println!("Items skipped: {}", export.skipped_items.len());
    println!("Saved to: {}", destination.display());

    Ok(())
}

async fn spotify_get_json<T>(client: &Client, url: &str, access_token: &str) -> Result<T>
where
    T: DeserializeOwned,
{
    let response = client
        .get(url)
        .bearer_auth(access_token)
        .send()
        .await
        .with_context(|| format!("Could not contact Spotify: {url}"))?;

    let status = response.status();

    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    let body = response.text().await?;

    if !status.is_success() {
        if status.as_u16() == 403 {
            bail!(
                "Spotify returned 403 for {url}. \
                 Make sure you own the playlist or are a collaborator. \
                 Response: {body}"
            );
        }

        if status.as_u16() == 429 {
            bail!(
                "Spotify rate-limited the request. \
                 Retry-After: {} seconds. Response: {body}",
                retry_after.as_deref().unwrap_or("unknown")
            );
        }

        bail!("Spotify request failed with HTTP {status} for {url}:\n{body}");
    }

    serde_json::from_str(&body).with_context(|| format!("Spotify returned invalid JSON for {url}"))
}

async fn valid_spotify_token() -> Result<StoredSpotifyToken> {
    let token = load_token()?;
    let now = current_unix_timestamp()?;

    let expires_at = token.obtained_at.saturating_add(token.expires_in);

    if now < expires_at.saturating_sub(60) {
        return Ok(token);
    }

    refresh_spotify_token(token).await
}

async fn refresh_spotify_token(token: StoredSpotifyToken) -> Result<StoredSpotifyToken> {
    let client_id =
        env::var("SPOTIFY_CLIENT_ID").context("SPOTIFY_CLIENT_ID is missing from .env")?;

    let refresh_token = token.refresh_token.clone().context(
        "No Spotify refresh token is stored; \
             run `cargo run -- auth spotify` again",
    )?;

    let response = Client::new()
        .post(SPOTIFY_TOKEN_URL)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("client_id", client_id.as_str()),
        ])
        .send()
        .await
        .context("Could not contact Spotify's token endpoint")?;

    let status = response.status();
    let response_body = response.text().await?;

    if !status.is_success() {
        bail!(
            "Spotify token refresh failed with HTTP {}:\n{}\n\
             Run `cargo run -- auth spotify` again if the refresh token \
             is expired or revoked.",
            status,
            response_body
        );
    }

    let refreshed: SpotifyTokenResponse = serde_json::from_str(&response_body)
        .context("Spotify returned an invalid refresh-token response")?;

    let updated = StoredSpotifyToken {
        access_token: refreshed.access_token,
        token_type: refreshed.token_type,
        expires_in: refreshed.expires_in,

        scope: if refreshed.scope.is_empty() {
            token.scope
        } else {
            refreshed.scope
        },

        refresh_token: refreshed.refresh_token.or(token.refresh_token),

        obtained_at: current_unix_timestamp()?,
    };

    save_token(&updated)?;
    println!("Spotify access token refreshed.");

    Ok(updated)
}

fn extract_spotify_playlist_id(input: &str) -> Result<String> {
    let input = input.trim();

    if let Some(id) = input.strip_prefix("spotify:playlist:") {
        return validate_playlist_id(id);
    }

    if let Ok(url) = Url::parse(input) {
        if url.host_str() != Some("open.spotify.com") {
            bail!("Expected an open.spotify.com playlist URL");
        }

        let segments: Vec<_> = url
            .path_segments()
            .context("Spotify URL has no path")?
            .collect();

        if segments.len() >= 2 && segments[0] == "playlist" {
            return validate_playlist_id(segments[1]);
        }

        bail!("Spotify URL does not contain /playlist/<id>");
    }

    validate_playlist_id(input)
}

fn validate_playlist_id(id: &str) -> Result<String> {
    let valid = !id.is_empty()
        && id
            .chars()
            .all(|character| character.is_ascii_alphanumeric());

    if !valid {
        bail!("Invalid Spotify playlist ID: {id}");
    }

    Ok(id.to_owned())
}

fn default_export_path(metadata: &SpotifyPlaylistMetadataResponse) -> PathBuf {
    let name = sanitize_filename(&metadata.name);

    PathBuf::from(format!("data/{name}-{}.json", metadata.id))
}

fn default_match_report_path(input: &Path) -> PathBuf {
    let stem = input
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("spotify-export");
    let filename = format!("{stem}-tidal-matches.json");

    input
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("data"))
        .join(filename)
}

fn sanitize_filename(value: &str) -> String {
    let mut result = String::new();
    let mut previous_was_separator = false;

    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            result.push(character.to_ascii_lowercase());
            previous_was_separator = false;
        } else if !previous_was_separator {
            result.push('-');
            previous_was_separator = true;
        }
    }

    let result = result.trim_matches('-');

    if result.is_empty() {
        "playlist".to_owned()
    } else {
        result.to_owned()
    }
}

fn write_json<T>(path: &Path, value: &T) -> Result<()>
where
    T: Serialize,
{
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let serialized = serde_json::to_vec_pretty(value)?;

    fs::write(path, serialized).with_context(|| format!("Could not write {}", path.display()))?;

    Ok(())
}

fn load_token() -> Result<StoredSpotifyToken> {
    let bytes = fs::read(TOKEN_PATH).with_context(|| {
        format!(
            "Could not read {TOKEN_PATH}; \
             run `cargo run -- auth spotify` first"
        )
    })?;

    serde_json::from_slice(&bytes).context("The stored Spotify token file is invalid")
}

fn validate_redirect_url(redirect_url: &Url) -> Result<()> {
    if redirect_url.scheme() != "http" {
        bail!("The local callback must use http");
    }

    if redirect_url.host_str() != Some("127.0.0.1") {
        bail!("Use 127.0.0.1 rather than localhost");
    }

    if redirect_url.path() != "/callback/spotify" {
        bail!(
            "Expected callback path /callback/spotify, found {}",
            redirect_url.path()
        );
    }

    Ok(())
}

async fn wait_for_spotify_callback(
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
            send_browser_response(
                &mut socket,
                "400 Bad Request",
                "Invalid authorization callback.",
            )
            .await?;

            continue;
        };

        let callback_url = Url::parse(&format!(
            "http://127.0.0.1:{}{}",
            redirect_url.port().context("Missing callback port")?,
            request_target
        ))?;

        // Browsers may request /favicon.ico after loading the callback page.
        if callback_url.path() != redirect_url.path() {
            send_browser_response(&mut socket, "404 Not Found", "Not found.").await?;

            continue;
        }

        let parameters: HashMap<String, String> = callback_url.query_pairs().into_owned().collect();

        if let Some(error) = parameters.get("error") {
            send_browser_response(
                &mut socket,
                "400 Bad Request",
                "Spotify authorization was denied. You may close this tab.",
            )
            .await?;

            bail!("Spotify authorization failed: {error}");
        }

        let returned_state = parameters
            .get("state")
            .context("Spotify callback did not contain state")?;

        if returned_state != expected_state {
            send_browser_response(
                &mut socket,
                "400 Bad Request",
                "Invalid authorization state. You may close this tab.",
            )
            .await?;

            bail!("OAuth state validation failed");
        }

        let code = parameters
            .get("code")
            .context("Spotify callback did not contain an authorization code")?
            .to_owned();

        send_browser_response(
            &mut socket,
            "200 OK",
            "Spotify authorization completed. You may close this tab.",
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
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>Spotify authorization</title>
</head>
<body>
  <h1>{message}</h1>
</body>
</html>"#
    );

    let response = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    );

    socket.write_all(response.as_bytes()).await?;
    socket.shutdown().await?;

    Ok(())
}

async fn exchange_authorization_code(
    client_id: &str,
    redirect_uri: &str,
    authorization_code: &str,
    code_verifier: &str,
) -> Result<SpotifyTokenResponse> {
    let response = Client::new()
        .post(SPOTIFY_TOKEN_URL)
        .form(&[
            ("client_id", client_id),
            ("grant_type", "authorization_code"),
            ("code", authorization_code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", code_verifier),
        ])
        .send()
        .await
        .context("Could not contact Spotify's token endpoint")?;

    let status = response.status();
    let response_body = response.text().await?;

    if !status.is_success() {
        bail!(
            "Spotify token exchange failed with HTTP {}:\n{}",
            status,
            response_body
        );
    }

    serde_json::from_str(&response_body).context("Spotify returned an invalid token response")
}

fn save_token(token: &StoredSpotifyToken) -> Result<()> {
    let path = Path::new(TOKEN_PATH);

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let serialized = serde_json::to_vec_pretty(token)?;
    fs::write(path, serialized)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }

    Ok(())
}

fn current_unix_timestamp() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn random_string(length: usize) -> String {
    rand::rng()
        .sample_iter(&Alphanumeric)
        .take(length)
        .map(char::from)
        .collect()
}
