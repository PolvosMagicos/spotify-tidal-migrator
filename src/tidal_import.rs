use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use rand::{RngExt, distr::Alphanumeric};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

use crate::{
    model::{
        MatchReport, MatchResult, MatchStatus, ReviewDecision, ReviewDecisionAction,
        ReviewDecisionReport, TidalTrackCandidate,
    },
    tidal_user::TidalUserClient,
};

pub const IMPORT_SCHEMA_VERSION: u8 = 1;
pub const TIDAL_PLAYLIST_BATCH_SIZE: usize = 50;
const IMPORT_ATTRIBUTION: &str = "Migrated from Spotify using spotify-tidal-migrator.";

#[derive(Debug)]
pub struct ImportCommandOptions {
    pub name: Option<String>,
    pub description: Option<String>,
    pub dry_run: bool,
    pub apply: bool,
    pub include_review: bool,
    pub include_probable: bool,
    pub resume: bool,
    pub output: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportPlan {
    pub source: ImportSource,
    pub destination_name: String,
    pub destination_description: String,
    pub policy: ImportSelectionPolicy,
    pub selection: ImportSelectionSummary,
    pub selected_tracks: Vec<SelectedImportTrack>,
    pub skipped_tracks: Vec<SkippedImportTrack>,
    pub fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportSelectionPolicy {
    pub include_probable: bool,
    pub include_review: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportSource {
    pub match_report_path: String,
    pub spotify_playlist_id: String,
    pub spotify_playlist_name: String,
    pub spotify_snapshot_id: String,
    pub match_report_generated_at_unix: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportSelectionSummary {
    pub exact_included: usize,
    pub probable_included: usize,
    pub review_included: usize,
    pub probable_skipped: usize,
    pub review_skipped: usize,
    pub missing_skipped: usize,
    pub errors_skipped: usize,
    pub local_skipped: usize,
    pub conflicts_skipped: usize,
    pub tracks_to_import: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectedImportTrack {
    pub source_position: usize,
    pub spotify_title: String,
    pub spotify_artists: Vec<String>,
    pub spotify_album: Option<String>,
    pub source_match_status: MatchStatus,
    pub tidal_id: String,
    pub tidal_title: String,
    pub tidal_artists: Vec<String>,
    pub tidal_album: Option<String>,
    pub selected_by_review: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedImportTrack {
    pub source_position: usize,
    pub spotify_title: String,

    #[serde(default)]
    pub spotify_artists: Vec<String>,

    #[serde(default)]
    pub spotify_album: Option<String>,

    pub source_match_status: MatchStatus,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct ImportRunOutcome {
    pub source_playlist_name: String,
    pub skipped_tracks: Vec<SkippedImportTrack>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImportStateStatus {
    Planned,
    PlaylistCreated,
    Importing,
    Completed,
    Failed,
    VerificationFailed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportBatchRecord {
    pub batch_index: usize,
    pub start_index: usize,
    pub track_count: usize,
    pub idempotency_key: String,
    pub completed_at_unix: Option<u64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TidalImportState {
    pub schema_version: u8,
    pub fingerprint: String,
    pub source_report_path: String,
    pub source_spotify_playlist_id: String,
    pub destination_playlist_id: Option<String>,
    pub destination_playlist_name: String,
    pub selected_track_count: usize,
    pub completed_track_count: usize,
    pub completed_batches: Vec<ImportBatchRecord>,
    pub failed_batches: Vec<ImportBatchRecord>,
    pub pending_batch: Option<ImportBatchRecord>,
    pub started_at_unix: u64,
    pub updated_at_unix: u64,
    pub status: ImportStateStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TidalImportReport {
    pub schema_version: u8,
    pub generated_at_unix: u64,
    pub dry_run: bool,
    pub source: ImportSource,
    pub destination: ImportDestination,
    pub policy: ImportSelectionPolicy,
    pub selection: ImportSelectionSummary,
    pub selected_tracks: Vec<SelectedImportTrack>,
    pub skipped_tracks: Vec<SkippedImportTrack>,
    pub batches: Vec<ImportBatchRecord>,
    pub verification: ImportVerification,
    pub errors: Vec<String>,
    pub import_state_path: Option<String>,
    pub resume_available: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportDestination {
    pub playlist_id: Option<String>,
    pub name: String,
    pub description: String,
    pub access_type: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportVerification {
    pub attempted: bool,
    pub succeeded: bool,
    pub expected_tracks: usize,
    pub actual_tracks: Option<usize>,
    pub ordered_track_ids_match: Option<bool>,
    pub message: String,
}

pub async fn run_import(input: &Path, options: ImportCommandOptions) -> Result<ImportRunOutcome> {
    if options.apply && options.dry_run {
        bail!("--apply and --dry-run cannot be used together");
    }
    if options.resume && !options.apply {
        bail!("--resume requires --apply");
    }

    let match_report: MatchReport = read_json(input, "match report")?;
    let decisions = load_review_decisions(input, &match_report, options.include_review)?;
    let plan = build_import_plan(
        &match_report,
        input,
        decisions.as_ref(),
        options.name.as_deref(),
        options.description.as_deref(),
        options.include_probable,
        options.include_review,
    )?;
    let outcome = ImportRunOutcome {
        source_playlist_name: plan.source.spotify_playlist_name.clone(),
        skipped_tracks: plan.skipped_tracks.clone(),
    };
    print_preflight(&plan, !options.apply);

    let report_path = options
        .output
        .unwrap_or_else(|| default_import_report_path(input));
    let state_path = default_import_state_path(input);
    if report_path == input || state_path == input {
        bail!("Import output paths must not overwrite the source match report");
    }
    if !options.apply {
        let report = report_from_plan(&plan, true, None, None)?;
        atomic_write_json(&report_path, &report)?;
        println!("Import plan: {}", report_path.display());
        println!("No TIDAL playlist was created or modified.");
        return Ok(outcome);
    }
    if plan.selected_tracks.is_empty() {
        bail!("No tracks were selected; refusing to create an empty TIDAL playlist");
    }

    println!();
    if options.resume {
        println!(
            "This will resume the saved TIDAL import for {} selected tracks.",
            plan.selected_tracks.len()
        );
        println!("The saved destination playlist will be reused; no new playlist will be created.");
    } else {
        println!(
            "This will create one new TIDAL playlist and add {} tracks.",
            plan.selected_tracks.len()
        );
        println!("Existing playlists will not be modified.");
    }

    if !options.resume && state_path.exists() {
        bail!(
            "Import state {} already exists; use --resume to continue it or choose a different report",
            state_path.display()
        );
    }
    let now = current_unix_timestamp()?;
    let mut state = if options.resume {
        let state: TidalImportState = read_json(&state_path, "TIDAL import state")?;
        validate_resume_state(&state, &plan)?;
        state
    } else {
        new_import_state(&plan, input, now)
    };
    let mut client = TidalUserClient::from_env().await?;
    client.require_scope("playlists.write")?;
    if !options.resume {
        atomic_write_json(&state_path, &state)?;
    }

    let mut report = report_from_plan(
        &plan,
        false,
        Some(&state_path),
        state.destination_playlist_id.as_deref(),
    )?;

    if state.status == ImportStateStatus::Completed {
        println!("Import state is already completed; verifying without adding tracks.");
    } else if state.destination_playlist_id.is_none() {
        let creation_key = creation_idempotency_key(
            &plan.fingerprint,
            &plan.destination_name,
            &plan.destination_description,
        );
        match client
            .create_playlist(
                &plan.destination_name,
                &plan.destination_description,
                &creation_key,
            )
            .await
        {
            Ok(created) => {
                state.destination_playlist_id = Some(created.id.clone());
                state.status = ImportStateStatus::PlaylistCreated;
                state.updated_at_unix = current_unix_timestamp()?;
                atomic_write_json(&state_path, &state)?;
                report.destination.playlist_id = Some(created.id.clone());
                println!("Created TIDAL playlist: {}", created.id);
            }
            Err(error) => {
                return finish_failed_import(
                    error,
                    &mut state,
                    &state_path,
                    &mut report,
                    &report_path,
                );
            }
        }
    }

    let playlist_id = state
        .destination_playlist_id
        .clone()
        .context("Import state has no destination playlist ID")?;
    report.destination.playlist_id = Some(playlist_id.clone());

    if options.resume
        && state.status != ImportStateStatus::Completed
        && let Err(error) = reconcile_resume(&mut client, &plan, &mut state, &state_path).await
    {
        return finish_failed_import(error, &mut state, &state_path, &mut report, &report_path);
    }

    while state.completed_track_count < plan.selected_tracks.len()
        && state.status != ImportStateStatus::Completed
    {
        let start = state.completed_track_count;
        let end = (start + TIDAL_PLAYLIST_BATCH_SIZE).min(plan.selected_tracks.len());
        let batch_index = start / TIDAL_PLAYLIST_BATCH_SIZE;
        let ids = plan.selected_tracks[start..end]
            .iter()
            .map(|track| track.tidal_id.clone())
            .collect::<Vec<_>>();
        let batch = ImportBatchRecord {
            batch_index,
            start_index: start,
            track_count: ids.len(),
            idempotency_key: batch_idempotency_key(&plan.fingerprint, batch_index),
            completed_at_unix: None,
            error: None,
        };
        state.pending_batch = Some(batch.clone());
        state.status = ImportStateStatus::Importing;
        state.updated_at_unix = current_unix_timestamp()?;
        atomic_write_json(&state_path, &state)?;

        println!(
            "Adding batch {}/{} (tracks {}-{})...",
            batch_index + 1,
            plan.selected_tracks
                .len()
                .div_ceil(TIDAL_PLAYLIST_BATCH_SIZE),
            start + 1,
            end
        );
        match client
            .add_playlist_items(&playlist_id, &ids, &batch.idempotency_key)
            .await
        {
            Ok(()) => {
                let mut completed = batch;
                completed.completed_at_unix = Some(current_unix_timestamp()?);
                state.completed_track_count = end;
                state.completed_batches.push(completed);
                state.pending_batch = None;
                state.updated_at_unix = current_unix_timestamp()?;
                atomic_write_json(&state_path, &state)?;
            }
            Err(error) => {
                let mut failed = batch;
                failed.error = Some(error.to_string());
                state.failed_batches.push(failed);
                return finish_failed_import(
                    error,
                    &mut state,
                    &state_path,
                    &mut report,
                    &report_path,
                );
            }
        }
    }

    report.batches = state.completed_batches.clone();
    match verify_import(&mut client, &playlist_id, &plan).await {
        Ok(verification) if verification.succeeded => {
            report.verification = verification;
            state.status = ImportStateStatus::Completed;
            state.updated_at_unix = current_unix_timestamp()?;
            atomic_write_json(&state_path, &state)?;
            report.resume_available = false;
            atomic_write_json(&report_path, &report)?;
            println!("Verification succeeded: all tracks are present in source order.");
            println!("Import state: {}", state_path.display());
            println!("Import report: {}", report_path.display());
            Ok(outcome)
        }
        Ok(verification) => {
            let error = anyhow::anyhow!("{}", verification.message);
            report.verification = verification;
            state.status = ImportStateStatus::VerificationFailed;
            state.updated_at_unix = current_unix_timestamp()?;
            atomic_write_json(&state_path, &state)?;
            report.errors.push(error.to_string());
            report.resume_available = true;
            atomic_write_json(&report_path, &report)?;
            Err(error)
        }
        Err(error) => {
            state.status = ImportStateStatus::VerificationFailed;
            state.updated_at_unix = current_unix_timestamp()?;
            atomic_write_json(&state_path, &state)?;
            report.verification = ImportVerification {
                attempted: true,
                succeeded: false,
                expected_tracks: plan.selected_tracks.len(),
                actual_tracks: None,
                ordered_track_ids_match: None,
                message: format!("Verification request failed: {error}"),
            };
            report.errors.push(error.to_string());
            report.resume_available = true;
            atomic_write_json(&report_path, &report)?;
            Err(error)
        }
    }
}

fn build_import_plan(
    report: &MatchReport,
    input: &Path,
    decisions: Option<&ReviewDecisionReport>,
    name_override: Option<&str>,
    description_override: Option<&str>,
    include_probable: bool,
    include_review: bool,
) -> Result<ImportPlan> {
    validate_match_report(report)?;
    let decision_map = validate_review_decisions(report, decisions)?;
    let mut result_refs = report.results.iter().collect::<Vec<_>>();
    result_refs.sort_by_key(|result| result.spotify_track.position);

    let destination_name = name_override
        .unwrap_or(&report.source_playlist.name)
        .trim()
        .to_owned();
    if destination_name.is_empty() {
        bail!("The destination playlist name cannot be empty");
    }
    let destination_description = destination_description(
        description_override.or(report.source_playlist.description.as_deref()),
    );
    let mut summary = ImportSelectionSummary::default();
    let mut selected_tracks = Vec::new();
    let mut skipped_tracks = Vec::new();

    for result in result_refs {
        let position = result.spotify_track.position;
        let skip = |reason: &str| SkippedImportTrack {
            source_position: position,
            spotify_title: result.spotify_track.title.clone(),
            spotify_artists: result.spotify_track.artists.clone(),
            spotify_album: result.spotify_track.album.clone(),
            source_match_status: result.status,
            reason: reason.to_owned(),
        };

        if result.spotify_track.is_local {
            summary.local_skipped += 1;
            skipped_tracks.push(skip("Local Spotify track"));
            continue;
        }
        if result.error.is_some() {
            summary.errors_skipped += 1;
            skipped_tracks.push(skip("TIDAL search failed for this track"));
            continue;
        }

        let selection = match result.status {
            MatchStatus::Exact => {
                if has_unresolved_conflict(result) {
                    summary.conflicts_skipped += 1;
                    skipped_tracks.push(skip("Unresolved version or explicitness conflict"));
                    None
                } else {
                    summary.exact_included += 1;
                    Some((required_best_candidate(result)?, false))
                }
            }
            MatchStatus::Probable if include_probable => {
                if has_unresolved_conflict(result) {
                    summary.conflicts_skipped += 1;
                    skipped_tracks.push(skip("Unresolved version or explicitness conflict"));
                    None
                } else {
                    summary.probable_included += 1;
                    Some((required_best_candidate(result)?, false))
                }
            }
            MatchStatus::Probable => {
                summary.probable_skipped += 1;
                skipped_tracks.push(skip("Probable match excluded by default"));
                None
            }
            MatchStatus::Review if include_review => match decision_map.get(&position) {
                Some(decision) if decision.action == ReviewDecisionAction::Selected => {
                    let candidate = decision
                        .selected_tidal_candidate
                        .as_ref()
                        .context("A selected review decision has no TIDAL candidate")?;
                    validate_tidal_candidate(candidate)?;
                    summary.review_included += 1;
                    Some((candidate, true))
                }
                Some(_) => {
                    summary.review_skipped += 1;
                    skipped_tracks.push(skip("Review decision explicitly skipped this track"));
                    None
                }
                None => {
                    summary.review_skipped += 1;
                    skipped_tracks.push(skip("Review track has no explicit selection decision"));
                    None
                }
            },
            MatchStatus::Review => {
                summary.review_skipped += 1;
                skipped_tracks.push(skip(
                    "Review match requires --include-review and a decision",
                ));
                None
            }
            MatchStatus::Missing => {
                summary.missing_skipped += 1;
                skipped_tracks.push(skip("No acceptable TIDAL match"));
                None
            }
        };

        if let Some((candidate, selected_by_review)) = selection {
            validate_tidal_candidate(candidate)?;
            selected_tracks.push(selected_track(result, candidate, selected_by_review));
        }
    }

    summary.tracks_to_import = selected_tracks.len();
    let source = ImportSource {
        match_report_path: input.display().to_string(),
        spotify_playlist_id: report.source_playlist.spotify_id.clone(),
        spotify_playlist_name: report.source_playlist.name.clone(),
        spotify_snapshot_id: report.source_playlist.snapshot_id.clone(),
        match_report_generated_at_unix: report.generated_at_unix,
    };
    let policy = ImportSelectionPolicy {
        include_probable,
        include_review,
    };
    let fingerprint = import_fingerprint(&source, &policy, &selected_tracks);

    Ok(ImportPlan {
        source,
        destination_name,
        destination_description,
        policy,
        selection: summary,
        selected_tracks,
        skipped_tracks,
        fingerprint,
    })
}

fn validate_match_report(report: &MatchReport) -> Result<()> {
    if report.schema_version != 1 {
        bail!(
            "Unsupported match report schema version {}; expected version 1",
            report.schema_version
        );
    }
    if report.source_playlist.spotify_id.trim().is_empty()
        || report.source_playlist.name.trim().is_empty()
        || report.source_playlist.snapshot_id.trim().is_empty()
    {
        bail!("Match report source playlist metadata is incomplete");
    }
    if report.processed_tracks != report.results.len() {
        bail!(
            "Match report says it processed {} tracks but contains {} results",
            report.processed_tracks,
            report.results.len()
        );
    }

    let mut positions = HashSet::new();
    for result in &report.results {
        if result.spotify_track.position == 0 {
            bail!("Match report contains source position 0; positions must be one-based");
        }
        if !positions.insert(result.spotify_track.position) {
            bail!(
                "Match report contains duplicate source position {}",
                result.spotify_track.position
            );
        }
    }
    Ok(())
}

fn validate_review_decisions<'a>(
    report: &MatchReport,
    decisions: Option<&'a ReviewDecisionReport>,
) -> Result<BTreeMap<usize, &'a ReviewDecision>> {
    let Some(decisions) = decisions else {
        return Ok(BTreeMap::new());
    };
    if decisions.schema_version != 1 {
        bail!(
            "Unsupported review decisions schema version {}; expected version 1",
            decisions.schema_version
        );
    }
    if decisions.source_playlist.spotify_id != report.source_playlist.spotify_id
        || decisions.source_match_generated_at_unix != report.generated_at_unix
    {
        bail!("Review decisions do not belong to this match-report generation");
    }

    let mut map = BTreeMap::new();
    for decision in &decisions.decisions {
        let result = report
            .results
            .iter()
            .find(|result| result.spotify_track.position == decision.position)
            .with_context(|| {
                format!(
                    "Review decision references unknown source position {}",
                    decision.position
                )
            })?;
        if result.status != MatchStatus::Review
            || result.spotify_track.spotify_id != decision.spotify_id
            || result.spotify_track.title != decision.spotify_title
        {
            bail!(
                "Review decision at source position {} does not match the Review result",
                decision.position
            );
        }
        if map.insert(decision.position, decision).is_some() {
            bail!(
                "Review decisions contain duplicate source position {}",
                decision.position
            );
        }
    }
    Ok(map)
}

fn required_best_candidate(result: &MatchResult) -> Result<&TidalTrackCandidate> {
    result
        .best_candidate
        .as_ref()
        .context("A selected match result has no TIDAL candidate")
}

fn validate_tidal_candidate(candidate: &TidalTrackCandidate) -> Result<()> {
    if candidate.tidal_id.trim().is_empty() || candidate.tidal_id.chars().any(char::is_control) {
        bail!("A selected TIDAL candidate has an invalid opaque resource ID");
    }
    Ok(())
}

fn has_unresolved_conflict(result: &MatchResult) -> bool {
    result.reasons.iter().any(|reason| {
        let reason = reason.to_ascii_lowercase();
        reason.contains("conflict")
    })
}

fn selected_track(
    result: &MatchResult,
    candidate: &TidalTrackCandidate,
    selected_by_review: bool,
) -> SelectedImportTrack {
    SelectedImportTrack {
        source_position: result.spotify_track.position,
        spotify_title: result.spotify_track.title.clone(),
        spotify_artists: result.spotify_track.artists.clone(),
        spotify_album: result.spotify_track.album.clone(),
        source_match_status: result.status,
        tidal_id: candidate.tidal_id.clone(),
        tidal_title: candidate.title.clone(),
        tidal_artists: candidate.artists.clone(),
        tidal_album: candidate.album.clone(),
        selected_by_review,
    }
}

fn destination_description(source: Option<&str>) -> String {
    let source = source.unwrap_or_default().trim();
    if source.is_empty() {
        IMPORT_ATTRIBUTION.to_owned()
    } else if source.contains(IMPORT_ATTRIBUTION) {
        source.to_owned()
    } else {
        format!("{source}\n\n{IMPORT_ATTRIBUTION}")
    }
}

fn import_fingerprint(
    source: &ImportSource,
    policy: &ImportSelectionPolicy,
    selected: &[SelectedImportTrack],
) -> String {
    let mut hasher = Sha256::new();
    hash_component(&mut hasher, "spotify-tidal-import-v1");
    hash_component(&mut hasher, &source.spotify_playlist_id);
    hash_component(&mut hasher, &source.spotify_snapshot_id);
    hash_component(
        &mut hasher,
        &source.match_report_generated_at_unix.to_string(),
    );
    hash_component(&mut hasher, if policy.include_probable { "1" } else { "0" });
    hash_component(&mut hasher, if policy.include_review { "1" } else { "0" });
    for track in selected {
        hash_component(&mut hasher, &track.tidal_id);
    }
    hex_digest(hasher.finalize().as_slice())
}

fn hash_component(hasher: &mut Sha256, value: &str) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value.as_bytes());
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn creation_idempotency_key(fingerprint: &str, name: &str, description: &str) -> String {
    let mut hasher = Sha256::new();
    hash_component(&mut hasher, fingerprint);
    hash_component(&mut hasher, name);
    hash_component(&mut hasher, description);
    format!("stm-create-{}", &hex_digest(&hasher.finalize())[..32])
}

fn batch_idempotency_key(fingerprint: &str, batch_index: usize) -> String {
    format!("stm-{}-batch-{batch_index}", &fingerprint[..32])
}

fn new_import_state(plan: &ImportPlan, input: &Path, now: u64) -> TidalImportState {
    TidalImportState {
        schema_version: IMPORT_SCHEMA_VERSION,
        fingerprint: plan.fingerprint.clone(),
        source_report_path: input.display().to_string(),
        source_spotify_playlist_id: plan.source.spotify_playlist_id.clone(),
        destination_playlist_id: None,
        destination_playlist_name: plan.destination_name.clone(),
        selected_track_count: plan.selected_tracks.len(),
        completed_track_count: 0,
        completed_batches: Vec::new(),
        failed_batches: Vec::new(),
        pending_batch: None,
        started_at_unix: now,
        updated_at_unix: now,
        status: ImportStateStatus::Planned,
    }
}

fn validate_resume_state(state: &TidalImportState, plan: &ImportPlan) -> Result<()> {
    if state.schema_version != IMPORT_SCHEMA_VERSION {
        bail!(
            "Unsupported import-state schema version {}; expected {}",
            state.schema_version,
            IMPORT_SCHEMA_VERSION
        );
    }
    if state.fingerprint != plan.fingerprint {
        bail!("Import-state fingerprint does not match the current report and selection");
    }
    if state.source_spotify_playlist_id != plan.source.spotify_playlist_id
        || state.selected_track_count != plan.selected_tracks.len()
        || state.destination_playlist_name != plan.destination_name
    {
        bail!("Import state is incompatible with the current import plan");
    }
    if state.completed_track_count > state.selected_track_count {
        bail!("Import state has an invalid completed-track count");
    }
    if state.destination_playlist_id.is_none()
        && !matches!(
            state.status,
            ImportStateStatus::Planned | ImportStateStatus::Failed
        )
    {
        bail!("Import state is missing its destination playlist ID");
    }
    Ok(())
}

async fn reconcile_resume(
    client: &mut TidalUserClient,
    plan: &ImportPlan,
    state: &mut TidalImportState,
    state_path: &Path,
) -> Result<()> {
    let playlist_id = state
        .destination_playlist_id
        .as_deref()
        .context("Cannot resume without a destination playlist ID")?;
    let actual = client.playlist_items(playlist_id).await?;
    if actual.iter().any(|item| item.resource_type != "tracks") {
        bail!("Destination playlist contains non-track items and cannot be resumed safely");
    }
    let actual_ids = actual
        .iter()
        .map(|item| item.id.as_str())
        .collect::<Vec<_>>();
    let expected_ids = plan
        .selected_tracks
        .iter()
        .map(|track| track.tidal_id.as_str())
        .collect::<Vec<_>>();

    if let Some(pending) = state.pending_batch.clone() {
        let pending_end = pending.start_index.saturating_add(pending.track_count);
        if pending.start_index != state.completed_track_count || pending_end > expected_ids.len() {
            bail!("Import state contains an invalid pending batch");
        }
        if actual_ids == expected_ids[..pending_end] {
            let mut completed = pending;
            completed.completed_at_unix = Some(current_unix_timestamp()?);
            state.completed_track_count = pending_end;
            state.completed_batches.push(completed);
            state.pending_batch = None;
            state.updated_at_unix = current_unix_timestamp()?;
            atomic_write_json(state_path, state)?;
        } else if actual_ids != expected_ids[..state.completed_track_count] {
            bail!(
                "Destination playlist contents do not match either the confirmed or pending import prefix"
            );
        }
    } else if actual_ids != expected_ids[..state.completed_track_count] {
        bail!("Destination playlist contents do not match the confirmed import prefix");
    }
    Ok(())
}

async fn verify_import(
    client: &mut TidalUserClient,
    playlist_id: &str,
    plan: &ImportPlan,
) -> Result<ImportVerification> {
    let actual = client.playlist_items(playlist_id).await?;
    let all_tracks = actual.iter().all(|item| item.resource_type == "tracks");
    let actual_ids = actual
        .iter()
        .map(|item| item.id.as_str())
        .collect::<Vec<_>>();
    let expected_ids = plan
        .selected_tracks
        .iter()
        .map(|track| track.tidal_id.as_str())
        .collect::<Vec<_>>();
    let matches = all_tracks && actual_ids == expected_ids;
    Ok(ImportVerification {
        attempted: true,
        succeeded: matches,
        expected_tracks: expected_ids.len(),
        actual_tracks: Some(actual_ids.len()),
        ordered_track_ids_match: Some(matches),
        message: if matches {
            "Verified item count, order, resource type, and duplicate positions.".to_owned()
        } else {
            format!(
                "Verification mismatch: expected {} ordered tracks, received {} items.",
                expected_ids.len(),
                actual_ids.len()
            )
        },
    })
}

fn finish_failed_import<T>(
    error: anyhow::Error,
    state: &mut TidalImportState,
    state_path: &Path,
    report: &mut TidalImportReport,
    report_path: &Path,
) -> Result<T> {
    state.status = ImportStateStatus::Failed;
    state.updated_at_unix = current_unix_timestamp()?;
    atomic_write_json(state_path, state)?;
    report.batches = state
        .completed_batches
        .iter()
        .chain(&state.failed_batches)
        .cloned()
        .collect();
    report.errors.push(error.to_string());
    report.resume_available = true;
    atomic_write_json(report_path, report)?;
    Err(error)
}

fn report_from_plan(
    plan: &ImportPlan,
    dry_run: bool,
    state_path: Option<&Path>,
    playlist_id: Option<&str>,
) -> Result<TidalImportReport> {
    Ok(TidalImportReport {
        schema_version: IMPORT_SCHEMA_VERSION,
        generated_at_unix: current_unix_timestamp()?,
        dry_run,
        source: plan.source.clone(),
        destination: ImportDestination {
            playlist_id: playlist_id.map(ToOwned::to_owned),
            name: plan.destination_name.clone(),
            description: plan.destination_description.clone(),
            access_type: "UNLISTED".to_owned(),
        },
        policy: plan.policy.clone(),
        selection: plan.selection.clone(),
        selected_tracks: plan.selected_tracks.clone(),
        skipped_tracks: plan.skipped_tracks.clone(),
        batches: Vec::new(),
        verification: ImportVerification {
            attempted: false,
            succeeded: false,
            expected_tracks: plan.selected_tracks.len(),
            actual_tracks: None,
            ordered_track_ids_match: None,
            message: if dry_run {
                "Not attempted during dry run.".to_owned()
            } else {
                "Not attempted yet.".to_owned()
            },
        },
        errors: Vec::new(),
        import_state_path: state_path.map(|path| path.display().to_string()),
        resume_available: false,
    })
}

fn print_preflight(plan: &ImportPlan, dry_run: bool) {
    println!("Playlist: {}", terminal_safe(&plan.destination_name));
    println!("Exact selected: {}", plan.selection.exact_included);
    println!("Probable selected: {}", plan.selection.probable_included);
    println!("Review selected: {}", plan.selection.review_included);
    println!("Probable skipped: {}", plan.selection.probable_skipped);
    println!("Review skipped: {}", plan.selection.review_skipped);
    println!("Missing skipped: {}", plan.selection.missing_skipped);
    println!("Errors skipped: {}", plan.selection.errors_skipped);
    println!("Local tracks skipped: {}", plan.selection.local_skipped);
    println!(
        "Version conflicts skipped: {}",
        plan.selection.conflicts_skipped
    );
    println!("Tracks to import: {}", plan.selection.tracks_to_import);
    println!("Dry run: {}", if dry_run { "yes" } else { "no" });
    println!(
        "Description: {}",
        terminal_safe(&truncate_unicode(&plan.destination_description, 160))
    );
    print_selected_preview(&plan.selected_tracks);
}

fn print_selected_preview(tracks: &[SelectedImportTrack]) {
    if tracks.is_empty() {
        println!("Selected tracks: none");
        return;
    }
    println!("First selected tracks:");
    for track in tracks.iter().take(5) {
        print_preview_track(track);
    }
    if tracks.len() > 5 {
        println!("Last selected tracks:");
        for track in tracks.iter().skip(tracks.len().saturating_sub(5)) {
            print_preview_track(track);
        }
    }
}

fn print_preview_track(track: &SelectedImportTrack) {
    let artists = if track.spotify_artists.is_empty() {
        "Unknown artist".to_owned()
    } else {
        track.spotify_artists.join(", ")
    };
    println!(
        "  #{} {} — {}",
        track.source_position,
        terminal_safe(&track.spotify_title),
        terminal_safe(&artists)
    );
}

fn truncate_unicode(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        value.to_owned()
    } else {
        value.chars().take(max_chars).collect()
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

fn load_review_decisions(
    input: &Path,
    report: &MatchReport,
    include_review: bool,
) -> Result<Option<ReviewDecisionReport>> {
    if !include_review {
        return Ok(None);
    }
    let path = default_review_decisions_path(input);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("Could not read {}", path.display()));
        }
    };
    let decisions: ReviewDecisionReport = serde_json::from_slice(&bytes)
        .with_context(|| format!("Invalid review decisions JSON in {}", path.display()))?;
    validate_review_decisions(report, Some(&decisions))?;
    Ok(Some(decisions))
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

fn default_review_decisions_path(match_report: &Path) -> PathBuf {
    let stem = report_base_stem(match_report);
    match_report_parent(match_report).join(format!("{stem}-tidal-review-decisions.json"))
}

pub fn default_import_state_path(match_report: &Path) -> PathBuf {
    let stem = report_base_stem(match_report);
    match_report_parent(match_report).join(format!("{stem}-tidal-import-state.json"))
}

pub fn default_import_report_path(match_report: &Path) -> PathBuf {
    let stem = report_base_stem(match_report);
    match_report_parent(match_report).join(format!("{stem}-tidal-import-report.json"))
}

fn report_base_stem(path: &Path) -> &str {
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("playlist");
    stem.strip_suffix("-tidal-matches").unwrap_or(stem)
}

fn match_report_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("data"))
}

fn atomic_write_json<T>(path: &Path, value: &T) -> Result<()>
where
    T: Serialize,
{
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let serialized = serde_json::to_vec_pretty(value)?;
    let suffix: String = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(12)
        .map(char::from)
        .collect();
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("tidal-import.json");
    let temporary = path.with_file_name(format!(".{file_name}.{suffix}.tmp"));

    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .with_context(|| format!("Could not create {}", temporary.display()))?;
        file.write_all(&serialized)?;
        file.flush()?;
        file.sync_all()?;
        fs::rename(&temporary, path)
            .with_context(|| format!("Could not atomically replace {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn current_unix_timestamp() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("System clock is before the Unix epoch")?
        .as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ExportedPlaylistMetadata, MatchSummary, SourceTrack};

    fn metadata() -> ExportedPlaylistMetadata {
        ExportedPlaylistMetadata {
            spotify_id: "spotify-playlist".to_owned(),
            name: "Qué Rico".to_owned(),
            description: Some("Descripción".to_owned()),
            spotify_url: None,
            snapshot_id: "snapshot".to_owned(),
            total_reported_by_spotify: 4,
        }
    }

    fn candidate(id: &str) -> TidalTrackCandidate {
        TidalTrackCandidate {
            tidal_id: id.to_owned(),
            title: format!("TIDAL {id}"),
            version: None,
            isrc: None,
            duration_ms: Some(180_000),
            explicit: Some(false),
            artists: vec!["Artista TIDAL".to_owned()],
            album: Some("Álbum TIDAL".to_owned()),
        }
    }

    fn result(position: usize, status: MatchStatus) -> MatchResult {
        MatchResult {
            spotify_track: SourceTrack {
                position,
                added_at: None,
                spotify_id: Some(format!("spotify-{position}")),
                spotify_uri: format!("spotify:track:{position}"),
                title: format!("Canción {position}"),
                artists: vec!["Artista".to_owned()],
                album: Some("Álbum".to_owned()),
                duration_ms: 180_000,
                isrc: None,
                explicit: false,
                is_local: false,
            },
            status,
            best_candidate: (status != MatchStatus::Missing)
                .then(|| candidate(&format!("tidal-{position}"))),
            score: Some(100),
            reasons: Vec::new(),
            alternatives: Vec::new(),
            search_query: "query".to_owned(),
            error: None,
        }
    }

    fn report() -> MatchReport {
        MatchReport {
            schema_version: 1,
            generated_at_unix: 1_700_000_000,
            source_playlist: metadata(),
            country_code: "PE".to_owned(),
            processed_tracks: 4,
            summary: MatchSummary {
                exact: 1,
                probable: 1,
                review: 1,
                missing: 1,
            },
            results: vec![
                result(1, MatchStatus::Exact),
                result(2, MatchStatus::Probable),
                result(3, MatchStatus::Review),
                result(4, MatchStatus::Missing),
            ],
        }
    }

    fn decisions() -> ReviewDecisionReport {
        ReviewDecisionReport {
            schema_version: 1,
            generated_at_unix: 1_700_000_100,
            source_playlist: metadata(),
            source_match_report: "data/report.json".to_owned(),
            source_match_generated_at_unix: 1_700_000_000,
            source_review_report: "data/review.json".to_owned(),
            selected_count: 1,
            skipped_count: 0,
            decisions: vec![ReviewDecision {
                position: 3,
                spotify_id: Some("spotify-3".to_owned()),
                spotify_title: "Canción 3".to_owned(),
                spotify_artists: vec!["Artista".to_owned()],
                spotify_album: Some("Álbum".to_owned()),
                action: ReviewDecisionAction::Selected,
                selected_candidate_rank: Some(2),
                selected_machine_match_percentage: Some(75),
                selected_tidal_candidate: Some(candidate("review-choice")),
            }],
        }
    }

    #[test]
    fn exact_only_is_the_default_selection() {
        let plan = build_import_plan(
            &report(),
            Path::new("data/report.json"),
            None,
            None,
            None,
            false,
            false,
        )
        .unwrap();
        assert_eq!(plan.selected_tracks.len(), 1);
        assert_eq!(
            plan.selected_tracks[0].source_match_status,
            MatchStatus::Exact
        );
        assert_eq!(plan.selection.probable_skipped, 1);
        assert_eq!(plan.selection.review_skipped, 1);
        assert_eq!(plan.selection.missing_skipped, 1);
        let missing = plan
            .skipped_tracks
            .iter()
            .find(|track| track.source_position == 4)
            .unwrap();
        assert_eq!(missing.spotify_artists, ["Artista"]);
        assert_eq!(missing.spotify_album.as_deref(), Some("Álbum"));
        assert_eq!(missing.reason, "No acceptable TIDAL match");
    }

    #[test]
    fn probable_and_explicit_review_choices_can_be_included() {
        let report = report();
        let decisions = decisions();
        let plan = build_import_plan(
            &report,
            Path::new("data/report.json"),
            Some(&decisions),
            None,
            None,
            true,
            true,
        )
        .unwrap();
        assert_eq!(
            plan.selected_tracks
                .iter()
                .map(|track| track.tidal_id.as_str())
                .collect::<Vec<_>>(),
            ["tidal-1", "tidal-2", "review-choice"]
        );
        assert!(plan.selected_tracks[2].selected_by_review);
    }

    #[test]
    fn missing_errors_local_tracks_and_conflicts_are_always_skipped() {
        let mut report = report();
        report.results[0].reasons = vec!["Explicit/clean status conflicts".to_owned()];
        report.results[1].error = Some("temporary failure".to_owned());
        report.results[2].spotify_track.is_local = true;
        let plan = build_import_plan(
            &report,
            Path::new("data/report.json"),
            None,
            None,
            None,
            true,
            true,
        )
        .unwrap();
        assert!(plan.selected_tracks.is_empty());
        assert_eq!(plan.selection.conflicts_skipped, 1);
        assert_eq!(plan.selection.errors_skipped, 1);
        assert_eq!(plan.selection.local_skipped, 1);
        assert_eq!(plan.selection.missing_skipped, 1);
    }

    #[test]
    fn fingerprint_is_stable_and_order_sensitive() {
        let plan = build_import_plan(
            &report(),
            Path::new("data/report.json"),
            None,
            None,
            None,
            true,
            false,
        )
        .unwrap();
        assert_eq!(
            import_fingerprint(&plan.source, &plan.policy, &plan.selected_tracks),
            import_fingerprint(&plan.source, &plan.policy, &plan.selected_tracks)
        );
        let mut reversed = plan.selected_tracks.clone();
        reversed.reverse();
        assert_ne!(
            import_fingerprint(&plan.source, &plan.policy, &plan.selected_tracks),
            import_fingerprint(&plan.source, &plan.policy, &reversed)
        );
    }

    #[test]
    fn chunks_tracks_at_the_official_batch_limit() {
        let tracks = (0..121).collect::<Vec<_>>();
        let sizes = tracks
            .chunks(TIDAL_PLAYLIST_BATCH_SIZE)
            .map(<[usize]>::len)
            .collect::<Vec<_>>();
        assert_eq!(sizes, [50, 50, 21]);
    }

    #[test]
    fn resume_requires_the_same_fingerprint() {
        let plan = build_import_plan(
            &report(),
            Path::new("data/report.json"),
            None,
            None,
            None,
            false,
            false,
        )
        .unwrap();
        let mut state = new_import_state(&plan, Path::new("data/report.json"), 10);
        validate_resume_state(&state, &plan).unwrap();
        state.fingerprint = "different".to_owned();
        assert!(validate_resume_state(&state, &plan).is_err());
    }

    #[test]
    fn import_state_and_report_round_trip() {
        let plan = build_import_plan(
            &report(),
            Path::new("data/report.json"),
            None,
            None,
            None,
            false,
            false,
        )
        .unwrap();
        let state = new_import_state(&plan, Path::new("data/report.json"), 10);
        let state_json = serde_json::to_vec(&state).unwrap();
        let restored: TidalImportState = serde_json::from_slice(&state_json).unwrap();
        assert_eq!(restored.fingerprint, state.fingerprint);

        let report = report_from_plan(&plan, true, None, None).unwrap();
        let report_json = serde_json::to_vec(&report).unwrap();
        let restored: TidalImportReport = serde_json::from_slice(&report_json).unwrap();
        assert!(restored.dry_run);
        assert_eq!(restored.selection.exact_included, 1);
    }

    #[test]
    fn atomic_state_write_replaces_valid_json() {
        let directory = std::env::temp_dir().join(format!(
            "spotify-tidal-import-test-{}",
            rand::rng()
                .sample_iter(&Alphanumeric)
                .take(12)
                .map(char::from)
                .collect::<String>()
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("state.json");
        atomic_write_json(&path, &serde_json::json!({"value": 1})).unwrap();
        atomic_write_json(&path, &serde_json::json!({"value": 2})).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["value"], 2);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn unicode_truncation_uses_character_boundaries() {
        assert_eq!(truncate_unicode("Qué canción 日本", 12), "Qué canción ");
        assert_eq!(truncate_unicode("Álbum", 20), "Álbum");
    }

    #[test]
    fn default_paths_follow_the_match_report() {
        let input = Path::new("data/que-rico-tidal-matches.json");
        assert_eq!(
            default_import_state_path(input),
            PathBuf::from("data/que-rico-tidal-import-state.json")
        );
        assert_eq!(
            default_import_report_path(input),
            PathBuf::from("data/que-rico-tidal-import-report.json")
        );
        assert_eq!(
            default_review_decisions_path(input),
            PathBuf::from("data/que-rico-tidal-review-decisions.json")
        );
    }

    #[tokio::test]
    async fn dry_run_writes_a_plan_without_loading_a_mutation_client() {
        let directory = std::env::temp_dir().join(format!(
            "spotify-tidal-dry-run-test-{}",
            rand::rng()
                .sample_iter(&Alphanumeric)
                .take(12)
                .map(char::from)
                .collect::<String>()
        ));
        fs::create_dir_all(&directory).unwrap();
        let input = directory.join("matches.json");
        let output = directory.join("plan.json");
        fs::write(&input, serde_json::to_vec_pretty(&report()).unwrap()).unwrap();

        let outcome = run_import(
            &input,
            ImportCommandOptions {
                name: None,
                description: None,
                dry_run: true,
                apply: false,
                include_review: false,
                include_probable: false,
                resume: false,
                output: Some(output.clone()),
            },
        )
        .await
        .unwrap();

        assert_eq!(outcome.source_playlist_name, "Qué Rico");
        assert_eq!(outcome.skipped_tracks.len(), 3);
        let plan_report: TidalImportReport =
            serde_json::from_slice(&fs::read(output).unwrap()).unwrap();
        assert!(plan_report.dry_run);
        assert_eq!(plan_report.selection.tracks_to_import, 1);
        assert!(!default_import_state_path(&input).exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn resume_recovers_a_successful_pending_batch_without_readding_it() {
        use axum::{Router, routing::get};
        use url::Url;

        let plan = build_import_plan(
            &report(),
            Path::new("data/report.json"),
            None,
            None,
            None,
            true,
            false,
        )
        .unwrap();
        let mut state = new_import_state(&plan, Path::new("data/report.json"), 10);
        state.destination_playlist_id = Some("playlist-1".to_owned());
        state.completed_track_count = 1;
        state.status = ImportStateStatus::Importing;
        state.pending_batch = Some(ImportBatchRecord {
            batch_index: 1,
            start_index: 1,
            track_count: 1,
            idempotency_key: "pending-key".to_owned(),
            completed_at_unix: None,
            error: None,
        });

        let app = Router::new().fallback(get(|| async {
            (
                [("content-type", "application/vnd.api+json")],
                r#"{"data":[{"type":"tracks","id":"tidal-1"},{"type":"tracks","id":"tidal-2"}],"links":{"next":null}}"#,
            )
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut client = TidalUserClient::for_test(
            Url::parse(&format!("http://{address}/v2/")).unwrap(),
            format!("http://{address}/token"),
            "access",
            Some("refresh"),
            "playlists.write",
        );
        let state_path = std::env::temp_dir().join(format!(
            "spotify-tidal-resume-state-{}.json",
            rand::rng()
                .sample_iter(&Alphanumeric)
                .take(12)
                .map(char::from)
                .collect::<String>()
        ));
        atomic_write_json(&state_path, &state).unwrap();

        reconcile_resume(&mut client, &plan, &mut state, &state_path)
            .await
            .unwrap();
        assert_eq!(state.completed_track_count, 2);
        assert!(state.pending_batch.is_none());
        assert_eq!(state.completed_batches.len(), 1);

        task.abort();
        fs::remove_file(state_path).unwrap();
    }
}
