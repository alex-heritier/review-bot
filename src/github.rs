//! GitHub: talks to the GitHub REST API on behalf of the App.
//!
//! Mints App JWTs and installation tokens and posts PR reviews, optionally
//! with batched inline comments pinned to a reviewed commit.

use anyhow::{Context, Result};
use axum::http::header;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::AppState;

const GITHUB_API_URL: &str = "https://api.github.com";
const GITHUB_API_VERSION: &str = "2022-11-28";

/// GitHub caps what one createReview request can carry; their official
/// action splits into sequential batches after a run failed on 71 comments.
const COMMENT_BATCH: usize = 50;

/// One resolved inline comment, ready for the reviews API.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CommentSpec {
    pub(crate) path: String,
    pub(crate) body: String,
    pub(crate) start_line: Option<u64>,
    pub(crate) line: Option<u64>,
}

#[derive(Debug, Serialize)]
struct GithubReview<'a> {
    body: &'a str,
    event: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    comments: Vec<InlineComment<'a>>,
}

#[derive(Debug, Serialize)]
struct InlineComment<'a> {
    path: &'a str,
    body: &'a str,
    side: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_side: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_line: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<u64>,
}

impl<'a> From<&'a CommentSpec> for InlineComment<'a> {
    fn from(spec: &'a CommentSpec) -> Self {
        Self {
            path: &spec.path,
            body: &spec.body,
            side: "RIGHT",
            start_side: spec.start_line.map(|_| "RIGHT"),
            start_line: spec.start_line,
            line: spec.line,
        }
    }
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

/// Post a COMMENT review, batching inline comments so no single request
/// exceeds GitHub's practical limits. Returns the number of inline comments
/// accepted.
pub(crate) async fn post_review(
    state: &AppState,
    token: &str,
    repository: &str,
    number: u64,
    commit_id: Option<&str>,
    body: &str,
    comments: &[CommentSpec],
) -> Result<usize> {
    let url = format!("{GITHUB_API_URL}/repos/{repository}/pulls/{number}/reviews");
    let batches: Vec<&[CommentSpec]> = if comments.is_empty() {
        vec![&[]]
    } else {
        comments.chunks(COMMENT_BATCH).collect()
    };
    let mut posted = 0;
    for (index, batch) in batches.iter().enumerate() {
        let batch_body = match index {
            0 => body.to_owned(),
            _ => format!("More findings ({}/{})", index + 1, batches.len()),
        };
        let review = GithubReview {
            body: &batch_body,
            event: "COMMENT",
            commit_id,
            comments: batch.iter().map(InlineComment::from).collect(),
        };
        state
            .client
            .post(&url)
            .bearer_auth(token)
            .header(header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
            .json(&review)
            .send()
            .await?
            .error_for_status()
            .context("GitHub rejected the review")?;
        posted += batch.len();
    }
    Ok(posted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_json_omits_absent_fields() {
        let review = GithubReview {
            body: "summary",
            event: "COMMENT",
            commit_id: None,
            comments: vec![],
        };
        let value = serde_json::to_value(&review).unwrap();
        assert_eq!(value["event"], "COMMENT");
        assert!(value.get("commit_id").is_none());
        assert!(value.get("comments").is_none());
    }

    #[test]
    fn inline_comments_carry_right_side_positioning() {
        let spec = CommentSpec {
            path: "src/a.rs".to_owned(),
            body: "bad".to_owned(),
            start_line: Some(3),
            line: Some(5),
        };
        let value = serde_json::to_value(InlineComment::from(&spec)).unwrap();
        assert_eq!(value["path"], "src/a.rs");
        assert_eq!(value["side"], "RIGHT");
        assert_eq!(value["start_side"], "RIGHT");
        assert_eq!(value["start_line"], 3);
        assert_eq!(value["line"], 5);

        let single = CommentSpec {
            path: "src/a.rs".to_owned(),
            body: "bad".to_owned(),
            start_line: None,
            line: Some(9),
        };
        let value = serde_json::to_value(InlineComment::from(&single)).unwrap();
        assert!(value.get("start_side").is_none());
        assert!(value.get("start_line").is_none());
        assert_eq!(value["line"], 9);
    }
}
