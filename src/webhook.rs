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

use crate::{
    review::{review_pull_request, ReviewRequest},
    AppState,
};

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
    clone_url: String,
}

#[derive(Debug, Deserialize)]
struct PullRequest {
    number: u64,
    draft: Option<bool>,
    user: User,
    base: GitRef,
    head: GitRef,
}

#[derive(Debug, Deserialize)]
struct GitRef {
    #[serde(rename = "ref")]
    name: String,
    sha: String,
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

    let request = match review_request(&event, &state.github_username) {
        Some(request) => request,
        None => return StatusCode::NO_CONTENT,
    };

    tokio::spawn(async move {
        let full_name = request.full_name.clone();
        let number = request.number;
        if let Err(error) = review_pull_request(&state, &request).await {
            error!(%error, %full_name, number, "PR review failed");
        }
    });

    StatusCode::ACCEPTED
}

/// Turn a webhook event into a review request, or `None` when the event is
/// not one this bot reviews: wrong action, draft PR, or another author.
fn review_request(event: &PullRequestEvent, username: &str) -> Option<ReviewRequest> {
    if !matches!(event.action.as_str(), "opened" | "reopened" | "synchronize") {
        return None;
    }
    if event.pull_request.draft.unwrap_or(false) {
        return None;
    }
    if !event.pull_request.user.login.eq_ignore_ascii_case(username) {
        return None;
    }
    Some(ReviewRequest {
        full_name: event.repository.full_name.clone(),
        clone_url: event.repository.clone_url.clone(),
        number: event.pull_request.number,
        author: event.pull_request.user.login.clone(),
        base_ref: event.pull_request.base.name.clone(),
        head_sha: event.pull_request.head.sha.clone(),
        installation_id: event.installation.id,
    })
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

    fn event(action: &str, draft: bool, author: &str) -> PullRequestEvent {
        serde_json::from_value(serde_json::json!({
            "action": action,
            "installation": {"id": 42},
            "repository": {
                "full_name": "alex-heritier/review-bot",
                "clone_url": "https://github.com/alex-heritier/review-bot.git"
            },
            "pull_request": {
                "number": 7,
                "draft": draft,
                "user": {"login": author},
                "base": {"ref": "master", "sha": "aaa"},
                "head": {"ref": "feature", "sha": "bbb"}
            }
        }))
        .unwrap()
    }

    #[test]
    fn eligible_event_becomes_full_review_request() {
        let request = review_request(&event("opened", false, "Alex-Heritier"), "alex-heritier")
            .expect("opened PR by owner is reviewed (case-insensitive)");
        assert_eq!(request.full_name, "alex-heritier/review-bot");
        assert_eq!(
            request.clone_url,
            "https://github.com/alex-heritier/review-bot.git"
        );
        assert_eq!(request.number, 7);
        assert_eq!(request.base_ref, "master");
        assert_eq!(request.head_sha, "bbb");
        assert_eq!(request.installation_id, 42);
    }

    #[test]
    fn ineligible_events_are_dropped() {
        assert!(
            review_request(&event("closed", false, "alex-heritier"), "alex-heritier").is_none()
        );
        assert!(
            review_request(&event("opened", true, "alex-heritier"), "alex-heritier").is_none()
        );
        assert!(review_request(
            &event("synchronize", false, "someone-else"),
            "alex-heritier"
        )
        .is_none());
        assert!(
            review_request(&event("reopened", false, "alex-heritier"), "alex-heritier").is_some()
        );
        assert!(review_request(
            &event("synchronize", false, "alex-heritier"),
            "alex-heritier"
        )
        .is_some());
    }
}
