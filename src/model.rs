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

#[cfg(test)]
mod tests {
    use super::SpotifyPlaylistExport;

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
}
