use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use clap::{Parser, Subcommand, ValueEnum};
use futures_util::{StreamExt, stream};
use inquire::{Confirm, MultiSelect, Select, Text};
use rand::{RngExt, distr::Alphanumeric};
use reqwest::Client;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use url::Url;

mod cache;
mod matching;
mod model;
mod tidal;
mod tidal_import;
mod tidal_user;

use cache::TidalSearchCache;
use matching::{failed_match, fallback_search_queries, match_candidates, search_query};
use model::{
    ExportedPlaylistMetadata, MatchReport, MatchResult, MatchStatus, MatchSummary,
    ReviewChoiceCache, ReviewChoiceCacheEntry, ReviewDecision, ReviewDecisionAction,
    ReviewDecisionReport, ReviewReport, SkippedPlaylistItem, SourceTrack, SpotifyPlaylistExport,
    TidalTrackCandidate,
};

const SPOTIFY_AUTHORIZE_URL: &str = "https://accounts.spotify.com/authorize";
const SPOTIFY_TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const SPOTIFY_API_URL: &str = "https://api.spotify.com/v1";
const TOKEN_PATH: &str = "data/spotify-token.json";
const REVIEW_CHOICE_CACHE_PATH: &str = "data/tidal-review-choice-cache.json";
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

        /// Maximum sustained TIDAL search starts per second.
        #[arg(long, value_parser = tidal::parse_rate_limit)]
        rate_limit: Option<f64>,

        /// Ignore cached TIDAL searches and replace them with fresh responses.
        #[arg(long)]
        refresh_cache: bool,

        /// Retry Review/Missing tracks using cached album-context and title-only searches.
        #[arg(long)]
        fallback_searches: bool,
    },

    /// Select, export, match, review, and import Spotify sources in one invocation.
    Migrate {
        /// Maximum simultaneous Spotify page and TIDAL catalog requests.
        #[arg(long, default_value_t = 4, value_parser = parse_concurrency)]
        concurrency: usize,

        /// Maximum sustained TIDAL catalog search starts per second.
        #[arg(long, value_parser = tidal::parse_rate_limit)]
        rate_limit: Option<f64>,

        /// Ignore cached TIDAL searches and replace them with fresh responses.
        #[arg(long)]
        refresh_cache: bool,

        /// Retry Review/Missing tracks using album-context and title-only searches.
        #[arg(long)]
        fallback_searches: bool,

        /// Complete the workflow without creating or modifying a TIDAL playlist.
        #[arg(long, conflicts_with = "apply")]
        dry_run: bool,

        /// Explicitly create and populate the selected TIDAL playlists.
        #[arg(long, conflicts_with = "dry_run")]
        apply: bool,

        /// Include matches classified as Probable.
        #[arg(long)]
        include_probable: bool,

        /// Interactively review and include explicitly selected Review matches.
        #[arg(long)]
        include_review: bool,
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

        /// Maximum sustained TIDAL search starts per second.
        #[arg(long, value_parser = tidal::parse_rate_limit)]
        rate_limit: Option<f64>,

        /// Ignore cached TIDAL searches and replace them with fresh responses.
        #[arg(long)]
        refresh_cache: bool,

        /// Retry Review/Missing tracks using cached album-context and title-only searches.
        #[arg(long)]
        fallback_searches: bool,
    },

    /// Verify TIDAL authentication and catalog access.
    TidalTest,

    /// Interactively resolve tracks classified as Review without modifying TIDAL.
    Review {
        /// Optional review-report paths; omit to select reports discovered under data/.
        inputs: Vec<PathBuf>,
    },

    /// Validate or apply a TIDAL playlist import from a match report.
    ImportTidal {
        /// Match-report JSON created by match-tidal.
        input: PathBuf,

        /// Override the source Spotify playlist name.
        #[arg(long)]
        name: Option<String>,

        /// Override the destination playlist description.
        #[arg(long)]
        description: Option<String>,

        /// Validate and write an import plan without mutating TIDAL.
        #[arg(long, conflicts_with = "apply")]
        dry_run: bool,

        /// Explicitly create and populate one new TIDAL playlist.
        #[arg(long, conflicts_with = "dry_run")]
        apply: bool,

        /// Include manually selected Review decisions.
        #[arg(long)]
        include_review: bool,

        /// Include matches classified as Probable.
        #[arg(long)]
        include_probable: bool,

        /// Resume a compatible partially completed import.
        #[arg(long, requires = "apply")]
        resume: bool,

        /// Optional import report destination.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
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
        Command::SelectSpotify {
            concurrency,
            rate_limit,
            refresh_cache,
            fallback_searches,
        } => {
            select_spotify_playlists(concurrency, rate_limit, refresh_cache, fallback_searches)
                .await
        }
        Command::Migrate {
            concurrency,
            rate_limit,
            refresh_cache,
            fallback_searches,
            dry_run,
            apply,
            include_probable,
            include_review,
        } => {
            migrate_spotify_sources(MigrateOptions {
                concurrency,
                rate_limit,
                refresh_cache,
                fallback_searches,
                dry_run,
                apply,
                include_probable,
                include_review,
            })
            .await
        }
        Command::MatchTidal {
            input,
            limit,
            output,
            concurrency,
            rate_limit,
            refresh_cache,
            fallback_searches,
        } => {
            match_tidal_playlist(
                &input,
                limit,
                output,
                concurrency,
                rate_limit,
                refresh_cache,
                fallback_searches,
            )
            .await
        }
        Command::TidalTest => tidal::test_catalog().await,
        Command::Review { inputs } => review_tracks(&inputs).await,
        Command::ImportTidal {
            input,
            name,
            description,
            dry_run,
            apply,
            include_review,
            include_probable,
            resume,
            output,
        } => tidal_import::run_import(
            &input,
            tidal_import::ImportCommandOptions {
                name,
                description,
                dry_run,
                apply,
                include_review,
                include_probable,
                resume,
                output,
            },
        )
        .await
        .map(|_| ()),
    }
}

async fn select_spotify_playlists(
    concurrency: usize,
    rate_limit: Option<f64>,
    refresh_cache: bool,
    fallback_searches: bool,
) -> Result<()> {
    let summaries =
        prepare_spotify_sources(concurrency, rate_limit, refresh_cache, fallback_searches).await?;
    if !summaries.is_empty() {
        println!("No TIDAL import was performed.");
    }
    Ok(())
}

#[derive(Debug)]
struct MigrateOptions {
    concurrency: usize,
    rate_limit: Option<f64>,
    refresh_cache: bool,
    fallback_searches: bool,
    dry_run: bool,
    apply: bool,
    include_probable: bool,
    include_review: bool,
}

async fn migrate_spotify_sources(options: MigrateOptions) -> Result<()> {
    if options.apply && options.dry_run {
        bail!("--apply and --dry-run cannot be used together");
    }

    println!(
        "Full migration mode: {}",
        if options.apply {
            "apply"
        } else {
            "dry run (no TIDAL mutations)"
        }
    );
    if options.apply {
        println!("Validating TIDAL user authorization before matching...");
        let user_client = tidal_user::TidalUserClient::from_env().await?;
        user_client.require_scope("playlists.write")?;
        println!("TIDAL playlist-write authorization is ready.");
    }
    let summaries = prepare_spotify_sources(
        options.concurrency,
        options.rate_limit,
        options.refresh_cache,
        options.fallback_searches,
    )
    .await?;
    if summaries.is_empty() {
        return Ok(());
    }

    if options.include_review {
        let mut review_tidal_client = None;
        for summary in &summaries {
            if summary.failure.is_some() || summary.review == 0 {
                continue;
            }
            let review_path = summary
                .review_report_path
                .as_deref()
                .context("A matched source has no Review report path")?;
            let outcome = review_one_report(review_path, &mut review_tidal_client).await?;
            if outcome.cancelled {
                bail!(
                    "Migration cancelled during Review; no TIDAL import was started. \
                     Saved decisions can be continued with `cargo run -- review`."
                );
            }
        }
    }

    println!();
    println!("Starting TIDAL import stage...");
    let mut imported_sources = 0_usize;
    let mut failed_sources = Vec::new();
    let mut skipped_tracks = Vec::new();
    for summary in &summaries {
        let Some(match_report) = summary.report_path.as_deref() else {
            if summary.failure.is_some() {
                failed_sources.push(summary.source_name.clone());
            }
            continue;
        };

        println!();
        println!("Importing source: {}", terminal_safe(&summary.source_name));
        let outcome = tidal_import::run_import(
            match_report,
            tidal_import::ImportCommandOptions {
                name: None,
                description: None,
                dry_run: options.dry_run,
                apply: options.apply,
                include_review: options.include_review,
                include_probable: options.include_probable,
                resume: false,
                output: None,
            },
        )
        .await
        .with_context(|| {
            format!(
                "Could not import Spotify source {}",
                terminal_safe(&summary.source_name)
            )
        })?;
        skipped_tracks.extend(
            outcome
                .skipped_tracks
                .into_iter()
                .map(|track| (outcome.source_playlist_name.clone(), track)),
        );
        imported_sources += 1;
    }

    println!();
    println!(
        "Full migration flow completed for {imported_sources} source(s) in {} mode.",
        if options.apply { "apply" } else { "dry-run" }
    );
    print_migration_skipped_tracks(&skipped_tracks);
    if !failed_sources.is_empty() {
        bail!(
            "{} Spotify source(s) failed before import: {}",
            failed_sources.len(),
            failed_sources.join(", ")
        );
    }
    Ok(())
}

fn print_migration_skipped_tracks(skipped_tracks: &[(String, tidal_import::SkippedImportTrack)]) {
    println!();
    if skipped_tracks.is_empty() {
        println!("Skipped songs: none.");
        return;
    }

    println!(
        "Skipped songs for manual follow-up ({}):",
        skipped_tracks.len()
    );
    for line in migration_skipped_track_lines(skipped_tracks) {
        println!("  {line}");
    }
}

fn migration_skipped_track_lines(
    skipped_tracks: &[(String, tidal_import::SkippedImportTrack)],
) -> Vec<String> {
    skipped_tracks
        .iter()
        .map(|(playlist, track)| {
            let artists = if track.spotify_artists.is_empty() {
                "Unknown artist".to_owned()
            } else {
                track.spotify_artists.join(", ")
            };
            let album = track.spotify_album.as_deref().unwrap_or("Unknown album");
            format!(
                "[{}] #{} {} — {} | album: {} | {} | {}",
                terminal_safe(playlist),
                track.source_position,
                terminal_safe(&track.spotify_title),
                terminal_safe(&artists),
                terminal_safe(album),
                track.source_match_status,
                terminal_safe(&track.reason)
            )
        })
        .collect()
}

async fn prepare_spotify_sources(
    concurrency: usize,
    rate_limit: Option<f64>,
    refresh_cache: bool,
    fallback_searches: bool,
) -> Result<Vec<SelectionMatchSummary>> {
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
        .with_help_message(
            "Space: toggle | Right: select all | Left: clear all | Enter: confirm | Esc: cancel",
        )
        .with_page_size(15)
        .prompt_skippable()
        .context("Could not read the Spotify playlist selection")?;

    let Some(selected) = selected else {
        println!("Playlist selection cancelled.");
        return Ok(Vec::new());
    };

    if selected.is_empty() {
        println!("No playlists selected.");
        return Ok(Vec::new());
    }

    println!();
    let selected_count = selected.len();
    println!("Selected {selected_count} source(s).");
    println!("Authenticating with TIDAL for read-only catalog matching...");
    let tidal_client = tidal::TidalClient::from_env_with_rate_limit(rate_limit).await?;
    println!("TIDAL authentication succeeded.");
    println!(
        "Sustained TIDAL request rate: {:.2}/second",
        tidal_client.request_rate_limit()
    );
    let search_cache = TidalSearchCache::load_default()?;
    println!(
        "TIDAL cache: {} entries in {}",
        search_cache.len()?,
        search_cache.path().display()
    );

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
                    &tidal_client,
                    &search_cache,
                    MatchRunOptions {
                        limit: None,
                        output: None,
                        concurrency,
                        refresh_cache,
                        fallback_searches,
                    },
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
    Ok(summaries)
}

#[derive(Debug, Clone)]
struct ReviewReportOption {
    path: PathBuf,
    playlist_name: String,
    review_count: usize,
}

impl std::fmt::Display for ReviewReportOption {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} — {} tracks to review — {}",
            terminal_safe(&self.playlist_name),
            self.review_count,
            self.path.display()
        )
    }
}

#[derive(Debug, Clone)]
enum TidalReviewChoice {
    Candidate {
        candidate: TidalTrackCandidate,
        rank: usize,
        score: u8,
        suggested: bool,
        version_conflict: bool,
    },
    ManualTrackId,
    Skip,
    Finish,
}

impl std::fmt::Display for TidalReviewChoice {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ManualTrackId => {
                write!(
                    formatter,
                    "Enter a TIDAL track ID — resolve an exact catalog item"
                )
            }
            Self::Skip => write!(formatter, "Skip — exclude this track from a future import"),
            Self::Finish => write!(
                formatter,
                "Finish review — inspect the decision summary before saving"
            ),
            Self::Candidate {
                candidate,
                score,
                suggested,
                version_conflict,
                ..
            } => {
                let artists = if candidate.artists.is_empty() {
                    "Unknown artist".to_owned()
                } else {
                    candidate.artists.join(", ")
                };
                let album = candidate.album.as_deref().unwrap_or("Unknown album");
                let duration = candidate
                    .duration_ms
                    .map_or_else(|| "unknown duration".to_owned(), format_duration);
                let explicit = match candidate.explicit {
                    Some(true) => "explicit",
                    Some(false) => "clean",
                    None => "explicitness unknown",
                };
                let isrc = candidate.isrc.as_deref().unwrap_or("no ISRC");
                let version = candidate
                    .version
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                    .map_or_else(String::new, |value| {
                        format!(" | version: {}", terminal_safe(value))
                    });
                let suggested = if *suggested { " | suggested" } else { "" };
                let conflict = if *version_conflict {
                    " | VERSION CONFLICT"
                } else {
                    ""
                };

                write!(
                    formatter,
                    "[{score}%] {} — {} | album: {} | {} | {} | ISRC: {}{}{}{}",
                    terminal_safe(&candidate.title),
                    terminal_safe(&artists),
                    terminal_safe(album),
                    duration,
                    explicit,
                    terminal_safe(isrc),
                    version,
                    suggested,
                    conflict
                )
            }
        }
    }
}

async fn review_tracks(inputs: &[PathBuf]) -> Result<()> {
    let available = if inputs.is_empty() {
        discover_review_reports(Path::new("data"))?
    } else {
        inputs
            .iter()
            .map(|path| review_report_option(path))
            .collect::<Result<Vec<_>>>()?
    };

    if available.is_empty() {
        println!(
            "No playlists with Review tracks were found. Run `cargo run -- match-tidal <export.json>` first."
        );
        return Ok(());
    }

    let selected = if inputs.is_empty() {
        MultiSelect::new(
            "Select playlists whose Review tracks you want to resolve:",
            available,
        )
        .with_help_message(
            "Space: toggle | Right: select all | Left: clear all | Enter: confirm | Esc: cancel",
        )
        .with_page_size(15)
        .prompt_skippable()
        .context("Could not read the review-report selection")?
    } else {
        Some(available)
    };

    let Some(selected) = selected else {
        println!("Review cancelled.");
        return Ok(());
    };
    if selected.is_empty() {
        println!("No review reports selected.");
        return Ok(());
    }

    let mut reviewed = 0;
    let mut skipped = 0;
    let mut tidal_client = None;
    for option in selected {
        let outcome = review_one_report(&option.path, &mut tidal_client).await?;
        reviewed += outcome.selected;
        skipped += outcome.skipped;
        if outcome.cancelled {
            break;
        }
    }

    println!();
    println!("Review session completed.");
    println!("TIDAL candidates selected: {reviewed}");
    println!("Tracks explicitly skipped: {skipped}");
    println!("No TIDAL playlist or library changes were made.");
    Ok(())
}

#[derive(Debug, Default)]
struct ReviewSessionOutcome {
    selected: usize,
    skipped: usize,
    cancelled: bool,
}

async fn review_one_report(
    path: &Path,
    tidal_client: &mut Option<tidal::TidalClient>,
) -> Result<ReviewSessionOutcome> {
    let review_report: ReviewReport = read_json(path, "review report")?;
    if review_report.schema_version != 1 {
        bail!(
            "Unsupported review report schema version {} in {}; expected version 1",
            review_report.schema_version,
            path.display()
        );
    }

    let match_report_path = PathBuf::from(&review_report.source_match_report);
    let match_report: MatchReport =
        read_json(&match_report_path, "match report").with_context(|| {
            format!(
                "The review report {} references {}",
                path.display(),
                match_report_path.display()
            )
        })?;
    if match_report.schema_version != 1 {
        bail!(
            "Unsupported match report schema version {} in {}; expected version 1",
            match_report.schema_version,
            match_report_path.display()
        );
    }
    if match_report.source_playlist.spotify_id != review_report.source_playlist.spotify_id {
        bail!(
            "Review report {} and match report {} refer to different Spotify playlists",
            path.display(),
            match_report_path.display()
        );
    }

    let review_results: Vec<_> = match_report
        .results
        .iter()
        .filter(|result| result.is_reviewable())
        .collect();
    let decision_path = default_review_decisions_path(path);
    let mut decisions = load_existing_review_decisions(
        &decision_path,
        &match_report.source_playlist.spotify_id,
        match_report.generated_at_unix,
    )?;
    let mut shared_choices = load_review_choice_cache(Path::new(REVIEW_CHOICE_CACHE_PATH))?;
    let reused_choices = apply_cached_review_choices(
        &match_report.country_code,
        &review_results,
        &mut decisions,
        &shared_choices,
    );

    println!();
    println!(
        "Playlist: {} — {} tracks to review",
        terminal_safe(&match_report.source_playlist.name),
        review_results.len()
    );
    if reused_choices > 0 {
        println!(
            "Reused {reused_choices} previously confirmed selection(s) from {REVIEW_CHOICE_CACHE_PATH}."
        );
    }

    let mut index = 0_usize;
    let mut prompt_existing_decision = false;
    let mut stopped_early = false;
    loop {
        while index < review_results.len() {
            let result = review_results[index];
            let track = &result.spotify_track;
            if !prompt_existing_decision && decisions.contains_key(&track.position) {
                index += 1;
                continue;
            }
            prompt_existing_decision = false;

            let artists = if track.artists.is_empty() {
                "Unknown artist".to_owned()
            } else {
                track.artists.join(", ")
            };

            println!();
            println!("Review {}/{}", index + 1, review_results.len());
            println!(
                "Spotify: {} — {}",
                terminal_safe(&track.title),
                terminal_safe(&artists)
            );
            println!(
                "Album: {}",
                terminal_safe(track.album.as_deref().unwrap_or("Unknown album"))
            );
            println!(
                "Duration: {} | {} | ISRC: {}",
                format_duration(track.duration_ms),
                if track.explicit { "explicit" } else { "clean" },
                terminal_safe(track.isrc.as_deref().unwrap_or("no ISRC"))
            );
            println!(
                "Machine match: {}",
                result
                    .score
                    .map_or_else(|| "no score".to_owned(), |score| format!("{score}%"))
            );
            if result.status == MatchStatus::Missing {
                println!("No acceptable automatic TIDAL match; enter a track ID manually or Skip.");
            }
            for reason in &result.reasons {
                println!("  - {}", terminal_safe(reason));
            }

            let choices = tidal_review_choices(result);
            let starting_cursor = existing_review_cursor(&choices, decisions.get(&track.position));
            let prompt = format!(
                "Choose the TIDAL track for \"{}\":",
                terminal_safe(&track.title)
            );
            let selection = Select::new(&prompt, choices)
            .with_starting_cursor(starting_cursor)
            .with_page_size(8)
            .with_help_message(
                    "Enter: choose | Esc: previous song (first song: cancel) | Finish: review summary",
            )
            .prompt_skippable()
            .context("Could not read the TIDAL candidate selection")?;

            let Some(selection) = selection else {
                if index > 0 {
                    index -= 1;
                    prompt_existing_decision = true;
                    println!("Returning to the previous song.");
                    continue;
                }
                println!("Review cancelled; no new decisions were saved.");
                return Ok(ReviewSessionOutcome {
                    cancelled: true,
                    ..ReviewSessionOutcome::default()
                });
            };

            let decision = match selection {
                TidalReviewChoice::Finish => {
                    stopped_early = true;
                    break;
                }
                TidalReviewChoice::ManualTrackId => {
                    let Some(candidate) = prompt_manual_tidal_track(track, tidal_client).await?
                    else {
                        continue;
                    };
                    decision_from_manual_candidate(track, candidate)
                }
                choice => decision_from_choice(track, choice)
                    .context("A review navigation choice cannot be saved as a track decision")?,
            };
            match decision.action {
                ReviewDecisionAction::Selected => println!(
                    "Selected TIDAL track {}.",
                    decision
                        .selected_tidal_candidate
                        .as_ref()
                        .map_or("unknown", |candidate| candidate.tidal_id.as_str())
                ),
                ReviewDecisionAction::Skipped => println!("Track marked to skip."),
            }
            decisions.insert(track.position, decision);
            index += 1;
        }

        print_review_decision_summary(&review_results, &decisions);
        let prompt = if stopped_early {
            "Save these partial review decisions?"
        } else {
            "Confirm and save these review decisions?"
        };
        if Confirm::new(prompt)
            .with_default(false)
            .prompt()
            .context("Could not read the review confirmation")?
        {
            break;
        }

        index = review_results
            .iter()
            .rposition(|result| decisions.contains_key(&result.spotify_track.position))
            .unwrap_or(0);
        prompt_existing_decision = true;
        stopped_early = false;
        println!(
            "Returning to review {}/{}. Press Esc to continue moving backward.",
            index + 1,
            review_results.len()
        );
    }

    update_review_choice_cache(
        &match_report.country_code,
        &review_results,
        &decisions,
        &mut shared_choices,
    )?;
    let decisions: Vec<_> = decisions.into_values().collect();
    let selected_count = decisions
        .iter()
        .filter(|decision| decision.action == ReviewDecisionAction::Selected)
        .count();
    let skipped_count = decisions
        .iter()
        .filter(|decision| decision.action == ReviewDecisionAction::Skipped)
        .count();
    let decision_report = ReviewDecisionReport {
        schema_version: 1,
        generated_at_unix: current_unix_timestamp()?,
        source_playlist: match_report.source_playlist.clone(),
        source_match_report: match_report_path.display().to_string(),
        source_match_generated_at_unix: match_report.generated_at_unix,
        source_review_report: path.display().to_string(),
        selected_count,
        skipped_count,
        decisions,
    };
    write_json(&decision_path, &decision_report)?;
    save_review_choice_cache(Path::new(REVIEW_CHOICE_CACHE_PATH), shared_choices)?;

    println!("Review decisions: {}", decision_path.display());
    println!("Reusable selections: {REVIEW_CHOICE_CACHE_PATH}");
    Ok(ReviewSessionOutcome {
        selected: selected_count,
        skipped: skipped_count,
        cancelled: false,
    })
}

fn discover_review_reports(directory: &Path) -> Result<Vec<ReviewReportOption>> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("Could not read {}", directory.display()));
        }
    };

    let mut paths = Vec::new();
    for entry in entries {
        let entry =
            entry.with_context(|| format!("Could not read an entry in {}", directory.display()))?;
        let path = entry.path();
        if path.is_file()
            && path
                .file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|name| name.ends_with("-tidal-review.json"))
        {
            paths.push(path);
        }
    }
    paths.sort();

    let mut reports = Vec::new();
    for path in paths {
        let option = review_report_option(&path)?;
        if option.review_count > 0 {
            reports.push(option);
        }
    }
    Ok(reports)
}

fn review_report_option(path: &Path) -> Result<ReviewReportOption> {
    let report: ReviewReport = read_json(path, "review report")?;
    if report.schema_version != 1 {
        bail!(
            "Unsupported review report schema version {} in {}; expected version 1",
            report.schema_version,
            path.display()
        );
    }

    let match_report_path = PathBuf::from(&report.source_match_report);
    let match_report: MatchReport =
        read_json(&match_report_path, "match report").with_context(|| {
            format!(
                "The review report {} references {}",
                path.display(),
                match_report_path.display()
            )
        })?;
    if match_report.source_playlist.spotify_id != report.source_playlist.spotify_id {
        bail!(
            "Review report {} and match report {} refer to different Spotify playlists",
            path.display(),
            match_report_path.display()
        );
    }
    let review_count = match_report
        .results
        .iter()
        .filter(|result| result.is_reviewable())
        .count();

    Ok(ReviewReportOption {
        path: path.to_owned(),
        playlist_name: report.source_playlist.name,
        review_count,
    })
}

fn read_json<T>(path: &Path, description: &str) -> Result<T>
where
    T: DeserializeOwned,
{
    let bytes = fs::read(path)
        .with_context(|| format!("Could not read {description} {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("Invalid {description} JSON in {}", path.display()))
}

fn tidal_review_choices(result: &MatchResult) -> Vec<TidalReviewChoice> {
    let mut choices = Vec::with_capacity(result.alternatives.len() + 4);
    let mut identifiers = HashSet::new();

    if result.status == MatchStatus::Review {
        if let Some(candidate) = &result.best_candidate {
            identifiers.insert(candidate.tidal_id.clone());
            choices.push(TidalReviewChoice::Candidate {
                candidate: candidate.clone(),
                rank: 1,
                score: result.score.unwrap_or_default(),
                suggested: true,
                version_conflict: result
                    .reasons
                    .iter()
                    .any(|reason| reason.to_ascii_lowercase().contains("conflict")),
            });
        }

        for (index, alternative) in result.alternatives.iter().enumerate() {
            if identifiers.insert(alternative.candidate.tidal_id.clone()) {
                choices.push(TidalReviewChoice::Candidate {
                    candidate: alternative.candidate.clone(),
                    rank: index + 2,
                    score: alternative.score,
                    suggested: false,
                    version_conflict: alternative.version_conflict,
                });
            }
        }
    }

    choices.push(TidalReviewChoice::ManualTrackId);
    choices.push(TidalReviewChoice::Skip);
    choices.push(TidalReviewChoice::Finish);
    choices
}

fn existing_review_cursor(
    choices: &[TidalReviewChoice],
    decision: Option<&ReviewDecision>,
) -> usize {
    let Some(decision) = decision else {
        return 0;
    };

    choices
        .iter()
        .position(|choice| match choice {
            TidalReviewChoice::ManualTrackId | TidalReviewChoice::Finish => false,
            TidalReviewChoice::Skip => decision.action == ReviewDecisionAction::Skipped,
            TidalReviewChoice::Candidate { candidate, .. } => decision
                .selected_tidal_candidate
                .as_ref()
                .is_some_and(|selected| selected.tidal_id == candidate.tidal_id),
        })
        .unwrap_or(0)
}

fn decision_from_choice(track: &SourceTrack, choice: TidalReviewChoice) -> Option<ReviewDecision> {
    let (action, rank, score, candidate) = match choice {
        TidalReviewChoice::Candidate {
            candidate,
            rank,
            score,
            ..
        } => (
            ReviewDecisionAction::Selected,
            Some(rank),
            Some(score),
            Some(candidate),
        ),
        TidalReviewChoice::ManualTrackId => return None,
        TidalReviewChoice::Skip => (ReviewDecisionAction::Skipped, None, None, None),
        TidalReviewChoice::Finish => return None,
    };

    Some(ReviewDecision {
        position: track.position,
        spotify_id: track.spotify_id.clone(),
        spotify_title: track.title.clone(),
        spotify_artists: track.artists.clone(),
        spotify_album: track.album.clone(),
        action,
        selected_candidate_rank: rank,
        selected_machine_match_percentage: score,
        selected_tidal_candidate: candidate,
    })
}

fn decision_from_manual_candidate(
    track: &SourceTrack,
    candidate: TidalTrackCandidate,
) -> ReviewDecision {
    ReviewDecision {
        position: track.position,
        spotify_id: track.spotify_id.clone(),
        spotify_title: track.title.clone(),
        spotify_artists: track.artists.clone(),
        spotify_album: track.album.clone(),
        action: ReviewDecisionAction::Selected,
        selected_candidate_rank: None,
        selected_machine_match_percentage: None,
        selected_tidal_candidate: Some(candidate),
    }
}

async fn prompt_manual_tidal_track(
    source_track: &SourceTrack,
    tidal_client: &mut Option<tidal::TidalClient>,
) -> Result<Option<TidalTrackCandidate>> {
    let track_id = Text::new("Enter the TIDAL track ID:")
        .with_help_message("Enter: resolve ID using the official TIDAL catalog | Esc: go back")
        .prompt_skippable()
        .context("Could not read the manual TIDAL track ID")?;
    let Some(track_id) = track_id else {
        return Ok(None);
    };
    let track_id = track_id.trim();
    if track_id.is_empty() {
        eprintln!("TIDAL track ID cannot be empty.");
        return Ok(None);
    }

    if tidal_client.is_none() {
        *tidal_client = Some(tidal::TidalClient::from_env().await?);
    }
    let candidate = match tidal_client
        .as_ref()
        .context("TIDAL catalog client was not initialized")?
        .track_by_id(track_id)
        .await
    {
        Ok(candidate) => candidate,
        Err(error) => {
            eprintln!("Could not resolve TIDAL track ID `{track_id}`: {error}");
            return Ok(None);
        }
    };

    let artists = if candidate.artists.is_empty() {
        "Unknown artist".to_owned()
    } else {
        candidate.artists.join(", ")
    };
    println!();
    println!(
        "Resolved TIDAL track: {} — {}",
        terminal_safe(&candidate.title),
        terminal_safe(&artists)
    );
    println!(
        "Album: {} | Duration: {} | ISRC: {} | {}",
        terminal_safe(candidate.album.as_deref().unwrap_or("Unknown album")),
        candidate
            .duration_ms
            .map_or_else(|| "unknown".to_owned(), format_duration),
        terminal_safe(candidate.isrc.as_deref().unwrap_or("no ISRC")),
        match candidate.explicit {
            Some(true) => "explicit",
            Some(false) => "clean",
            None => "explicitness unknown",
        }
    );
    let source_artists = if source_track.artists.is_empty() {
        "Unknown artist".to_owned()
    } else {
        source_track.artists.join(", ")
    };
    let confirmation = format!(
        "Use TIDAL track {} for Spotify \"{}\" — {}?",
        terminal_safe(&candidate.tidal_id),
        terminal_safe(&source_track.title),
        terminal_safe(&source_artists)
    );
    if !Confirm::new(&confirmation)
        .with_default(false)
        .prompt()
        .context("Could not read the manual track confirmation")?
    {
        return Ok(None);
    }

    Ok(Some(candidate))
}

fn load_existing_review_decisions(
    path: &Path,
    spotify_playlist_id: &str,
    source_match_generated_at_unix: u64,
) -> Result<BTreeMap<usize, ReviewDecision>> {
    let report: ReviewDecisionReport = match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("Invalid review decisions JSON in {}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("Could not read {}", path.display()));
        }
    };

    if report.schema_version != 1 {
        bail!(
            "Unsupported review decisions schema version {} in {}; expected version 1",
            report.schema_version,
            path.display()
        );
    }
    if report.source_playlist.spotify_id != spotify_playlist_id {
        bail!(
            "Review decisions {} belong to a different Spotify playlist",
            path.display()
        );
    }
    if report.source_match_generated_at_unix != source_match_generated_at_unix {
        eprintln!(
            "Ignoring stale review decisions in {} because the match report was regenerated.",
            path.display()
        );
        return Ok(BTreeMap::new());
    }

    Ok(report
        .decisions
        .into_iter()
        .map(|decision| (decision.position, decision))
        .collect())
}

type ReviewChoiceCacheMap = BTreeMap<(String, String), ReviewChoiceCacheEntry>;

fn review_choice_cache_key(country_code: &str, spotify_id: &str) -> (String, String) {
    (
        country_code.trim().to_ascii_uppercase(),
        spotify_id.trim().to_owned(),
    )
}

fn load_review_choice_cache(path: &Path) -> Result<ReviewChoiceCacheMap> {
    let report: ReviewChoiceCache = match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("Invalid review choice cache JSON in {}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BTreeMap::new());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("Could not read {}", path.display()));
        }
    };

    if report.schema_version != 1 {
        bail!(
            "Unsupported review choice cache schema version {} in {}; expected version 1",
            report.schema_version,
            path.display()
        );
    }

    let mut choices = BTreeMap::new();
    for choice in report.choices {
        if choice.country_code.trim().is_empty()
            || choice.spotify_id.trim().is_empty()
            || choice.selected_tidal_candidate.tidal_id.trim().is_empty()
        {
            bail!(
                "Review choice cache {} contains an entry with an empty country, Spotify ID, or TIDAL ID",
                path.display()
            );
        }
        let key = review_choice_cache_key(&choice.country_code, &choice.spotify_id);
        if choices.insert(key, choice).is_some() {
            bail!(
                "Review choice cache {} contains a duplicate entry",
                path.display()
            );
        }
    }
    Ok(choices)
}

fn save_review_choice_cache(path: &Path, choices: ReviewChoiceCacheMap) -> Result<()> {
    let report = ReviewChoiceCache {
        schema_version: 1,
        updated_at_unix: current_unix_timestamp()?,
        choices: choices.into_values().collect(),
    };
    write_json(path, &report)
}

fn apply_cached_review_choices(
    country_code: &str,
    review_results: &[&MatchResult],
    decisions: &mut BTreeMap<usize, ReviewDecision>,
    choices: &ReviewChoiceCacheMap,
) -> usize {
    let mut reused = 0;
    for result in review_results {
        let track = &result.spotify_track;
        if decisions.contains_key(&track.position) {
            continue;
        }
        let Some(spotify_id) = track.spotify_id.as_deref() else {
            continue;
        };
        let Some(choice) = choices.get(&review_choice_cache_key(country_code, spotify_id)) else {
            continue;
        };
        decisions.insert(
            track.position,
            ReviewDecision {
                position: track.position,
                spotify_id: track.spotify_id.clone(),
                spotify_title: track.title.clone(),
                spotify_artists: track.artists.clone(),
                spotify_album: track.album.clone(),
                action: ReviewDecisionAction::Selected,
                selected_candidate_rank: None,
                selected_machine_match_percentage: choice.selected_machine_match_percentage,
                selected_tidal_candidate: Some(choice.selected_tidal_candidate.clone()),
            },
        );
        reused += 1;
    }
    reused
}

fn update_review_choice_cache(
    country_code: &str,
    review_results: &[&MatchResult],
    decisions: &BTreeMap<usize, ReviewDecision>,
    choices: &mut ReviewChoiceCacheMap,
) -> Result<()> {
    for result in review_results {
        let track = &result.spotify_track;
        let Some(spotify_id) = track.spotify_id.as_deref() else {
            continue;
        };
        let key = review_choice_cache_key(country_code, spotify_id);
        let Some(decision) = decisions.get(&track.position) else {
            continue;
        };
        match decision.action {
            ReviewDecisionAction::Selected => {
                let candidate = decision
                    .selected_tidal_candidate
                    .clone()
                    .context("A selected review decision has no TIDAL candidate")?;
                choices.insert(
                    key,
                    ReviewChoiceCacheEntry {
                        country_code: country_code.trim().to_ascii_uppercase(),
                        spotify_id: spotify_id.to_owned(),
                        spotify_title: track.title.clone(),
                        spotify_artists: track.artists.clone(),
                        spotify_album: track.album.clone(),
                        selected_machine_match_percentage: decision
                            .selected_machine_match_percentage,
                        selected_tidal_candidate: candidate,
                    },
                );
            }
            ReviewDecisionAction::Skipped => {
                choices.remove(&key);
            }
        }
    }
    Ok(())
}

fn print_review_decision_summary(
    review_results: &[&MatchResult],
    decisions: &BTreeMap<usize, ReviewDecision>,
) {
    println!();
    println!("Review decision summary:");
    for result in review_results {
        let track = &result.spotify_track;
        let artists = if track.artists.is_empty() {
            "Unknown artist".to_owned()
        } else {
            track.artists.join(", ")
        };
        match decisions.get(&track.position) {
            Some(decision) if decision.action == ReviewDecisionAction::Selected => {
                let candidate = decision.selected_tidal_candidate.as_ref();
                println!(
                    "  #{} {} — {} -> {} — {} [{}]",
                    track.position,
                    terminal_safe(&track.title),
                    terminal_safe(&artists),
                    terminal_safe(
                        candidate.map_or("Unknown TIDAL track", |item| item.title.as_str())
                    ),
                    terminal_safe(
                        candidate
                            .and_then(|item| item.artists.first())
                            .map_or("Unknown artist", String::as_str)
                    ),
                    terminal_safe(candidate.map_or("unknown ID", |item| item.tidal_id.as_str()))
                );
            }
            Some(_) => println!(
                "  #{} {} — {} -> SKIP",
                track.position,
                terminal_safe(&track.title),
                terminal_safe(&artists)
            ),
            None => println!(
                "  #{} {} — {} -> UNRESOLVED",
                track.position,
                terminal_safe(&track.title),
                terminal_safe(&artists)
            ),
        }
    }
}

fn format_duration(duration_ms: u64) -> String {
    let total_seconds = duration_ms / 1_000;
    format!("{}:{:02}", total_seconds / 60, total_seconds % 60)
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
    cache_hits: usize,
    cache_misses: usize,
    fallback_queries: usize,
    non_exact_tracks: Vec<String>,
    report_path: Option<PathBuf>,
    review_report_path: Option<PathBuf>,
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
            cache_hits: 0,
            cache_misses: 0,
            fallback_queries: 0,
            non_exact_tracks: Vec::new(),
            report_path: None,
            review_report_path: None,
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
    cache_hits: usize,
    cache_misses: usize,
    fallback_queries: usize,
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
        totals.cache_hits += summary.cache_hits;
        totals.cache_misses += summary.cache_misses;
        totals.fallback_queries += summary.fallback_queries;
        totals.failed_sources += usize::from(summary.failure.is_some());
    }

    totals
}

fn non_exact_track_lines(results: &[MatchResult]) -> Vec<String> {
    results
        .iter()
        .filter(|result| result.status != MatchStatus::Exact)
        .map(|result| {
            let track = &result.spotify_track;
            let artist = track
                .artists
                .first()
                .map_or("Unknown artist", String::as_str);
            let score = result
                .score
                .map_or_else(|| "no score".to_owned(), |score| format!("{score}/100"));
            let error_marker = if result.error.is_some() {
                " [search error]"
            } else {
                ""
            };

            format!(
                "#{} {} — {} — {} ({score}){error_marker}",
                track.position,
                terminal_safe(&track.title),
                terminal_safe(artist),
                result.status
            )
        })
        .collect()
}

fn print_non_exact_tracks(summaries: &[SelectionMatchSummary]) {
    let total: usize = summaries
        .iter()
        .map(|summary| summary.non_exact_tracks.len())
        .sum();

    println!();
    println!("Non-exact tracks: {total}");
    if total == 0 {
        println!("All processed tracks were Exact.");
        return;
    }

    for summary in summaries {
        if summary.non_exact_tracks.is_empty() {
            continue;
        }

        println!();
        println!("- {}", terminal_safe(&summary.source_name));
        for line in &summary.non_exact_tracks {
            println!("  {line}");
        }
    }
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
        println!(
            "  Search cache hits: {} | misses: {} | fallback queries: {}",
            summary.cache_hits, summary.cache_misses, summary.fallback_queries
        );
        if let Some(path) = &summary.report_path {
            println!("  Match report: {}", path.display());
        }
        if let Some(path) = &summary.review_report_path {
            println!("  Review report: {}", path.display());
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
    println!(
        "Searches — Cache hits: {} | misses: {} | fallback queries: {}",
        totals.cache_hits, totals.cache_misses, totals.fallback_queries
    );
    if totals.failed_sources > 0 {
        println!(
            "Sources that could not be processed: {}",
            totals.failed_sources
        );
    }
    print_non_exact_tracks(summaries);
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
    rate_limit: Option<f64>,
    refresh_cache: bool,
    fallback_searches: bool,
) -> Result<()> {
    println!("Authenticating with TIDAL...");

    // Authentication happens exactly once; this client and its bearer token
    // are reused for every catalog request in the run.
    let tidal_client = tidal::TidalClient::from_env_with_rate_limit(rate_limit).await?;
    println!("TIDAL authentication succeeded.");
    println!("Country: {}", tidal_client.country_code());
    println!(
        "Sustained request rate: {:.2}/second",
        tidal_client.request_rate_limit()
    );
    let search_cache = TidalSearchCache::load_default()?;
    println!(
        "Cache: {} entries in {}",
        search_cache.len()?,
        search_cache.path().display()
    );
    println!();

    let summary = match_tidal_playlist_with_client(
        input,
        &tidal_client,
        &search_cache,
        MatchRunOptions {
            limit,
            output,
            concurrency,
            refresh_cache,
            fallback_searches,
        },
    )
    .await?;
    print_non_exact_tracks(std::slice::from_ref(&summary));
    Ok(())
}

struct CandidateSearchOutcome {
    candidates: Vec<TidalTrackCandidate>,
    cache_hit: bool,
}

struct TrackSearchOutcome {
    result: MatchResult,
    cache_hits: usize,
    cache_misses: usize,
    fallback_queries: usize,
}

async fn cached_tidal_candidates(
    query: &str,
    tidal_client: &tidal::TidalClient,
    search_cache: &TidalSearchCache,
    refresh_cache: bool,
) -> Result<CandidateSearchOutcome> {
    if !refresh_cache {
        match search_cache.get(tidal_client.country_code(), query) {
            Ok(Some(candidates)) => {
                return Ok(CandidateSearchOutcome {
                    candidates,
                    cache_hit: true,
                });
            }
            Ok(None) => {}
            Err(error) => {
                eprintln!("Could not read the TIDAL search cache: {error:#}");
            }
        }
    }

    let candidates = tidal_client.search_tracks_query(query).await?;
    if let Ok(timestamp) = current_unix_timestamp() {
        let cache_result = if refresh_cache {
            search_cache.replace(tidal_client.country_code(), query, &candidates, timestamp)
        } else {
            search_cache
                .insert(tidal_client.country_code(), query, &candidates, timestamp)
                .map(|_| ())
        };
        if let Err(error) = cache_result {
            eprintln!("Could not update the TIDAL search cache: {error:#}");
        }
    }

    Ok(CandidateSearchOutcome {
        candidates,
        cache_hit: false,
    })
}

fn merge_unique_candidates(
    candidates: &mut Vec<TidalTrackCandidate>,
    additional: Vec<TidalTrackCandidate>,
) {
    let mut identifiers: HashSet<String> = candidates
        .iter()
        .map(|candidate| candidate.tidal_id.clone())
        .collect();
    candidates.extend(
        additional
            .into_iter()
            .filter(|candidate| identifiers.insert(candidate.tidal_id.clone())),
    );
}

async fn match_track_with_searches(
    track: SourceTrack,
    tidal_client: &tidal::TidalClient,
    search_cache: &TidalSearchCache,
    refresh_cache: bool,
    enable_fallbacks: bool,
) -> TrackSearchOutcome {
    let primary_query = search_query(&track);
    let primary =
        match cached_tidal_candidates(&primary_query, tidal_client, search_cache, refresh_cache)
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                return TrackSearchOutcome {
                    result: failed_match(&track, primary_query, format!("{error:#}")),
                    cache_hits: 0,
                    cache_misses: 1,
                    fallback_queries: 0,
                };
            }
        };

    let mut cache_hits = usize::from(primary.cache_hit);
    let mut cache_misses = usize::from(!primary.cache_hit);
    let mut candidates = primary.candidates;
    let mut used_queries = vec![primary_query];
    let mut result = match_candidates(&track, used_queries[0].clone(), candidates.clone());
    let mut attempted_fallbacks = 0;

    if enable_fallbacks && matches!(result.status, MatchStatus::Review | MatchStatus::Missing) {
        for fallback_query in fallback_search_queries(&track) {
            attempted_fallbacks += 1;
            let fallback = match cached_tidal_candidates(
                &fallback_query,
                tidal_client,
                search_cache,
                refresh_cache,
            )
            .await
            {
                Ok(outcome) => outcome,
                Err(error) => {
                    cache_misses += 1;
                    eprintln!(
                        "Fallback search failed for {} — {}: {error:#}",
                        terminal_safe(&track.title),
                        terminal_safe(
                            track
                                .artists
                                .first()
                                .map_or("Unknown artist", String::as_str)
                        )
                    );
                    continue;
                }
            };

            cache_hits += usize::from(fallback.cache_hit);
            cache_misses += usize::from(!fallback.cache_hit);
            merge_unique_candidates(&mut candidates, fallback.candidates);
            used_queries.push(fallback_query);
            result = match_candidates(
                &track,
                used_queries.join(" | fallback: "),
                candidates.clone(),
            );

            if matches!(result.status, MatchStatus::Exact | MatchStatus::Probable) {
                break;
            }
        }
    }

    TrackSearchOutcome {
        result,
        cache_hits,
        cache_misses,
        fallback_queries: attempted_fallbacks,
    }
}

struct MatchRunOptions {
    limit: Option<usize>,
    output: Option<PathBuf>,
    concurrency: usize,
    refresh_cache: bool,
    fallback_searches: bool,
}

async fn match_tidal_playlist_with_client(
    input: &Path,
    tidal_client: &tidal::TidalClient,
    search_cache: &TidalSearchCache,
    options: MatchRunOptions,
) -> Result<SelectionMatchSummary> {
    let MatchRunOptions {
        limit,
        output,
        concurrency,
        refresh_cache,
        fallback_searches,
    } = options;
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
    if refresh_cache {
        println!("TIDAL cache refresh: enabled");
    }
    if fallback_searches {
        println!("Fallback searches: enabled for Review/Missing tracks");
    }

    let searches = stream::iter(export.tracks.iter().take(track_count).cloned().enumerate())
        .map(|(index, track)| async move {
            (
                index,
                match_track_with_searches(
                    track,
                    tidal_client,
                    search_cache,
                    refresh_cache,
                    fallback_searches,
                )
                .await,
            )
        })
        .buffer_unordered(concurrency);
    tokio::pin!(searches);

    let mut completed = Vec::with_capacity(track_count);
    let mut cache_hits = 0_usize;
    let mut cache_misses = 0_usize;
    let mut fallback_queries = 0_usize;
    while let Some((index, outcome)) = searches.next().await {
        cache_hits += outcome.cache_hits;
        cache_misses += outcome.cache_misses;
        fallback_queries += outcome.fallback_queries;
        let result = outcome.result;
        let completed_count = completed.len() + 1;
        let track = &result.spotify_track;
        let cache_marker = if outcome.cache_misses == 0 {
            " [cache]"
        } else if outcome.cache_hits > 0 {
            " [cache+network]"
        } else {
            ""
        };
        let fallback_marker = if outcome.fallback_queries > 0 {
            format!(" [fallback {}]", outcome.fallback_queries)
        } else {
            String::new()
        };
        match result.score {
            Some(score) => println!(
                "[{completed_count}/{track_count}] #{} {} — {}: {} ({score}/100){cache_marker}{fallback_marker}",
                index + 1,
                track.title,
                track
                    .artists
                    .first()
                    .map_or("Unknown artist", String::as_str),
                result.status
            ),
            None => println!(
                "[{completed_count}/{track_count}] #{} {} — {}: {}{cache_marker}{fallback_marker}",
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
    let review_destination = default_review_report_path(&destination);
    let review_report = ReviewReport::from_match_report(&report, destination.display().to_string());
    write_json(&review_destination, &review_report)?;

    println!();
    println!("Playlist: {}", report.source_playlist.name);
    println!("Processed: {}", report.processed_tracks);
    println!("Exact: {}", report.summary.exact);
    println!("Probable: {}", report.summary.probable);
    println!("Review: {}", report.summary.review);
    println!("Missing: {}", report.summary.missing);
    println!("Search cache hits: {cache_hits}");
    println!("Search cache misses: {cache_misses}");
    println!("Fallback queries: {fallback_queries}");
    println!("Match report: {}", destination.display());
    println!(
        "Review report: {} ({} tracks)",
        review_destination.display(),
        review_report.review_tracks.len()
    );

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
        cache_hits,
        cache_misses,
        fallback_queries,
        non_exact_tracks: non_exact_track_lines(&report.results),
        report_path: Some(destination),
        review_report_path: Some(review_destination),
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

fn default_review_report_path(match_report: &Path) -> PathBuf {
    let stem = match_report
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("tidal-matches");
    let base = stem.strip_suffix("-tidal-matches").unwrap_or(stem);
    let filename = format!("{base}-tidal-review.json");

    match_report
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("data"))
        .join(filename)
}

fn default_review_decisions_path(review_report: &Path) -> PathBuf {
    let stem = review_report
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("tidal-review");
    let filename = format!("{stem}-decisions.json");

    review_report
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
    use std::{
        collections::BTreeMap,
        path::{Path, PathBuf},
    };

    use clap::Parser;

    use super::{
        Cli, Command, SelectionMatchSummary, SelectionMatchTotals, SpotifyPlaylistsPage,
        SpotifySavedTracksPage, SpotifySelectionOption, TidalReviewChoice,
        apply_cached_review_choices, decision_from_choice, decision_from_manual_candidate,
        default_review_decisions_path, default_review_report_path, existing_review_cursor,
        format_duration, merge_unique_candidates, migration_skipped_track_lines,
        non_exact_track_lines, parse_concurrency, restore_source_order, selection_match_totals,
        spotify_offset_page_url, spotify_retry_delay, spotify_selection_options, terminal_safe,
        tidal_review_choices, update_review_choice_cache,
    };
    use crate::model::{
        MatchResult, MatchStatus, ReviewDecisionAction, ScoredCandidate, SourceTrack,
        TidalTrackCandidate,
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
    fn derives_a_dedicated_review_report_path() {
        assert_eq!(
            default_review_report_path(Path::new("data/liked-songs-tidal-matches.json")),
            PathBuf::from("data/liked-songs-tidal-review.json")
        );
        assert_eq!(
            default_review_report_path(Path::new("data/custom-report.json")),
            PathBuf::from("data/custom-report-tidal-review.json")
        );
        assert_eq!(
            default_review_decisions_path(Path::new("data/liked-songs-tidal-review.json")),
            PathBuf::from("data/liked-songs-tidal-review-decisions.json")
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
                cache_hits: 6,
                cache_misses: 4,
                fallback_queries: 2,
                non_exact_tracks: Vec::new(),
                report_path: None,
                review_report_path: None,
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
                cache_hits: 6,
                cache_misses: 4,
                fallback_queries: 2,
                failed_sources: 1,
            }
        );
    }

    #[test]
    fn merges_fallback_candidates_without_duplicate_tidal_ids() {
        let candidate = |tidal_id: &str, title: &str| TidalTrackCandidate {
            tidal_id: tidal_id.to_owned(),
            title: title.to_owned(),
            version: None,
            isrc: None,
            duration_ms: None,
            explicit: None,
            artists: Vec::new(),
            album: None,
        };
        let mut candidates = vec![candidate("1", "Primary")];

        merge_unique_candidates(
            &mut candidates,
            vec![candidate("1", "Duplicate"), candidate("2", "Fallback")],
        );

        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].title, "Primary");
        assert_eq!(candidates[1].tidal_id, "2");
    }

    #[test]
    fn formats_review_candidates_with_useful_metadata() {
        let choice = TidalReviewChoice::Candidate {
            candidate: TidalTrackCandidate {
                tidal_id: "tidal-1".to_owned(),
                title: "Canción (En Vivo)".to_owned(),
                version: Some("Live".to_owned()),
                isrc: Some("PEABC2600001".to_owned()),
                duration_ms: Some(201_000),
                explicit: Some(false),
                artists: vec!["Artista".to_owned()],
                album: Some("Álbum TIDAL".to_owned()),
            },
            rank: 1,
            score: 72,
            suggested: true,
            version_conflict: true,
        };

        let label = choice.to_string();
        assert!(label.contains("[72%]"));
        assert!(label.contains("Artista"));
        assert!(label.contains("album: Álbum TIDAL"));
        assert!(label.contains("3:21"));
        assert!(label.contains("PEABC2600001"));
        assert!(label.contains("suggested"));
        assert!(label.contains("VERSION CONFLICT"));
        assert_eq!(format_duration(65_999), "1:05");
    }

    #[test]
    fn builds_deduplicated_review_choices_and_records_a_selection() {
        let source = SourceTrack {
            position: 3,
            added_at: None,
            spotify_id: Some("spotify-3".to_owned()),
            spotify_uri: "spotify:track:spotify-3".to_owned(),
            title: "Canción".to_owned(),
            artists: vec!["Artista".to_owned()],
            album: Some("Álbum".to_owned()),
            duration_ms: 180_000,
            isrc: None,
            explicit: false,
            is_local: false,
        };
        let candidate = |tidal_id: &str| TidalTrackCandidate {
            tidal_id: tidal_id.to_owned(),
            title: format!("TIDAL {tidal_id}"),
            version: None,
            isrc: None,
            duration_ms: Some(180_000),
            explicit: Some(false),
            artists: vec!["Artista".to_owned()],
            album: Some("Álbum".to_owned()),
        };
        let result = MatchResult {
            spotify_track: source.clone(),
            status: MatchStatus::Review,
            best_candidate: Some(candidate("1")),
            score: Some(75),
            reasons: vec!["Ambiguous candidates".to_owned()],
            alternatives: vec![
                ScoredCandidate {
                    candidate: candidate("1"),
                    score: 74,
                    reasons: Vec::new(),
                    version_conflict: false,
                },
                ScoredCandidate {
                    candidate: candidate("2"),
                    score: 70,
                    reasons: Vec::new(),
                    version_conflict: false,
                },
            ],
            search_query: "Canción Artista".to_owned(),
            error: None,
        };

        let choices = tidal_review_choices(&result);
        assert_eq!(choices.len(), 5);
        let selected_choice = choices[1].clone();
        let decision = decision_from_choice(&source, selected_choice).unwrap();
        assert_eq!(decision.action, ReviewDecisionAction::Selected);
        assert_eq!(decision.selected_candidate_rank, Some(3));
        assert_eq!(
            decision
                .selected_tidal_candidate
                .as_ref()
                .map(|candidate| candidate.tidal_id.as_str()),
            Some("2")
        );
        assert_eq!(existing_review_cursor(&choices, Some(&decision)), 1);

        let skip = decision_from_choice(&source, TidalReviewChoice::Skip).unwrap();
        assert_eq!(skip.action, ReviewDecisionAction::Skipped);
        assert_eq!(existing_review_cursor(&choices, Some(&skip)), 3);
        assert!(decision_from_choice(&source, TidalReviewChoice::ManualTrackId).is_none());
        assert!(decision_from_choice(&source, TidalReviewChoice::Finish).is_none());

        let manual = decision_from_manual_candidate(&source, candidate("manual"));
        assert_eq!(manual.action, ReviewDecisionAction::Selected);
        assert_eq!(manual.selected_candidate_rank, None);
        assert_eq!(manual.selected_machine_match_percentage, None);
        assert_eq!(
            manual
                .selected_tidal_candidate
                .as_ref()
                .map(|item| item.tidal_id.as_str()),
            Some("manual")
        );
    }

    #[test]
    fn missing_tracks_only_offer_manual_id_skip_or_finish() {
        let candidate = |tidal_id: &str| TidalTrackCandidate {
            tidal_id: tidal_id.to_owned(),
            title: format!("TIDAL {tidal_id}"),
            version: None,
            isrc: None,
            duration_ms: Some(180_000),
            explicit: Some(false),
            artists: vec!["Artista".to_owned()],
            album: Some("Álbum".to_owned()),
        };
        let result = MatchResult {
            spotify_track: SourceTrack {
                position: 1,
                added_at: None,
                spotify_id: Some("spotify-1".to_owned()),
                spotify_uri: "spotify:track:spotify-1".to_owned(),
                title: "Sin coincidencia".to_owned(),
                artists: vec!["Artista".to_owned()],
                album: Some("Álbum".to_owned()),
                duration_ms: 180_000,
                isrc: None,
                explicit: false,
                is_local: false,
            },
            status: MatchStatus::Missing,
            best_candidate: Some(candidate("weak-best")),
            score: Some(40),
            reasons: vec!["No acceptable TIDAL match".to_owned()],
            alternatives: vec![ScoredCandidate {
                candidate: candidate("weak-alternative"),
                score: 35,
                reasons: Vec::new(),
                version_conflict: false,
            }],
            search_query: "Sin coincidencia Artista".to_owned(),
            error: None,
        };

        let choices = tidal_review_choices(&result);
        assert_eq!(choices.len(), 3);
        assert!(matches!(choices[0], TidalReviewChoice::ManualTrackId));
        assert!(matches!(choices[1], TidalReviewChoice::Skip));
        assert!(matches!(choices[2], TidalReviewChoice::Finish));
    }

    #[test]
    fn reuses_confirmed_review_choices_across_playlists_and_country() {
        let candidate = TidalTrackCandidate {
            tidal_id: "tidal-1".to_owned(),
            title: "Canción TIDAL".to_owned(),
            version: None,
            isrc: Some("PEABC2600001".to_owned()),
            duration_ms: Some(180_000),
            explicit: Some(false),
            artists: vec!["Artista".to_owned()],
            album: Some("Álbum TIDAL".to_owned()),
        };
        let source = |position| SourceTrack {
            position,
            added_at: None,
            spotify_id: Some("spotify-shared".to_owned()),
            spotify_uri: "spotify:track:spotify-shared".to_owned(),
            title: "Canción".to_owned(),
            artists: vec!["Artista".to_owned()],
            album: Some("Álbum".to_owned()),
            duration_ms: 180_000,
            isrc: Some("PEABC2600001".to_owned()),
            explicit: false,
            is_local: false,
        };
        let result = |position| MatchResult {
            spotify_track: source(position),
            status: MatchStatus::Review,
            best_candidate: Some(candidate.clone()),
            score: Some(75),
            reasons: vec!["Manual review".to_owned()],
            alternatives: Vec::new(),
            search_query: "Canción Artista".to_owned(),
            error: None,
        };
        let first = result(2);
        let selected = decision_from_choice(
            &first.spotify_track,
            TidalReviewChoice::Candidate {
                candidate: candidate.clone(),
                rank: 1,
                score: 75,
                suggested: true,
                version_conflict: false,
            },
        )
        .unwrap();
        let mut first_decisions = BTreeMap::from([(2, selected)]);
        let mut cache = BTreeMap::new();
        update_review_choice_cache("PE", &[&first], &first_decisions, &mut cache).unwrap();

        let second = result(9);
        let mut second_decisions = BTreeMap::new();
        assert_eq!(
            apply_cached_review_choices("PE", &[&second], &mut second_decisions, &cache),
            1
        );
        assert_eq!(
            second_decisions[&9]
                .selected_tidal_candidate
                .as_ref()
                .map(|item| item.tidal_id.as_str()),
            Some("tidal-1")
        );

        let mut other_country = BTreeMap::new();
        assert_eq!(
            apply_cached_review_choices("US", &[&second], &mut other_country, &cache),
            0
        );

        first_decisions.insert(
            2,
            decision_from_choice(&first.spotify_track, TidalReviewChoice::Skip).unwrap(),
        );
        update_review_choice_cache("PE", &[&first], &first_decisions, &mut cache).unwrap();
        assert!(cache.is_empty());
    }

    #[test]
    fn caches_manual_track_id_for_missing_song_across_playlists() {
        let candidate = TidalTrackCandidate {
            tidal_id: "manual-tidal-id".to_owned(),
            title: "Canción TIDAL".to_owned(),
            version: None,
            isrc: None,
            duration_ms: Some(180_000),
            explicit: Some(false),
            artists: vec!["Artista".to_owned()],
            album: Some("Álbum TIDAL".to_owned()),
        };
        let result = |position| MatchResult {
            spotify_track: SourceTrack {
                position,
                added_at: None,
                spotify_id: Some("spotify-missing".to_owned()),
                spotify_uri: "spotify:track:spotify-missing".to_owned(),
                title: "Canción sin coincidencia".to_owned(),
                artists: vec!["Artista".to_owned()],
                album: Some("Álbum".to_owned()),
                duration_ms: 180_000,
                isrc: None,
                explicit: false,
                is_local: false,
            },
            status: MatchStatus::Missing,
            best_candidate: None,
            score: Some(40),
            reasons: vec!["No acceptable TIDAL match".to_owned()],
            alternatives: Vec::new(),
            search_query: "Canción sin coincidencia Artista".to_owned(),
            error: None,
        };

        let first = result(4);
        let manual = decision_from_manual_candidate(&first.spotify_track, candidate);
        let first_decisions = BTreeMap::from([(4, manual)]);
        let mut cache = BTreeMap::new();
        update_review_choice_cache("PE", &[&first], &first_decisions, &mut cache).unwrap();

        let second = result(12);
        let mut second_decisions = BTreeMap::new();
        assert_eq!(
            apply_cached_review_choices("PE", &[&second], &mut second_decisions, &cache),
            1
        );
        let reused = &second_decisions[&12];
        assert_eq!(reused.selected_candidate_rank, None);
        assert_eq!(
            reused
                .selected_tidal_candidate
                .as_ref()
                .map(|item| item.tidal_id.as_str()),
            Some("manual-tidal-id")
        );
    }

    #[test]
    fn lists_only_non_exact_track_titles_in_source_order() {
        let result = |position, title: &str, status, score| MatchResult {
            spotify_track: SourceTrack {
                position,
                added_at: None,
                spotify_id: None,
                spotify_uri: format!("spotify:track:{position}"),
                title: title.to_owned(),
                artists: vec!["Artista".to_owned()],
                album: None,
                duration_ms: 180_000,
                isrc: None,
                explicit: false,
                is_local: false,
            },
            status,
            best_candidate: None,
            score,
            reasons: Vec::new(),
            alternatives: Vec::new(),
            search_query: title.to_owned(),
            error: None,
        };
        let results = vec![
            result(1, "Exacta", MatchStatus::Exact, Some(100)),
            result(2, "Revisar", MatchStatus::Review, Some(70)),
            result(3, "Ausente", MatchStatus::Missing, None),
        ];

        assert_eq!(
            non_exact_track_lines(&results),
            vec![
                "#2 Revisar — Artista — Review (70/100)",
                "#3 Ausente — Artista — Missing (no score)",
            ]
        );
    }

    #[test]
    fn formats_skipped_songs_with_playlist_metadata_and_reason() {
        let skipped = vec![(
            "Qué Rico".to_owned(),
            crate::tidal_import::SkippedImportTrack {
                source_position: 15,
                spotify_title: "Ámame".to_owned(),
                spotify_artists: vec!["El Gran Combo De Puerto Rico".to_owned()],
                spotify_album: Some("¡Ámame!".to_owned()),
                source_match_status: MatchStatus::Missing,
                reason: "No acceptable TIDAL match".to_owned(),
            },
        )];

        assert_eq!(
            migration_skipped_track_lines(&skipped),
            [
                "[Qué Rico] #15 Ámame — El Gran Combo De Puerto Rico | album: ¡Ámame! | Missing | No acceptable TIDAL match"
            ]
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

    #[test]
    fn parses_single_invocation_migration_flags_and_rejects_conflicting_modes() {
        let cli = Cli::try_parse_from([
            "spotify-tidal-migrator",
            "migrate",
            "--concurrency",
            "12",
            "--rate-limit",
            "4",
            "--fallback-searches",
            "--apply",
            "--include-probable",
            "--include-review",
        ])
        .unwrap();
        match cli.command {
            Command::Migrate {
                concurrency,
                rate_limit,
                fallback_searches,
                apply,
                include_probable,
                include_review,
                ..
            } => {
                assert_eq!(concurrency, 12);
                assert_eq!(rate_limit, Some(4.0));
                assert!(fallback_searches);
                assert!(apply);
                assert!(include_probable);
                assert!(include_review);
            }
            _ => panic!("expected migrate command"),
        }

        assert!(
            Cli::try_parse_from(["spotify-tidal-migrator", "migrate", "--dry-run", "--apply",])
                .is_err()
        );
    }
}
