//! Review: the review workflow.
//!
//! Orchestrates one full review: token, diff, truncation, LLM review, post.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::{github, AppState};

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: [ChatMessage<'a>; 2],
    temperature: f32,
}

#[derive(Debug, Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: Message,
}

#[derive(Debug, Deserialize)]
struct Message {
    content: String,
}

pub(crate) async fn review_pull_request(
    state: &AppState,
    repository: &str,
    number: u64,
    author: &str,
    installation_id: u64,
) -> Result<()> {
    info!(%repository, number, %author, "reviewing pull request");
    let token = github::installation_token(state, installation_id).await?;

    let diff = github::fetch_diff(state, &token, repository, number).await?;
    let diff = if diff.len() > state.max_diff_chars {
        let end = diff_boundary(&diff, state.max_diff_chars);
        format!(
            "{}\n\n[Diff truncated at {} characters.]",
            &diff[..end],
            state.max_diff_chars
        )
    } else {
        diff
    };

    let review = generate_review(state, repository, number, author, &diff).await?;
    github::post_review(state, &token, repository, number, &review).await?;

    info!(%repository, number, "review posted");
    Ok(())
}

fn diff_boundary(diff: &str, max_chars: usize) -> usize {
    let mut end = max_chars.min(diff.len());
    while !diff.is_char_boundary(end) {
        end -= 1;
    }
    end
}

async fn generate_review(
    state: &AppState,
    repository: &str,
    number: u64,
    author: &str,
    diff: &str,
) -> Result<String> {
    let system = "You are a pragmatic senior code reviewer. Review the supplied GitHub pull request diff. Focus only on concrete bugs, security issues, data loss, and important maintainability problems introduced by this change. Do not praise the code or invent issues. Return a concise Markdown review suitable for posting as one GitHub PR review. If there are no actionable findings, say exactly: No actionable issues found.";
    let prompt = format!(
        "Repository: {repository}\nPull request: #{number}\nAuthor: {author}\n\nDiff:\n```diff\n{diff}\n```"
    );
    let request = ChatRequest {
        model: &state.llm_model,
        messages: [
            ChatMessage {
                role: "system",
                content: system,
            },
            ChatMessage {
                role: "user",
                content: &prompt,
            },
        ],
        temperature: 0.1,
    };

    let url = format!("{}/chat/completions", state.llm_base_url);
    let response: ChatResponse = state
        .client
        .post(url)
        .bearer_auth(&state.llm_api_key)
        .json(&request)
        .send()
        .await?
        .error_for_status()
        .context("LLM request failed")?
        .json()
        .await?;

    response
        .choices
        .into_iter()
        .next()
        .map(|choice| choice.message.content.trim().to_owned())
        .filter(|review| !review.is_empty())
        .ok_or_else(|| anyhow!("LLM returned no review text"))
}
