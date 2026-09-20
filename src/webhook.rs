//! Webhook: receives GitHub webhook requests.
//!
//! Verifies the HMAC signature, filters for review-worthy `pull_request`
//! events, and hands them off to the review workflow.

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;
use subtle::ConstantTimeEq;
use tracing::{error, warn};

use crate::{review, AppState};

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Deserialize)]
struct PullRequestEvent {
    action: String,
    repository: Repository,
    pull_request: PullRequest,
    installation: Installation,
}

#[derive(Debug, Deserialize)]
struct Installation {
    id: u64,
}

#[derive(Debug, Deserialize)]
struct Repository {
    full_name: String,
}

#[derive(Debug, Deserialize)]
struct PullRequest {
    number: u64,
    draft: Option<bool>,
    user: User,
}

#[derive(Debug, Deserialize)]
struct User {
    login: String,
}

pub(crate) async fn github_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if !valid_signature(&state.webhook_secret, &headers, &body) {
        return StatusCode::UNAUTHORIZED;
    }

    if headers
        .get("x-github-event")
        .and_then(|value| value.to_str().ok())
        != Some("pull_request")
    {
        return StatusCode::NO_CONTENT;
    }

    let event: PullRequestEvent = match serde_json::from_slice(&body) {
        Ok(event) => event,
        Err(error) => {
            warn!(%error, "invalid pull_request webhook payload");
            return StatusCode::BAD_REQUEST;
        }
    };

    if !matches!(event.action.as_str(), "opened" | "reopened" | "synchronize") {
        return StatusCode::NO_CONTENT;
    }

    if event.pull_request.draft.unwrap_or(false) {
        return StatusCode::NO_CONTENT;
    }

    if !event
        .pull_request
        .user
        .login
        .eq_ignore_ascii_case(&state.github_username)
    {
        return StatusCode::NO_CONTENT;
    }

    let state_for_task = state.clone();
    tokio::spawn(async move {
        let repository = event.repository.full_name;
        let number = event.pull_request.number;
        let author = event.pull_request.user.login;
        let installation_id = event.installation.id;
        if let Err(error) = review::review_pull_request(
            &state_for_task,
            &repository,
            number,
            &author,
            installation_id,
        )
        .await
        {
            error!(%error, %repository, number, "PR review failed");
        }
    });

    StatusCode::ACCEPTED
}

fn valid_signature(secret: &str, headers: &HeaderMap, body: &[u8]) -> bool {
    let Some(signature) = headers
        .get("x-hub-signature-256")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("sha256="))
    else {
        return false;
    };

    let Ok(expected) = hex::decode(signature) else {
        return false;
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.finalize()
        .into_bytes()
        .as_slice()
        .ct_eq(&expected)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_github_signature() {
        let secret = "secret";
        let body = br#"{"ok":true}"#;
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        let mut headers = HeaderMap::new();
        headers.insert("x-hub-signature-256", signature.parse().unwrap());

        assert!(valid_signature(secret, &headers, body));
    }

    #[test]
    fn rejects_invalid_github_signature() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-hub-signature-256",
            "sha256=0000000000000000000000000000000000000000000000000000000000000000"
                .parse()
                .unwrap(),
        );

        assert!(!valid_signature("secret", &headers, b"body"));
    }
}
