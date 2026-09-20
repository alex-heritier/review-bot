//! GitHub: talks to the GitHub REST API on behalf of the App.
//!
//! Mints App JWTs and installation tokens, fetches PR diffs, and posts reviews.

use anyhow::{Context, Result};
use axum::http::header;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::AppState;

const GITHUB_API_URL: &str = "https://api.github.com";
const GITHUB_API_VERSION: &str = "2022-11-28";

#[derive(Debug, Serialize)]
struct GithubReview<'a> {
    body: &'a str,
    event: &'static str,
}

#[derive(Debug, Deserialize)]
struct InstallationToken {
    token: String,
}

#[derive(Debug, Serialize)]
struct AppClaims {
    iat: i64,
    exp: i64,
    iss: u64,
}

pub(crate) async fn installation_token(state: &AppState, installation_id: u64) -> Result<String> {
    let jwt = app_jwt(state)?;
    let url = format!("{GITHUB_API_URL}/app/installations/{installation_id}/access_tokens");
    let response: InstallationToken = state
        .client
        .post(url)
        .bearer_auth(jwt)
        .header(header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
        .send()
        .await?
        .error_for_status()
        .context("GitHub rejected the App installation token request")?
        .json()
        .await?;
    Ok(response.token)
}

fn app_jwt(state: &AppState) -> Result<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_secs() as i64;
    let claims = AppClaims {
        iat: now - 60,
        exp: now + (9 * 60),
        iss: state.github_app_id,
    };
    let key = EncodingKey::from_rsa_pem(state.github_private_key.as_ref())
        .context("invalid GitHub App private key")?;
    encode(&Header::new(Algorithm::RS256), &claims, &key).context("could not sign GitHub App JWT")
}

pub(crate) async fn fetch_diff(
    state: &AppState,
    token: &str,
    repository: &str,
    number: u64,
) -> Result<String> {
    let diff_url = format!("{GITHUB_API_URL}/repos/{repository}/pulls/{number}.diff");
    let diff = state
        .client
        .get(diff_url)
        .bearer_auth(token)
        .header(header::ACCEPT, "application/vnd.github.v3.diff")
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    Ok(diff)
}

pub(crate) async fn post_review(
    state: &AppState,
    token: &str,
    repository: &str,
    number: u64,
    review: &str,
) -> Result<()> {
    let review_url = format!("{GITHUB_API_URL}/repos/{repository}/pulls/{number}/reviews");
    state
        .client
        .post(review_url)
        .bearer_auth(token)
        .header(header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
        .json(&GithubReview {
            body: review,
            event: "COMMENT",
        })
        .send()
        .await?
        .error_for_status()
        .context("GitHub rejected the review")?;
    Ok(())
}
