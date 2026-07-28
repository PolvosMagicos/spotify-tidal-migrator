mod normalize;

use std::collections::BTreeSet;

use strsim::jaro_winkler;

use crate::model::{MatchResult, MatchStatus, ScoredCandidate, SourceTrack, TidalTrackCandidate};

pub use normalize::{normalize, normalize_artist};

pub const EXACT_THRESHOLD: u8 = 95;
pub const PROBABLE_THRESHOLD: u8 = 80;
pub const REVIEW_THRESHOLD: u8 = 55;
const AMBIGUITY_MARGIN: u8 = 3;

pub fn search_query(track: &SourceTrack) -> String {
    match track.artists.first() {
        Some(artist) if !artist.trim().is_empty() => format!("{} {artist}", track.title),
        _ => track.title.clone(),
    }
}

pub fn fallback_search_queries(track: &SourceTrack) -> Vec<String> {
    let title = track.title.trim();
    if title.is_empty() {
        return Vec::new();
    }

    let primary_artist = track
        .artists
        .first()
        .map(|artist| artist.trim())
        .filter(|artist| !artist.is_empty());
    let mut queries = Vec::with_capacity(2);

    if let Some(album) = track.album.as_deref().map(str::trim)
        && !album.is_empty()
    {
        let album_query = match primary_artist {
            Some(artist) => format!("{title} {artist} {album}"),
            None => format!("{title} {album}"),
        };
        if album_query != search_query(track) {
            queries.push(album_query);
        }
    }

    if title != search_query(track) && !queries.iter().any(|query| query == title) {
        queries.push(title.to_owned());
    }

    queries
}

pub fn match_candidates(
    track: &SourceTrack,
    query: String,
    candidates: Vec<TidalTrackCandidate>,
) -> MatchResult {
    let mut scored: Vec<_> = candidates
        .into_iter()
        .map(|candidate| score_candidate(track, candidate))
        .collect();

    scored.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.candidate.tidal_id.cmp(&right.candidate.tidal_id))
    });

    let ambiguous = scored
        .get(1)
        .zip(scored.first())
        .is_some_and(|(second, first)| {
            first.score < 100
                && first.score >= PROBABLE_THRESHOLD
                && first.score.saturating_sub(second.score) <= AMBIGUITY_MARGIN
        });

    let status = scored.first().map_or(MatchStatus::Missing, |best| {
        classify(best.score, best.version_conflict, ambiguous)
    });

    let best = scored.first().cloned();
    let alternatives = scored.into_iter().skip(1).take(5).collect();

    MatchResult {
        spotify_track: track.clone(),
        status,
        best_candidate: best.as_ref().map(|item| item.candidate.clone()),
        score: best.as_ref().map(|item| item.score),
        reasons: best.map_or_else(Vec::new, |item| item.reasons),
        alternatives,
        search_query: query,
        error: None,
    }
}

pub fn failed_match(track: &SourceTrack, query: String, error: String) -> MatchResult {
    MatchResult {
        spotify_track: track.clone(),
        status: MatchStatus::Missing,
        best_candidate: None,
        score: None,
        reasons: vec!["TIDAL search failed; manual retry is required".to_owned()],
        alternatives: Vec::new(),
        search_query: query,
        error: Some(error),
    }
}

pub fn classify(score: u8, version_conflict: bool, ambiguous: bool) -> MatchStatus {
    if score < REVIEW_THRESHOLD {
        MatchStatus::Missing
    } else if version_conflict || ambiguous {
        MatchStatus::Review
    } else if score >= EXACT_THRESHOLD {
        MatchStatus::Exact
    } else if score >= PROBABLE_THRESHOLD {
        MatchStatus::Probable
    } else {
        MatchStatus::Review
    }
}

pub fn score_candidate(source: &SourceTrack, candidate: TidalTrackCandidate) -> ScoredCandidate {
    let mut score = 0_i16;
    let mut reasons = Vec::new();

    let isrc_exact = source
        .isrc
        .as_deref()
        .zip(candidate.isrc.as_deref())
        .is_some_and(|(source_isrc, tidal_isrc)| {
            source_isrc.trim().eq_ignore_ascii_case(tidal_isrc.trim())
        });

    let source_title = normalize(&source.title);
    let candidate_title = candidate_title(&candidate);
    let normalized_candidate_title = normalize(&candidate_title);

    let title_version_conflict = version_conflict(&source_title, &normalized_candidate_title);
    let explicit_conflict = candidate
        .explicit
        .is_some_and(|explicit| explicit != source.explicit);
    let version_conflict = title_version_conflict || explicit_conflict;

    if isrc_exact && !version_conflict {
        reasons.push("Exact ISRC match".to_owned());
        return ScoredCandidate {
            candidate,
            score: 100,
            reasons,
            version_conflict: false,
        };
    }

    if isrc_exact {
        score += 70;
        reasons.push("Exact ISRC match, but version or explicitness metadata conflicts".to_owned());
    }

    if source_title == normalized_candidate_title {
        score += 45;
        reasons.push("Normalized title matches exactly (+45)".to_owned());
    } else {
        let similarity = jaro_winkler(&source_title, &normalized_candidate_title);
        if similarity >= 0.70 {
            let title_points = (similarity * 45.0).round() as i16;
            score += title_points;
            reasons.push(format!(
                "Normalized title similarity {:.0}% (+{title_points})",
                similarity * 100.0
            ));
        } else {
            reasons.push(format!("Low title similarity ({:.0}%)", similarity * 100.0));
        }
    }

    if let (Some(source_artist), Some(candidate_artist)) =
        (source.artists.first(), candidate.artists.first())
    {
        let source_artist = normalize_artist(source_artist);
        let candidate_artist = normalize_artist(candidate_artist);

        if source_artist == candidate_artist {
            score += 30;
            reasons.push("Primary artist matches exactly (+30)".to_owned());
        } else {
            let similarity = jaro_winkler(&source_artist, &candidate_artist);
            if similarity >= 0.80 {
                let artist_points = (similarity * 25.0).round() as i16;
                score += artist_points;
                reasons.push(format!(
                    "Primary artist similarity {:.0}% (+{artist_points})",
                    similarity * 100.0
                ));
            } else {
                reasons.push("Primary artist does not match".to_owned());
            }
        }
    } else {
        reasons.push("TIDAL search response did not include artist names".to_owned());
    }

    if let (Some(source_album), Some(candidate_album)) =
        (source.album.as_deref(), candidate.album.as_deref())
        && normalize(source_album) == normalize(candidate_album)
    {
        score += 10;
        reasons.push("Album matches exactly (+10)".to_owned());
    }

    let duration_points = duration_score(source.duration_ms, candidate.duration_ms);
    score += i16::from(duration_points);
    match duration_points {
        10 => reasons.push("Duration differs by at most 2 seconds (+10)".to_owned()),
        5 => reasons.push("Duration differs by at most 5 seconds (+5)".to_owned()),
        _ if candidate.duration_ms.is_some() => {
            reasons.push("Duration differs by more than 5 seconds".to_owned())
        }
        _ => reasons.push("TIDAL duration was unavailable".to_owned()),
    }

    match candidate.explicit {
        Some(explicit) if explicit == source.explicit => {
            score += 5;
            reasons.push("Explicit status matches (+5)".to_owned());
        }
        Some(_) => reasons.push("Explicit/clean status conflicts".to_owned()),
        None => reasons.push("TIDAL explicit status was unavailable".to_owned()),
    }

    if version_conflict {
        score -= 50;
        if title_version_conflict {
            reasons.push("Meaningful version indicator conflicts (-50)".to_owned());
        } else {
            reasons.push("Explicit/clean status conflict penalty (-50)".to_owned());
        }
    }

    ScoredCandidate {
        candidate,
        score: score.clamp(0, 100) as u8,
        reasons,
        version_conflict,
    }
}

fn candidate_title(candidate: &TidalTrackCandidate) -> String {
    match candidate.version.as_deref().map(str::trim) {
        Some(version) if !version.is_empty() => format!("{} {version}", candidate.title),
        _ => candidate.title.clone(),
    }
}

pub fn duration_score(source_ms: u64, candidate_ms: Option<u64>) -> u8 {
    let Some(candidate_ms) = candidate_ms else {
        return 0;
    };

    match source_ms.abs_diff(candidate_ms) {
        0..=2_000 => 10,
        2_001..=5_000 => 5,
        _ => 0,
    }
}

pub fn version_conflict(left: &str, right: &str) -> bool {
    version_indicators(left) != version_indicators(right)
}

fn version_indicators(value: &str) -> BTreeSet<&'static str> {
    let normalized = normalize(value);
    let tokens: Vec<_> = normalized.split_whitespace().collect();
    let mut indicators = BTreeSet::new();

    let contains = |word: &str| tokens.contains(&word);
    let contains_pair =
        |first: &str, second: &str| tokens.windows(2).any(|window| window == [first, second]);

    if contains("live") || contains_pair("en", "vivo") {
        indicators.insert("live");
    }
    if contains("acoustic") || contains("acustico") || contains("acustica") {
        indicators.insert("acoustic");
    }
    if contains("remix") || contains("mix") {
        indicators.insert("remix");
    }
    if contains("demo") {
        indicators.insert("demo");
    }
    if contains("karaoke") {
        indicators.insert("karaoke");
    }
    if contains("instrumental") {
        indicators.insert("instrumental");
    }
    if contains("remaster") || contains("remastered") || contains("remasterizado") {
        indicators.insert("remaster");
    }
    if contains_pair("radio", "edit") || contains_pair("radio", "version") {
        indicators.insert("radio-edit");
    }
    if contains("clean") {
        indicators.insert("clean");
    }

    indicators
}

#[cfg(test)]
mod tests {
    use crate::model::{MatchStatus, SourceTrack, TidalTrackCandidate};

    use super::{
        classify, duration_score, fallback_search_queries, score_candidate, version_conflict,
    };

    fn source() -> SourceTrack {
        SourceTrack {
            position: 1,
            added_at: None,
            spotify_id: Some("spotify-id".to_owned()),
            spotify_uri: "spotify:track:spotify-id".to_owned(),
            title: "Canción".to_owned(),
            artists: vec!["Artista".to_owned()],
            album: Some("Álbum".to_owned()),
            duration_ms: 180_000,
            isrc: Some("PEABC2600001".to_owned()),
            explicit: false,
            is_local: false,
        }
    }

    fn candidate() -> TidalTrackCandidate {
        TidalTrackCandidate {
            tidal_id: "tidal-id".to_owned(),
            title: "Cancion".to_owned(),
            version: None,
            isrc: Some("PEABC2600001".to_owned()),
            duration_ms: Some(180_900),
            explicit: Some(false),
            artists: vec!["Artista".to_owned()],
            album: Some("Album".to_owned()),
        }
    }

    #[test]
    fn detects_version_conflicts() {
        assert!(version_conflict("Canción", "Canción (En Vivo)"));
        assert!(version_conflict("Canción", "Canción acoustic"));
        assert!(version_conflict("Canción", "Canción remix"));
        assert!(version_conflict("Canción", "Canción - Remastered 2024"));
        assert!(!version_conflict("Canción (En Vivo)", "Cancion - Live"));
    }

    #[test]
    fn scores_duration_boundaries() {
        assert_eq!(duration_score(100_000, Some(102_000)), 10);
        assert_eq!(duration_score(100_000, Some(105_000)), 5);
        assert_eq!(duration_score(100_000, Some(105_001)), 0);
        assert_eq!(duration_score(100_000, None), 0);
    }

    #[test]
    fn exact_isrc_is_exact() {
        let scored = score_candidate(&source(), candidate());
        assert_eq!(scored.score, 100);
        assert_eq!(
            classify(scored.score, scored.version_conflict, false),
            MatchStatus::Exact
        );
    }

    #[test]
    fn builds_album_and_title_fallback_queries() {
        let mut track = source();
        track.title = "¿Para Qué Me Hablas?".to_owned();
        assert_eq!(
            fallback_search_queries(&track),
            vec!["¿Para Qué Me Hablas? Artista Álbum", "¿Para Qué Me Hablas?"]
        );

        let mut title_only = track;
        title_only.artists.clear();
        title_only.album = None;
        assert!(fallback_search_queries(&title_only).is_empty());
    }

    #[test]
    fn classifies_thresholds_and_uncertainty() {
        assert_eq!(classify(95, false, false), MatchStatus::Exact);
        assert_eq!(classify(94, false, false), MatchStatus::Probable);
        assert_eq!(classify(80, false, false), MatchStatus::Probable);
        assert_eq!(classify(79, false, false), MatchStatus::Review);
        assert_eq!(classify(55, false, false), MatchStatus::Review);
        assert_eq!(classify(54, false, false), MatchStatus::Missing);
        assert_eq!(classify(100, true, false), MatchStatus::Review);
        assert_eq!(classify(90, false, true), MatchStatus::Review);
    }
}
