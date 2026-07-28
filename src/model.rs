use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpotifyPlaylistExport {
    pub schema_version: u8,
    pub exported_at_unix: u64,
    pub playlist: ExportedPlaylistMetadata,
    pub tracks: Vec<SourceTrack>,

    #[serde(default)]
    pub skipped_items: Vec<SkippedPlaylistItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportedPlaylistMetadata {
    pub spotify_id: String,
    pub name: String,

    #[serde(default)]
    pub description: Option<String>,

    #[serde(default)]
    pub spotify_url: Option<String>,

    pub snapshot_id: String,
    pub total_reported_by_spotify: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceTrack {
    /// One-based position in the Spotify playlist.
    pub position: usize,

    #[serde(default)]
    pub added_at: Option<String>,

    #[serde(default)]
    pub spotify_id: Option<String>,

    pub spotify_uri: String,
    pub title: String,

    #[serde(default)]
    pub artists: Vec<String>,

    #[serde(default)]
    pub album: Option<String>,

    pub duration_ms: u64,

    #[serde(default)]
    pub isrc: Option<String>,

    #[serde(default)]
    pub explicit: bool,

    #[serde(default)]
    pub is_local: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedPlaylistItem {
    pub position: usize,
    pub reason: String,
    pub title: Option<String>,
    pub spotify_uri: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TidalTrackCandidate {
    pub tidal_id: String,
    pub title: String,
    pub version: Option<String>,
    pub isrc: Option<String>,
    pub duration_ms: Option<u64>,
    pub explicit: Option<bool>,
    pub artists: Vec<String>,
    pub album: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoredCandidate {
    pub candidate: TidalTrackCandidate,
    pub score: u8,
    pub reasons: Vec<String>,
    pub version_conflict: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchStatus {
    Exact,
    Probable,
    Review,
    Missing,
}

impl std::fmt::Display for MatchStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MatchResult {
    pub spotify_track: SourceTrack,
    pub status: MatchStatus,
    pub best_candidate: Option<TidalTrackCandidate>,
    pub score: Option<u8>,
    pub reasons: Vec<String>,
    pub alternatives: Vec<ScoredCandidate>,
    pub search_query: String,
    pub error: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct MatchSummary {
    pub exact: usize,
    pub probable: usize,
    pub review: usize,
    pub missing: usize,
}

impl MatchSummary {
    pub fn record(&mut self, status: MatchStatus) {
        match status {
            MatchStatus::Exact => self.exact += 1,
            MatchStatus::Probable => self.probable += 1,
            MatchStatus::Review => self.review += 1,
            MatchStatus::Missing => self.missing += 1,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MatchReport {
    pub schema_version: u8,
    pub generated_at_unix: u64,
    pub source_playlist: ExportedPlaylistMetadata,
    pub country_code: String,
    pub processed_tracks: usize,
    pub summary: MatchSummary,
    pub results: Vec<MatchResult>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ReviewReport {
    pub schema_version: u8,
    pub generated_at_unix: u64,
    pub source_playlist: ExportedPlaylistMetadata,
    pub source_match_report: String,
    pub review_tracks: Vec<ReviewTrack>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ReviewTrack {
    pub position: usize,
    pub spotify_id: Option<String>,
    pub spotify_title: String,
    pub spotify_artists: Vec<String>,
    pub spotify_album: Option<String>,
    pub match_percentage: u8,
    pub tidal_id: Option<String>,
    pub tidal_title: Option<String>,
    pub tidal_artists: Vec<String>,
    pub tidal_album: Option<String>,
    pub reasons: Vec<String>,
}

impl ReviewReport {
    pub fn from_match_report(report: &MatchReport, source_match_report: String) -> Self {
        let review_tracks = report
            .results
            .iter()
            .filter(|result| result.status == MatchStatus::Review)
            .map(|result| {
                let candidate = result.best_candidate.as_ref();
                ReviewTrack {
                    position: result.spotify_track.position,
                    spotify_id: result.spotify_track.spotify_id.clone(),
                    spotify_title: result.spotify_track.title.clone(),
                    spotify_artists: result.spotify_track.artists.clone(),
                    spotify_album: result.spotify_track.album.clone(),
                    match_percentage: result.score.unwrap_or_default(),
                    tidal_id: candidate.map(|item| item.tidal_id.clone()),
                    tidal_title: candidate.map(|item| item.title.clone()),
                    tidal_artists: candidate
                        .map(|item| item.artists.clone())
                        .unwrap_or_default(),
                    tidal_album: candidate.and_then(|item| item.album.clone()),
                    reasons: result.reasons.clone(),
                }
            })
            .collect();

        Self {
            schema_version: 1,
            generated_at_unix: report.generated_at_unix,
            source_playlist: report.source_playlist.clone(),
            source_match_report,
            review_tracks,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ExportedPlaylistMetadata, MatchReport, MatchResult, MatchStatus, MatchSummary,
        ReviewReport, SourceTrack, SpotifyPlaylistExport, TidalTrackCandidate,
    };

    #[test]
    fn deserializes_spotify_export() {
        let json = r#"{
          "schema_version": 1,
          "exported_at_unix": 1700000000,
          "playlist": {
            "spotify_id": "abc123",
            "name": "Indie Peru",
            "description": "Prueba",
            "spotify_url": "https://open.spotify.com/playlist/abc123",
            "snapshot_id": "snapshot",
            "total_reported_by_spotify": 1
          },
          "tracks": [{
            "position": 1,
            "added_at": "2026-01-01T00:00:00Z",
            "spotify_id": "track1",
            "spotify_uri": "spotify:track:track1",
            "title": "¿Para Qué Me Hablas?",
            "artists": ["Los Outsaiders"],
            "album": "Jueves en el Colectivo",
            "duration_ms": 201000,
            "isrc": "PEABC2600001",
            "explicit": false,
            "is_local": false
          }],
          "skipped_items": []
        }"#;

        let export: SpotifyPlaylistExport = serde_json::from_str(json).unwrap();
        assert_eq!(export.playlist.name, "Indie Peru");
        assert_eq!(export.tracks[0].title, "¿Para Qué Me Hablas?");
        assert_eq!(export.tracks[0].artists, ["Los Outsaiders"]);
    }

    #[test]
    fn review_report_contains_only_review_tracks_with_source_and_candidate_metadata() {
        let result = |position, status, score| MatchResult {
            spotify_track: SourceTrack {
                position,
                added_at: None,
                spotify_id: Some(format!("spotify-{position}")),
                spotify_uri: format!("spotify:track:{position}"),
                title: format!("Canción {position}"),
                artists: vec!["Artista".to_owned()],
                album: Some("Álbum de Spotify".to_owned()),
                duration_ms: 180_000,
                isrc: None,
                explicit: false,
                is_local: false,
            },
            status,
            best_candidate: Some(TidalTrackCandidate {
                tidal_id: format!("tidal-{position}"),
                title: format!("Canción TIDAL {position}"),
                version: None,
                isrc: None,
                duration_ms: Some(180_000),
                explicit: Some(false),
                artists: vec!["Artista TIDAL".to_owned()],
                album: Some("Álbum de TIDAL".to_owned()),
            }),
            score: Some(score),
            reasons: vec!["Possible match".to_owned()],
            alternatives: Vec::new(),
            search_query: "query".to_owned(),
            error: None,
        };
        let report = MatchReport {
            schema_version: 1,
            generated_at_unix: 1_700_000_000,
            source_playlist: ExportedPlaylistMetadata {
                spotify_id: "playlist".to_owned(),
                name: "Lista".to_owned(),
                description: None,
                spotify_url: None,
                snapshot_id: "snapshot".to_owned(),
                total_reported_by_spotify: 2,
            },
            country_code: "PE".to_owned(),
            processed_tracks: 2,
            summary: MatchSummary {
                exact: 1,
                probable: 0,
                review: 1,
                missing: 0,
            },
            results: vec![
                result(1, MatchStatus::Exact, 100),
                result(2, MatchStatus::Review, 72),
            ],
        };

        let review =
            ReviewReport::from_match_report(&report, "data/lista-tidal-matches.json".to_owned());

        assert_eq!(review.review_tracks.len(), 1);
        let track = &review.review_tracks[0];
        assert_eq!(track.position, 2);
        assert_eq!(track.spotify_title, "Canción 2");
        assert_eq!(track.spotify_artists, ["Artista"]);
        assert_eq!(track.spotify_album.as_deref(), Some("Álbum de Spotify"));
        assert_eq!(track.match_percentage, 72);
        assert_eq!(track.tidal_title.as_deref(), Some("Canción TIDAL 2"));
        assert_eq!(track.tidal_artists, ["Artista TIDAL"]);
        assert_eq!(track.tidal_album.as_deref(), Some("Álbum de TIDAL"));
    }
}
