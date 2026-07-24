use std::{
    collections::HashMap,
    env,
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::{Client, StatusCode, header::ACCEPT};
use serde::Deserialize;
use serde_json::Value;
use tokio::{
    sync::Mutex,
    time::{Instant, sleep},
};

use crate::model::TidalTrackCandidate;

const TIDAL_TOKEN_URL: &str = "https://auth.tidal.com/v1/oauth2/token";
const TIDAL_API_URL: &str = "https://openapi.tidal.com/v2";
const TIDAL_MEDIA_TYPE: &str = "application/vnd.api+json";
const MAX_ATTEMPTS: usize = 3;

#[derive(Debug, Deserialize)]
struct TidalTokenResponse {
    access_token: String,
}

pub struct TidalClient {
    client: Client,
    access_token: String,
    country_code: String,
    request_gate: RequestGate,
}

struct RequestGate {
    state: Mutex<RequestGateState>,
    base_interval: Duration,
}

struct RequestGateState {
    next_start: Instant,
    interval: Duration,
    cooldown_until: Option<Instant>,
}

impl RequestGate {
    fn new(interval: Duration) -> Self {
        Self {
            state: Mutex::new(RequestGateState {
                next_start: Instant::now(),
                interval,
                cooldown_until: None,
            }),
            base_interval: interval,
        }
    }

    async fn wait_for_slot(&self) {
        let mut state = self.state.lock().await;
        let now = Instant::now();
        if state.next_start > now {
            sleep(state.next_start - now).await;
        }
        state.next_start = Instant::now() + state.interval;
    }

    async fn apply_rate_limit(&self, duration: Duration) -> (Duration, bool) {
        let mut state = self.state.lock().await;
        let now = Instant::now();
        let cooldown_end = now + duration;
        let is_new_incident = state
            .cooldown_until
            .is_none_or(|current_cooldown| now >= current_cooldown);

        if cooldown_end > state.next_start {
            state.next_start = cooldown_end;
        }
        state.cooldown_until = Some(
            state
                .cooldown_until
                .map_or(cooldown_end, |current| current.max(cooldown_end)),
        );

        if is_new_incident {
            state.interval = state.interval.saturating_mul(2).min(Duration::from_secs(2));
        }

        (state.interval, is_new_incident)
    }

    async fn record_success(&self) {
        let mut state = self.state.lock().await;
        if state.interval > self.base_interval {
            state.interval = state
                .interval
                .saturating_sub(Duration::from_millis(1))
                .max(self.base_interval);
        }
    }
}

impl TidalClient {
    pub async fn from_env() -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("Could not create the TIDAL HTTP client")?;
        let token = request_client_credentials_token(&client).await?;
        let country_code = env::var("TIDAL_COUNTRY_CODE").unwrap_or_else(|_| "PE".to_owned());
        let country_code = country_code.trim().to_ascii_uppercase();

        if country_code.len() != 2
            || !country_code
                .chars()
                .all(|value| value.is_ascii_alphabetic())
        {
            bail!("TIDAL_COUNTRY_CODE must be a two-letter country code");
        }

        Ok(Self {
            client,
            access_token: token.access_token,
            country_code,
            request_gate: RequestGate::new(search_interval()),
        })
    }

    pub fn country_code(&self) -> &str {
        &self.country_code
    }

    pub async fn search_tracks(
        &self,
        title: &str,
        artists: &[String],
    ) -> Result<Vec<TidalTrackCandidate>> {
        let query = match artists.first() {
            Some(artist) if !artist.trim().is_empty() => format!("{title} {artist}"),
            _ => title.to_owned(),
        };

        let mut url = reqwest::Url::parse(TIDAL_API_URL)?;
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|_| anyhow!("Could not construct the TIDAL search URL"))?;
            // This public endpoint is case-sensitive.
            segments.push("searchResults");
            segments.push(&query);
        }
        url.query_pairs_mut()
            .append_pair("countryCode", &self.country_code)
            .append_pair("include", "tracks");

        let mut last_network_error = None;

        for attempt in 1..=MAX_ATTEMPTS {
            self.request_gate.wait_for_slot().await;
            let response = self
                .client
                .get(url.clone())
                .header(ACCEPT, TIDAL_MEDIA_TYPE)
                .bearer_auth(&self.access_token)
                .send()
                .await;

            let response = match response {
                Ok(response) => response,
                Err(error)
                    if attempt < MAX_ATTEMPTS && (error.is_timeout() || error.is_connect()) =>
                {
                    last_network_error = Some(error);
                    sleep(retry_backoff(attempt)).await;
                    continue;
                }
                Err(error) => {
                    return Err(error).context("Could not contact the TIDAL catalog API");
                }
            };

            let status = response.status();
            let retry_after = retry_after(&response);
            let body = response
                .text()
                .await
                .context("Could not read the TIDAL catalog response")?;

            if status.is_success() {
                self.request_gate.record_success().await;
                return parse_search_response(&body);
            }

            if status == StatusCode::TOO_MANY_REQUESTS && attempt < MAX_ATTEMPTS {
                let cooldown = retry_after.unwrap_or_else(|| retry_backoff(attempt));
                let (interval, is_new_incident) =
                    self.request_gate.apply_rate_limit(cooldown).await;
                if is_new_incident {
                    eprintln!(
                        "TIDAL rate limit reached; pausing catalog searches for {} ms and increasing request spacing to {} ms.",
                        cooldown.as_millis(),
                        interval.as_millis()
                    );
                }
                continue;
            }

            if status.is_server_error() && attempt < MAX_ATTEMPTS {
                sleep(retry_backoff(attempt)).await;
                continue;
            }

            bail!(
                "TIDAL catalog search failed with HTTP {status}: {}",
                safe_response_excerpt(&body, &self.access_token)
            );
        }

        Err(last_network_error
            .map(anyhow::Error::from)
            .unwrap_or_else(|| {
                anyhow!("TIDAL catalog search failed after {MAX_ATTEMPTS} attempts")
            }))
    }
}

fn search_interval() -> Duration {
    let milliseconds = env::var("TIDAL_SEARCH_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(150);

    Duration::from_millis(milliseconds)
}

pub async fn test_catalog() -> Result<()> {
    let client = TidalClient::from_env().await?;

    println!("TIDAL authentication succeeded.");
    println!(
        "Testing TIDAL catalog search for market {}...",
        client.country_code()
    );

    let candidates = client
        .search_tracks("Los Outsaiders", &["Los Outsaiders".to_owned()])
        .await?;

    println!("TIDAL catalog search succeeded.");
    println!("Included track resources: {}", candidates.len());
    for candidate in candidates.iter().take(5) {
        println!("- {} [{}]", candidate.title, candidate.tidal_id);
    }

    Ok(())
}

async fn request_client_credentials_token(client: &Client) -> Result<TidalTokenResponse> {
    let client_id = env::var("TIDAL_CLIENT_ID").context("TIDAL_CLIENT_ID is missing from .env")?;
    let client_secret =
        env::var("TIDAL_CLIENT_SECRET").context("TIDAL_CLIENT_SECRET is missing from .env")?;

    let response = client
        .post(TIDAL_TOKEN_URL)
        .basic_auth(&client_id, Some(&client_secret))
        .form(&[("grant_type", "client_credentials")])
        .send()
        .await
        .context("Could not contact TIDAL's token endpoint")?;
    let status = response.status();

    if !status.is_success() {
        // Authentication error bodies are deliberately omitted: some OAuth
        // servers echo submitted client information in diagnostic responses.
        bail!("TIDAL authentication failed with HTTP {status}");
    }

    response
        .json()
        .await
        .context("TIDAL returned an invalid token response")
}

fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    let value = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)?;

    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }

    httpdate::parse_http_date(value)
        .ok()
        .and_then(|date| date.duration_since(SystemTime::now()).ok())
}

fn retry_backoff(attempt: usize) -> Duration {
    Duration::from_millis(300 * attempt as u64)
}

fn safe_response_excerpt(body: &str, access_token: &str) -> String {
    if body.contains(access_token) {
        return "response omitted because it contained credential data".to_owned();
    }

    body.chars().take(2_000).collect()
}

fn parse_search_response(body: &str) -> Result<Vec<TidalTrackCandidate>> {
    let document: Value = serde_json::from_str(body).context("TIDAL returned invalid JSON")?;
    let included = document
        .get("included")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let resources: HashMap<(&str, &str), &Value> = included
        .iter()
        .filter_map(|resource| {
            Some((
                (
                    resource.get("type")?.as_str()?,
                    resource.get("id")?.as_str()?,
                ),
                resource,
            ))
        })
        .collect();

    Ok(included
        .iter()
        .filter(|resource| {
            resource
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.eq_ignore_ascii_case("tracks"))
        })
        // A malformed optional resource must not discard otherwise usable
        // candidates returned in the same search document.
        .filter_map(|resource| parse_track_resource(resource, &resources).ok())
        .collect())
}

fn parse_track_resource(
    resource: &Value,
    resources: &HashMap<(&str, &str), &Value>,
) -> Result<TidalTrackCandidate> {
    let id = resource
        .get("id")
        .and_then(Value::as_str)
        .context("A TIDAL track resource did not contain a string id")?;
    let attributes = resource.get("attributes").unwrap_or(&Value::Null);
    let title =
        string_at(attributes, "title").context("A TIDAL track resource did not contain a title")?;

    let mut artists = direct_artist_names(attributes);
    if artists.is_empty() {
        artists = related_names(resource, "artists", resources);
    }

    let album = string_at(attributes, "albumTitle")
        .or_else(|| string_at(attributes, "album"))
        .map(str::to_owned)
        .or_else(|| {
            related_names(resource, "albums", resources)
                .into_iter()
                .next()
        });

    Ok(TidalTrackCandidate {
        tidal_id: id.to_owned(),
        title: title.to_owned(),
        version: string_at(attributes, "version")
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned),
        isrc: string_at(attributes, "isrc").map(str::to_owned),
        duration_ms: attributes.get("duration").and_then(parse_duration_ms),
        explicit: attributes.get("explicit").and_then(Value::as_bool),
        artists,
        album,
    })
}

fn string_at<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn direct_artist_names(attributes: &Value) -> Vec<String> {
    attributes
        .get("artists")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|artist| {
            artist
                .get("name")
                .and_then(Value::as_str)
                .or_else(|| artist.as_str())
                .map(str::to_owned)
        })
        .collect()
}

fn related_names(
    resource: &Value,
    relationship: &str,
    resources: &HashMap<(&str, &str), &Value>,
) -> Vec<String> {
    let Some(data) = resource.pointer(&format!("/relationships/{relationship}/data")) else {
        return Vec::new();
    };

    let identifiers: Vec<&Value> = match data {
        Value::Array(values) => values.iter().collect(),
        Value::Object(_) => vec![data],
        _ => Vec::new(),
    };

    identifiers
        .into_iter()
        .filter_map(|identifier| {
            let kind = identifier.get("type")?.as_str()?;
            let id = identifier.get("id")?.as_str()?;
            let related = resources.get(&(kind, id))?;
            related
                .pointer("/attributes/name")
                .or_else(|| related.pointer("/attributes/title"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect()
}

fn parse_duration_ms(value: &Value) -> Option<u64> {
    if let Some(number) = value.as_u64() {
        return Some(if number <= 86_400 {
            number * 1_000
        } else {
            number
        });
    }

    let duration = value.as_str()?.trim();
    if let Ok(number) = duration.parse::<u64>() {
        return Some(if number <= 86_400 {
            number * 1_000
        } else {
            number
        });
    }

    parse_iso_8601_duration_ms(duration)
}

fn parse_iso_8601_duration_ms(value: &str) -> Option<u64> {
    let value = value.strip_prefix("PT")?;
    let mut number = String::new();
    let mut seconds = 0_f64;

    for character in value.chars() {
        if character.is_ascii_digit() || character == '.' {
            number.push(character);
            continue;
        }

        let amount = number.parse::<f64>().ok()?;
        number.clear();
        seconds += match character {
            'H' => amount * 3_600.0,
            'M' => amount * 60.0,
            'S' => amount,
            _ => return None,
        };
    }

    if !number.is_empty() || !seconds.is_finite() || seconds < 0.0 {
        return None;
    }

    Some((seconds * 1_000.0).round() as u64)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{RequestGate, parse_duration_ms, parse_search_response};
    use serde_json::json;

    #[test]
    fn parses_supported_duration_shapes() {
        assert_eq!(parse_duration_ms(&json!(201)), Some(201_000));
        assert_eq!(parse_duration_ms(&json!(201_234)), Some(201_234));
        assert_eq!(parse_duration_ms(&json!("PT3M21.5S")), Some(201_500));
    }

    #[test]
    fn parses_track_resources_with_optional_fields() {
        let response = r#"{
          "data": {"type": "searchResults", "id": "test"},
          "included": [{
            "type": "tracks",
            "id": "123",
            "attributes": {
              "title": "¿Para Qué Me Hablas?",
              "isrc": "PEABC2600001",
              "duration": "PT3M21S",
              "explicit": false
            },
            "relationships": {}
          }]
        }"#;

        let candidates = parse_search_response(response).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].tidal_id, "123");
        assert_eq!(candidates[0].duration_ms, Some(201_000));
        assert!(candidates[0].artists.is_empty());
        assert_eq!(candidates[0].album, None);
    }

    #[tokio::test]
    async fn coalesces_rate_limits_from_the_same_in_flight_burst() {
        let gate = RequestGate::new(Duration::from_millis(150));

        assert_eq!(
            gate.apply_rate_limit(Duration::from_secs(1)).await,
            (Duration::from_millis(300), true)
        );
        assert_eq!(
            gate.apply_rate_limit(Duration::from_secs(1)).await,
            (Duration::from_millis(300), false)
        );

        gate.state.lock().await.cooldown_until = Some(
            tokio::time::Instant::now()
                .checked_sub(Duration::from_millis(1))
                .unwrap(),
        );
        assert_eq!(
            gate.apply_rate_limit(Duration::from_secs(1)).await,
            (Duration::from_millis(600), true)
        );
    }
}
