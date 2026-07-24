use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use clap::{Parser, Subcommand, ValueEnum};
use futures_util::{StreamExt, stream};
use inquire::MultiSelect;
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
const SPOTIFY_PAGE_LIMIT: usize = 50;
const SPOTIFY_MAX_ATTEMPTS: usize = 3;

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

        /// Maximum number of simultaneous Spotify page requests.
        #[arg(long, default_value_t = 4, value_parser = parse_concurrency)]
        concurrency: usize,
    },

    /// Export the current user's Spotify Liked Songs to JSON.
    ExportSpotifyLiked {
        /// Optional destination path.
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Maximum number of simultaneous Spotify page requests.
        #[arg(long, default_value_t = 4, value_parser = parse_concurrency)]
        concurrency: usize,
    },

    /// Select Spotify playlists or Liked Songs for a later migration (read-only).
    SelectSpotify {
        /// Maximum simultaneous Spotify page and TIDAL catalog requests.
        #[arg(long, default_value_t = 4, value_parser = parse_concurrency)]
        concurrency: usize,
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

        /// Maximum number of simultaneous TIDAL catalog searches.
        #[arg(long, default_value_t = 4, value_parser = parse_concurrency)]
        concurrency: usize,
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
    total: usize,
}

#[derive(Debug, Deserialize)]
struct SpotifyPlaylistsPage {
    items: Vec<SpotifyPlaylistSummary>,
    next: Option<String>,
    total: usize,
}

#[derive(Debug, Deserialize)]
struct SpotifySavedTracksPage {
    items: Vec<SpotifySavedTrackEntry>,
    total: usize,
}

#[derive(Debug, Deserialize)]
struct SpotifySavedTrackEntry {
    added_at: Option<String>,
    track: Option<SpotifyPlaylistItem>,
}

#[derive(Debug, Clone, Deserialize)]
struct SpotifyPlaylistSummary {
    id: String,
    name: String,
    owner: SpotifyPlaylistOwner,
    public: Option<bool>,

    #[serde(default)]
    collaborative: bool,

    uri: String,

    #[serde(default)]
    external_urls: HashMap<String, String>,

    #[serde(default)]
    items: Option<SpotifyPlaylistItemCount>,

    #[serde(default)]
    tracks: Option<SpotifyPlaylistItemCount>,
}

impl SpotifyPlaylistSummary {
    fn item_count(&self) -> usize {
        self.items
            .as_ref()
            .or(self.tracks.as_ref())
            .map_or(0, |items| items.total)
    }

    fn spotify_url(&self) -> Option<&str> {
        self.external_urls.get("spotify").map(String::as_str)
    }
}

impl std::fmt::Display for SpotifyPlaylistSummary {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let visibility = if self.collaborative {
            "collaborative"
        } else {
            match self.public {
                Some(true) => "public",
                Some(false) => "private",
                None => "visibility unknown",
            }
        };
        let owner = self.owner.display_name.as_deref().unwrap_or(&self.owner.id);

        write!(
            formatter,
            "{} — {} — {} items — {}",
            terminal_safe(&self.name),
            terminal_safe(owner),
            self.item_count(),
            visibility
        )
    }
}

#[derive(Debug, Clone)]
enum SpotifySelectionOption {
    LikedSongs { total: usize },
    Playlist(SpotifyPlaylistSummary),
}

impl SpotifySelectionOption {
    fn name(&self) -> &str {
        match self {
            Self::LikedSongs { .. } => "Liked Songs",
            Self::Playlist(playlist) => &playlist.name,
        }
    }
}

impl std::fmt::Display for SpotifySelectionOption {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LikedSongs { total } => {
                write!(formatter, "Liked Songs — {total} tracks — saved library")
            }
            Self::Playlist(playlist) => playlist.fmt(formatter),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct SpotifyPlaylistOwner {
    id: String,
    display_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct SpotifyPlaylistItemCount {
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
    is_local: bool,

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

        Command::ExportSpotify {
            playlist,
            output,
            concurrency,
        } => export_spotify_playlist(&playlist, output, concurrency)
            .await
            .map(|_| ()),
        Command::ExportSpotifyLiked {
            output,
            concurrency,
        } => export_spotify_liked(output, concurrency).await.map(|_| ()),
        Command::SelectSpotify { concurrency } => select_spotify_playlists(concurrency).await,
        Command::MatchTidal {
            input,
            limit,
            output,
            concurrency,
        } => match_tidal_playlist(&input, limit, output, concurrency).await,
        Command::TidalTest => tidal::test_catalog().await,
    }
}

async fn select_spotify_playlists(concurrency: usize) -> Result<()> {
    let token = valid_spotify_token().await?;
    let client = Client::new();
    let mut liked_songs_url = Url::parse(&format!("{SPOTIFY_API_URL}/me/tracks"))?;
    liked_songs_url
        .query_pairs_mut()
        .append_pair("limit", "1")
        .append_pair("offset", "0");
    let mut first_page_url = Url::parse(&format!("{SPOTIFY_API_URL}/me/playlists"))?;
    first_page_url
        .query_pairs_mut()
        .append_pair("limit", "50")
        .append_pair("offset", "0");

    println!("Fetching Spotify library...");
    let liked_songs_page: SpotifySavedTracksPage =
        spotify_get_json(&client, liked_songs_url.as_str(), &token.access_token).await?;
    let liked_songs_total = liked_songs_page.total;

    println!("Fetching Spotify playlists...");
    let mut playlists = Vec::new();
    let mut next_url = Some(first_page_url.to_string());

    while let Some(page_url) = next_url {
        let page: SpotifyPlaylistsPage =
            spotify_get_json(&client, &page_url, &token.access_token).await?;
        let total_reported = page.total;
        playlists.extend(page.items);
        println!("Fetched {}/{total_reported} playlists...", playlists.len());
        next_url = page.next;
    }

    println!(
        "Spotify also returns followed playlists. Only playlists you own or collaborate on can be exported; check the owner shown in each option."
    );
    let options = spotify_selection_options(liked_songs_total, playlists);
    let selected = MultiSelect::new("Select Spotify sources to migrate:", options)
        .with_help_message("Use Space to toggle, Enter to confirm, and Esc to cancel")
        .with_page_size(15)
        .prompt_skippable()
        .context("Could not read the Spotify playlist selection")?;

    let Some(selected) = selected else {
        println!("Playlist selection cancelled.");
        return Ok(());
    };

    if selected.is_empty() {
        println!("No playlists selected.");
        return Ok(());
    }

    println!();
    let selected_count = selected.len();
    println!("Selected {selected_count} source(s).");
    println!("Authenticating with TIDAL for read-only catalog matching...");
    let tidal_client = tidal::TidalClient::from_env().await?;
    println!("TIDAL authentication succeeded.");

    let mut summaries = Vec::with_capacity(selected_count);
    for (index, source) in selected.into_iter().enumerate() {
        println!();
        println!("Source {}/{selected_count}", index + 1);
        println!("Preparing: {}", terminal_safe(source.name()));
        if let SpotifySelectionOption::Playlist(playlist) = &source {
            println!("Spotify playlist ID: {}", terminal_safe(&playlist.id));
            if let Some(url) = playlist.spotify_url() {
                println!("Spotify URL: {}", terminal_safe(url));
            }
        }

        let source_name = source.name().to_owned();
        let export_result = match source {
            SpotifySelectionOption::LikedSongs { .. } => {
                export_spotify_liked(None, concurrency).await
            }
            SpotifySelectionOption::Playlist(playlist) => {
                export_spotify_playlist(&playlist.uri, None, concurrency).await
            }
        };

        let outcome = match export_result {
            Ok(export_path) => {
                match_tidal_playlist_with_client(
                    &export_path,
                    None,
                    None,
                    &tidal_client,
                    concurrency,
                )
                .await
            }
            Err(error) => Err(error),
        };

        match outcome {
            Ok(summary) => summaries.push(summary),
            Err(error) => {
                eprintln!(
                    "Could not process {}: {error:#}",
                    terminal_safe(&source_name)
                );
                summaries.push(SelectionMatchSummary::failed(
                    source_name,
                    format!("{error:#}"),
                ));
            }
        }
    }

    print_selection_match_summary(&summaries);
    println!("No TIDAL import was performed.");
    Ok(())
}

fn spotify_selection_options(
    liked_songs_total: usize,
    playlists: Vec<SpotifyPlaylistSummary>,
) -> Vec<SpotifySelectionOption> {
    let mut options = Vec::with_capacity(playlists.len() + 1);
    options.push(SpotifySelectionOption::LikedSongs {
        total: liked_songs_total,
    });
    options.extend(playlists.into_iter().map(SpotifySelectionOption::Playlist));
    options
}

#[derive(Debug)]
struct SelectionMatchSummary {
    source_name: String,
    processed: usize,
    exact: usize,
    probable: usize,
    review: usize,
    missing: usize,
    errors: usize,
    report_path: Option<PathBuf>,
    failure: Option<String>,
}

impl SelectionMatchSummary {
    fn matched(&self) -> usize {
        self.exact + self.probable
    }

    fn failed(source_name: String, failure: String) -> Self {
        Self {
            source_name,
            processed: 0,
            exact: 0,
            probable: 0,
            review: 0,
            missing: 0,
            errors: 0,
            report_path: None,
            failure: Some(failure),
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct SelectionMatchTotals {
    processed: usize,
    matched: usize,
    exact: usize,
    probable: usize,
    review: usize,
    missing: usize,
    errors: usize,
    failed_sources: usize,
}

fn selection_match_totals(summaries: &[SelectionMatchSummary]) -> SelectionMatchTotals {
    let mut totals = SelectionMatchTotals::default();

    for summary in summaries {
        totals.processed += summary.processed;
        totals.matched += summary.matched();
        totals.exact += summary.exact;
        totals.probable += summary.probable;
        totals.review += summary.review;
        totals.missing += summary.missing;
        totals.errors += summary.errors;
        totals.failed_sources += usize::from(summary.failure.is_some());
    }

    totals
}

fn print_selection_match_summary(summaries: &[SelectionMatchSummary]) {
    println!();
    println!("Match summary");
    println!(
        "Matched means Exact + Probable; Review remains separate; request errors are included in Missing."
    );

    for summary in summaries {
        println!();
        println!("- {}", terminal_safe(&summary.source_name));
        if let Some(failure) = &summary.failure {
            println!("  Failed: {}", terminal_safe(failure));
            continue;
        }

        println!(
            "  Processed: {} | Matched: {} | Exact: {} | Probable: {} | Review: {} | Missing: {} | Errors: {}",
            summary.processed,
            summary.matched(),
            summary.exact,
            summary.probable,
            summary.review,
            summary.missing,
            summary.errors
        );
        if let Some(path) = &summary.report_path {
            println!("  Report: {}", path.display());
        }
    }

    let totals = selection_match_totals(summaries);
    println!();
    println!(
        "Total — Processed: {} | Matched: {} | Exact: {} | Probable: {} | Review: {} | Missing: {} | Errors: {}",
        totals.processed,
        totals.matched,
        totals.exact,
        totals.probable,
        totals.review,
        totals.missing,
        totals.errors
    );
    if totals.failed_sources > 0 {
        println!(
            "Sources that could not be processed: {}",
            totals.failed_sources
        );
    }
    println!();
}

fn parse_concurrency(value: &str) -> std::result::Result<usize, String> {
    let concurrency = value
        .parse::<usize>()
        .map_err(|_| "concurrency must be an integer from 1 to 16".to_owned())?;

    if !(1..=16).contains(&concurrency) {
        return Err("concurrency must be from 1 to 16".to_owned());
    }

    Ok(concurrency)
}

fn restore_source_order<T>(expected_len: usize, completed: Vec<(usize, T)>) -> Result<Vec<T>> {
    let mut ordered: Vec<Option<T>> = std::iter::repeat_with(|| None).take(expected_len).collect();

    for (index, value) in completed {
        let slot = ordered
            .get_mut(index)
            .with_context(|| format!("Received an out-of-range match result at index {index}"))?;
        if slot.replace(value).is_some() {
            bail!("Received a duplicate match result at index {index}");
        }
    }

    ordered
        .into_iter()
        .enumerate()
        .map(|(index, value)| {
            value.with_context(|| format!("Missing match result for source index {index}"))
        })
        .collect()
}

async fn match_tidal_playlist(
    input: &Path,
    limit: Option<usize>,
    output: Option<PathBuf>,
    concurrency: usize,
) -> Result<()> {
    println!("Authenticating with TIDAL...");

    // Authentication happens exactly once; this client and its bearer token
    // are reused for every catalog request in the run.
    let tidal_client = tidal::TidalClient::from_env().await?;
    println!("TIDAL authentication succeeded.");
    println!("Country: {}", tidal_client.country_code());
    println!();

    match_tidal_playlist_with_client(input, limit, output, &tidal_client, concurrency)
        .await
        .map(|_| ())
}

async fn match_tidal_playlist_with_client(
    input: &Path,
    limit: Option<usize>,
    output: Option<PathBuf>,
    tidal_client: &tidal::TidalClient,
    concurrency: usize,
) -> Result<SelectionMatchSummary> {
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
    println!("Concurrent TIDAL searches: {concurrency}");

    let searches = stream::iter(export.tracks.iter().take(track_count).cloned().enumerate())
        .map(|(index, track)| async move {
            let query = search_query(&track);
            let result = match tidal_client
                .search_tracks(&track.title, &track.artists)
                .await
            {
                Ok(candidates) => match_candidates(&track, query, candidates),
                Err(error) => failed_match(&track, query, format!("{error:#}")),
            };

            (index, result)
        })
        .buffer_unordered(concurrency);
    tokio::pin!(searches);

    let mut completed = Vec::with_capacity(track_count);
    while let Some((index, result)) = searches.next().await {
        let completed_count = completed.len() + 1;
        let track = &result.spotify_track;
        match result.score {
            Some(score) => println!(
                "[{completed_count}/{track_count}] #{} {} — {}: {} ({score}/100)",
                index + 1,
                track.title,
                track
                    .artists
                    .first()
                    .map_or("Unknown artist", String::as_str),
                result.status
            ),
            None => println!(
                "[{completed_count}/{track_count}] #{} {} — {}: {}",
                index + 1,
                track.title,
                track
                    .artists
                    .first()
                    .map_or("Unknown artist", String::as_str),
                result.status
            ),
        }
        if let Some(error) = &result.error {
            eprintln!("  Search error: {error}");
        }
        completed.push((index, result));
    }

    let results = restore_source_order(track_count, completed)?;
    let mut summary = MatchSummary::default();
    for result in &results {
        summary.record(result.status);
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

    Ok(SelectionMatchSummary {
        source_name: report.source_playlist.name.clone(),
        processed: report.processed_tracks,
        exact: report.summary.exact,
        probable: report.summary.probable,
        review: report.summary.review,
        missing: report.summary.missing,
        errors: report
            .results
            .iter()
            .filter(|result| result.error.is_some())
            .count(),
        report_path: Some(destination),
        failure: None,
    })
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

fn spotify_offset_page_url(endpoint: &str, offset: usize) -> Result<Url> {
    let mut url = Url::parse(endpoint)?;
    url.query_pairs_mut()
        .append_pair("limit", &SPOTIFY_PAGE_LIMIT.to_string())
        .append_pair("offset", &offset.to_string());
    Ok(url)
}

async fn fetch_remaining_spotify_pages<T>(
    client: &Client,
    endpoint: &str,
    access_token: &str,
    total: usize,
    concurrency: usize,
    item_label: &str,
) -> Result<Vec<(usize, T)>>
where
    T: DeserializeOwned,
{
    let requests = stream::iter((SPOTIFY_PAGE_LIMIT..total).step_by(SPOTIFY_PAGE_LIMIT))
        .map(|offset| async move {
            let url = spotify_offset_page_url(endpoint, offset)?;
            let page = spotify_get_json(client, url.as_str(), access_token).await?;
            Ok::<_, anyhow::Error>((offset, page))
        })
        .buffer_unordered(concurrency);
    tokio::pin!(requests);

    let mut pages = Vec::new();
    while let Some(page) = requests.next().await {
        pages.push(page?);
        let fetched = ((pages.len() + 1) * SPOTIFY_PAGE_LIMIT).min(total);
        println!("Fetched {fetched}/{total} {item_label}...");
    }

    pages.sort_unstable_by_key(|(offset, _)| *offset);
    Ok(pages)
}

async fn export_spotify_playlist(
    playlist_input: &str,
    output: Option<PathBuf>,
    concurrency: usize,
) -> Result<PathBuf> {
    let playlist_id = extract_spotify_playlist_id(playlist_input)?;
    let token = valid_spotify_token().await?;
    let client = Client::new();

    let metadata_url = format!("{SPOTIFY_API_URL}/playlists/{playlist_id}");

    let metadata: SpotifyPlaylistMetadataResponse =
        spotify_get_json(&client, &metadata_url, &token.access_token).await?;

    println!("Exporting: {}", metadata.name);

    let items_endpoint = format!("{SPOTIFY_API_URL}/playlists/{playlist_id}/items");
    let first_page_url = spotify_offset_page_url(&items_endpoint, 0)?;

    let mut tracks = Vec::new();
    let mut skipped_items = Vec::new();
    let mut processed = 0_usize;
    let first_page: SpotifyPlaylistItemsPage =
        spotify_get_json(&client, first_page_url.as_str(), &token.access_token).await?;
    let total_reported = first_page.total;
    println!(
        "Fetched {}/{total_reported} playlist items...",
        first_page.items.len()
    );
    let mut pages = vec![(0, first_page)];
    pages.extend(
        fetch_remaining_spotify_pages(
            &client,
            &items_endpoint,
            &token.access_token,
            total_reported,
            concurrency,
            "playlist items",
        )
        .await?,
    );

    for (offset, page) in pages {
        for (page_index, entry) in page.items.into_iter().enumerate() {
            processed += 1;
            let position = offset + page_index + 1;

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
    }
    println!("Processed {processed}/{total_reported} playlist items.");

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

    Ok(destination)
}

async fn export_spotify_liked(output: Option<PathBuf>, concurrency: usize) -> Result<PathBuf> {
    let token = valid_spotify_token().await?;
    let client = Client::new();
    let tracks_endpoint = format!("{SPOTIFY_API_URL}/me/tracks");
    let first_page_url = spotify_offset_page_url(&tracks_endpoint, 0)?;

    println!("Exporting: Liked Songs");

    let mut tracks = Vec::new();
    let mut skipped_items = Vec::new();
    let mut processed = 0_usize;
    let first_page: SpotifySavedTracksPage =
        spotify_get_json(&client, first_page_url.as_str(), &token.access_token).await?;
    let total_reported = first_page.total;
    println!(
        "Fetched {}/{total_reported} saved tracks...",
        first_page.items.len()
    );
    let mut pages = vec![(0, first_page)];
    pages.extend(
        fetch_remaining_spotify_pages(
            &client,
            &tracks_endpoint,
            &token.access_token,
            total_reported,
            concurrency,
            "saved tracks",
        )
        .await?,
    );

    for (offset, page) in pages {
        for (page_index, entry) in page.items.into_iter().enumerate() {
            processed += 1;
            let position = offset + page_index + 1;

            let Some(track) = entry.track else {
                skipped_items.push(SkippedPlaylistItem {
                    position,
                    reason: "Spotify returned a null or unavailable saved track".to_owned(),
                    title: None,
                    spotify_uri: None,
                });
                continue;
            };

            if track.item_type != "track" {
                skipped_items.push(SkippedPlaylistItem {
                    position,
                    reason: format!("Unsupported Spotify item type: {}", track.item_type),
                    title: Some(track.name),
                    spotify_uri: Some(track.uri),
                });
                continue;
            }

            tracks.push(SourceTrack {
                position,
                added_at: entry.added_at,
                spotify_id: track.id,
                spotify_uri: track.uri,
                title: track.name,
                artists: track
                    .artists
                    .into_iter()
                    .map(|artist| artist.name)
                    .collect(),
                album: track.album.map(|album| album.name),
                duration_ms: track.duration_ms,
                isrc: track.external_ids.and_then(|ids| ids.isrc),
                explicit: track.explicit,
                is_local: track.is_local,
            });
        }
    }
    println!("Processed {processed}/{total_reported} saved tracks.");

    let destination = output.unwrap_or_else(|| PathBuf::from("data/liked-songs.json"));
    let export = SpotifyPlaylistExport {
        schema_version: 1,
        exported_at_unix: current_unix_timestamp()?,
        playlist: ExportedPlaylistMetadata {
            // Saved tracks are a library collection rather than a Spotify playlist,
            // so Spotify does not provide a playlist ID or snapshot ID for it.
            spotify_id: "liked-songs".to_owned(),
            name: "Liked Songs".to_owned(),
            description: Some("Tracks saved in the Spotify user's library.".to_owned()),
            spotify_url: Some("https://open.spotify.com/collection/tracks".to_owned()),
            snapshot_id: "not-available-for-saved-tracks".to_owned(),
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

    Ok(destination)
}

async fn spotify_get_json<T>(client: &Client, url: &str, access_token: &str) -> Result<T>
where
    T: DeserializeOwned,
{
    for attempt in 1..=SPOTIFY_MAX_ATTEMPTS {
        let response = client.get(url).bearer_auth(access_token).send().await;
        let response = match response {
            Ok(response) => response,
            Err(error)
                if attempt < SPOTIFY_MAX_ATTEMPTS && (error.is_timeout() || error.is_connect()) =>
            {
                tokio::time::sleep(spotify_retry_delay(attempt, None)).await;
                continue;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("Could not contact Spotify: {url}"));
            }
        };

        let status = response.status();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = response.text().await?;

        if status.is_success() {
            return serde_json::from_str(&body)
                .with_context(|| format!("Spotify returned invalid JSON for {url}"));
        }

        if status.as_u16() == 429 && attempt < SPOTIFY_MAX_ATTEMPTS {
            tokio::time::sleep(spotify_retry_delay(attempt, retry_after.as_deref())).await;
            continue;
        }

        if status.is_server_error() && attempt < SPOTIFY_MAX_ATTEMPTS {
            tokio::time::sleep(spotify_retry_delay(attempt, None)).await;
            continue;
        }

        if status.as_u16() == 403 {
            bail!(
                "Spotify returned 403 for {url}. \
                 Make sure you own the playlist or are a collaborator. \
                 Response: {body}"
            );
        }

        if status.as_u16() == 429 {
            bail!(
                "Spotify rate-limited the request after {SPOTIFY_MAX_ATTEMPTS} attempts. \
                 Retry-After: {} seconds. Response: {body}",
                retry_after.as_deref().unwrap_or("unknown")
            );
        }

        bail!("Spotify request failed with HTTP {status} for {url}:\n{body}");
    }

    bail!("Spotify request failed after {SPOTIFY_MAX_ATTEMPTS} attempts")
}

fn spotify_retry_delay(attempt: usize, retry_after: Option<&str>) -> std::time::Duration {
    let seconds = retry_after.and_then(|value| value.parse::<u64>().ok());
    seconds.map_or_else(
        || std::time::Duration::from_millis(300 * attempt as u64),
        std::time::Duration::from_secs,
    )
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

fn terminal_safe(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::{
        SelectionMatchSummary, SelectionMatchTotals, SpotifyPlaylistsPage, SpotifySavedTracksPage,
        SpotifySelectionOption, parse_concurrency, restore_source_order, selection_match_totals,
        spotify_offset_page_url, spotify_retry_delay, spotify_selection_options, terminal_safe,
    };

    #[test]
    fn deserializes_current_user_playlists_page() {
        let json = r#"{
          "items": [{
            "id": "playlist123",
            "name": "Indie Perú",
            "owner": {"id": "owner1", "display_name": "María"},
            "public": false,
            "collaborative": true,
            "uri": "spotify:playlist:playlist123",
            "external_urls": {"spotify": "https://open.spotify.com/playlist/playlist123"},
            "items": {"href": "https://api.spotify.com/v1/playlists/playlist123/items", "total": 12},
            "tracks": {"href": "https://api.spotify.com/v1/playlists/playlist123/tracks", "total": 99}
          }],
          "next": null,
          "total": 1
        }"#;

        let page: SpotifyPlaylistsPage = serde_json::from_str(json).unwrap();
        let playlist = &page.items[0];
        assert_eq!(page.total, 1);
        assert_eq!(playlist.name, "Indie Perú");
        assert_eq!(playlist.item_count(), 12);
        assert_eq!(
            playlist.spotify_url(),
            Some("https://open.spotify.com/playlist/playlist123")
        );
        assert!(playlist.to_string().contains("collaborative"));
    }

    #[test]
    fn supports_deprecated_tracks_count_as_a_fallback() {
        let json = r#"{
          "items": [{
            "id": "playlist123",
            "name": "Archivo",
            "owner": {"id": "owner1", "display_name": null},
            "public": null,
            "collaborative": false,
            "uri": "spotify:playlist:playlist123",
            "external_urls": {},
            "tracks": {"total": 7}
          }],
          "next": null,
          "total": 1
        }"#;

        let page: SpotifyPlaylistsPage = serde_json::from_str(json).unwrap();
        assert_eq!(page.items[0].item_count(), 7);
        assert!(page.items[0].to_string().contains("visibility unknown"));
    }

    #[test]
    fn deserializes_saved_tracks_page() {
        let json = r#"{
          "items": [{
            "added_at": "2026-07-24T12:00:00Z",
            "track": {
              "id": "track123",
              "name": "¿Para Qué Me Hablas?",
              "uri": "spotify:track:track123",
              "duration_ms": 198000,
              "explicit": false,
              "is_local": false,
              "artists": [{"name": "Los Outsaiders"}],
              "album": {"name": "El Asesino del Rey Peste"},
              "external_ids": {"isrc": "PE1234567890"},
              "type": "track"
            }
          }],
          "next": null,
          "total": 1
        }"#;

        let page: SpotifySavedTracksPage = serde_json::from_str(json).unwrap();
        let entry = &page.items[0];
        let track = entry.track.as_ref().unwrap();

        assert_eq!(page.total, 1);
        assert_eq!(track.name, "¿Para Qué Me Hablas?");
        assert_eq!(track.artists[0].name, "Los Outsaiders");
        assert_eq!(
            track.external_ids.as_ref().unwrap().isrc.as_deref(),
            Some("PE1234567890")
        );
        assert!(!track.is_local);
    }

    #[test]
    fn liked_songs_is_the_first_selection_option() {
        let options = spotify_selection_options(42, Vec::new());

        assert_eq!(options.len(), 1);
        assert!(matches!(
            options.first(),
            Some(SpotifySelectionOption::LikedSongs { total: 42 })
        ));
        assert_eq!(
            options[0].to_string(),
            "Liked Songs — 42 tracks — saved library"
        );
    }

    #[test]
    fn aggregates_match_summaries_without_counting_review_as_matched() {
        let summaries = vec![
            SelectionMatchSummary {
                source_name: "Indie Perú".to_owned(),
                processed: 10,
                exact: 7,
                probable: 1,
                review: 1,
                missing: 1,
                errors: 0,
                report_path: None,
                failure: None,
            },
            SelectionMatchSummary::failed(
                "Unavailable playlist".to_owned(),
                "Spotify returned HTTP 403".to_owned(),
            ),
        ];

        assert_eq!(
            selection_match_totals(&summaries),
            SelectionMatchTotals {
                processed: 10,
                matched: 8,
                exact: 7,
                probable: 1,
                review: 1,
                missing: 1,
                errors: 0,
                failed_sources: 1,
            }
        );
    }

    #[test]
    fn restores_concurrent_results_to_source_order() {
        let ordered = restore_source_order(3, vec![(2, "third"), (0, "first"), (1, "second")])
            .expect("completion indexes are valid");

        assert_eq!(ordered, vec!["first", "second", "third"]);
    }

    #[test]
    fn validates_match_concurrency_bounds() {
        assert_eq!(parse_concurrency("1"), Ok(1));
        assert_eq!(parse_concurrency("16"), Ok(16));
        assert!(parse_concurrency("0").is_err());
        assert!(parse_concurrency("17").is_err());
        assert!(parse_concurrency("many").is_err());
    }

    #[test]
    fn builds_spotify_offset_page_urls_safely() {
        let url = spotify_offset_page_url("https://api.spotify.com/v1/me/tracks", 150).unwrap();
        let parameters: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();

        assert_eq!(parameters.get("limit").map(String::as_str), Some("50"));
        assert_eq!(parameters.get("offset").map(String::as_str), Some("150"));
    }

    #[test]
    fn prefers_spotify_retry_after_over_local_backoff() {
        assert_eq!(
            spotify_retry_delay(2, Some("7")),
            std::time::Duration::from_secs(7)
        );
        assert_eq!(
            spotify_retry_delay(2, None),
            std::time::Duration::from_millis(600)
        );
    }

    #[test]
    fn removes_terminal_control_characters() {
        assert_eq!(terminal_safe("Playlist\n\u{1b}[31m"), "Playlist  [31m");
    }
}
