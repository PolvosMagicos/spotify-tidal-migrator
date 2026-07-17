use std::env;

use anyhow::{Context, Result, bail};
use reqwest::{Client, header::ACCEPT};
use serde::Deserialize;
use serde_json::Value;

const TIDAL_TOKEN_URL: &str = "https://auth.tidal.com/v1/oauth2/token";
const TIDAL_API_URL: &str = "https://openapi.tidal.com/v2";
const TIDAL_MEDIA_TYPE: &str = "application/vnd.api+json";

#[derive(Debug, Deserialize)]
struct TidalTokenResponse {
    access_token: String,
    token_type: String,
    expires_in: u64,

    #[serde(default)]
    scope: String,
}

pub async fn test_catalog() -> Result<()> {
    let client = Client::new();
    let token = request_client_credentials_token(&client).await?;

    println!("TIDAL authentication succeeded.");
    println!("Token type: {}", token.token_type);
    println!("Expires in: {} seconds", token.expires_in);

    if !token.scope.is_empty() {
        println!("Scopes: {}", token.scope);
    }

    let country_code = env::var("TIDAL_COUNTRY_CODE").unwrap_or_else(|_| "PE".to_owned());

    let query = "Los Outsaiders";

    let mut url = reqwest::Url::parse(TIDAL_API_URL)?;

    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("Could not construct TIDAL search URL"))?;

        segments.push("searchResults");
        segments.push(query);
    }

    url.query_pairs_mut()
        .append_pair("countryCode", &country_code)
        .append_pair("include", "tracks");

    println!("Testing TIDAL catalog search for market {country_code}...");
    println!("Request: {url}");

    let response = client
        .get(url.clone())
        .header(ACCEPT, TIDAL_MEDIA_TYPE)
        .bearer_auth(&token.access_token)
        .send()
        .await
        .context("Could not contact the TIDAL catalog API")?;

    let status = response.status();
    let body = response
        .text()
        .await
        .context("Could not read the TIDAL API response")?;

    if !status.is_success() {
        bail!(
            "TIDAL catalog search failed with HTTP {status}.\n\
         Request: {url}\n\
         Response: {body}"
        );
    }

    let document: Value = serde_json::from_str(&body).context("TIDAL returned invalid JSON")?;

    let resource_type = document
        .pointer("/data/type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");

    let included = document.get("included").and_then(Value::as_array);

    let included_count = included.map_or(0, Vec::len);

    println!();
    println!("TIDAL catalog search succeeded.");
    println!("Resource type: {resource_type}");
    println!("Included resources: {included_count}");

    if let Some(resources) = included {
        for resource in resources.iter().take(5) {
            let id = resource
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("unknown");

            let title = resource
                .pointer("/attributes/title")
                .and_then(Value::as_str)
                .unwrap_or("title unavailable");

            println!("- {title} [{id}]");
        }
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
    let body = response
        .text()
        .await
        .context("Could not read TIDAL's token response")?;

    if !status.is_success() {
        bail!("TIDAL authentication failed with HTTP {status}:\n{body}");
    }

    serde_json::from_str(&body).context("TIDAL returned an invalid token response")
}
