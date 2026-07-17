use std::{
    collections::HashMap,
    env, fs,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use clap::{Parser, Subcommand, ValueEnum};
use rand::{RngExt, distr::Alphanumeric};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use url::Url;

const SPOTIFY_AUTHORIZE_URL: &str = "https://accounts.spotify.com/authorize";
const SPOTIFY_TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
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
}

#[derive(Debug, Clone, ValueEnum)]
enum Provider {
    Spotify,
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

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let cli = Cli::parse();

    match cli.command {
        Command::Auth {
            provider: Provider::Spotify,
        } => authenticate_spotify().await,
    }
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
