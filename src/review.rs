//! Review: drives the `ocr` engine (github.com/alibaba/open-code-review).
//!
//! One full review: mint an installation token, sync the PR into a local
//! repo cache, run `ocr review --format json`, then publish the findings as
//! inline GitHub review comments pinned to the reviewed head commit.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use tokio::process::Command;
use tracing::{info, warn};

use crate::{github, AppState};

/// Everything the review needs from the webhook event.
#[derive(Debug, Clone)]
pub(crate) struct ReviewRequest {
    pub(crate) full_name: String,
    pub(crate) clone_url: String,
    pub(crate) number: u64,
    pub(crate) author: String,
    pub(crate) base_ref: String,
    pub(crate) head_sha: String,
    pub(crate) installation_id: u64,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OcrResult {
    pub(crate) status: String,
    pub(crate) message: Option<String>,
    pub(crate) summary: OcrSummary,
    #[serde(default)]
    pub(crate) comments: Vec<OcrComment>,
    manifest: OcrManifest,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct OcrSummary {
    #[serde(default)]
    files_reviewed: usize,
    #[serde(default)]
    comments: usize,
    elapsed: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OcrManifest {
    input: OcrInput,
}

#[derive(Debug, Deserialize)]
struct OcrInput {
    resolved_head: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub(crate) struct OcrComment {
    pub(crate) path: String,
    pub(crate) content: String,
    existing_code: Option<String>,
    suggestion_code: Option<String>,
    start_line: Option<u64>,
    end_line: Option<u64>,
    severity: Option<String>,
    category: Option<String>,
}

pub(crate) async fn review_pull_request(state: &AppState, request: &ReviewRequest) -> Result<()> {
    // One review at a time: keeps repo caches, the shared ocr config HOME,
    // and GitHub comment ordering conflict-free. This bot is single-user.
    let _guard = state.review_lock.lock().await;

    info!(
        repository = %request.full_name,
        number = request.number,
        author = %request.author,
        "reviewing pull request"
    );
    let token = github::installation_token(state, request.installation_id).await?;
    let workdir = repo_workspace(&request.full_name)?;
    sync_repository(&token, request, &workdir).await?;

    let result_path = cache_root()?
        .join("results")
        .join(format!("pr-{}.json", request.number));
    let result = run_ocr(state, request, &workdir, &result_path).await?;
    let _ = fs::remove_file(&result_path);

    if result.status != "complete" {
        warn!(
            status = %result.status,
            message = result.message.as_deref().unwrap_or(""),
            repository = %request.full_name,
            number = request.number,
            "ocr reported an incomplete review; publishing partial results"
        );
    }

    let mut inline = Vec::new();
    let mut overflow = Vec::new();
    for comment in &result.comments {
        match to_review_comment(comment) {
            Some(spec) => inline.push(spec),
            None => overflow.push(comment),
        }
    }
    let body = review_summary(&result, &overflow);
    let commit_id = result
        .manifest
        .input
        .resolved_head
        .as_deref()
        .filter(|sha| !sha.is_empty());

    let posted = github::post_review(
        state,
        &token,
        &request.full_name,
        request.number,
        commit_id,
        &body,
        &inline,
    )
    .await?;

    info!(
        repository = %request.full_name,
        number = request.number,
        findings = result.comments.len(),
        inline = posted,
        "review posted"
    );
    Ok(())
}

/// Findings without usable line information cannot anchor inline; the
/// review summary carries them instead.
fn to_review_comment(comment: &OcrComment) -> Option<github::CommentSpec> {
    let start = comment.start_line.filter(|n| *n >= 1);
    let end = comment.end_line.filter(|n| *n >= 1);
    if start.is_none() && end.is_none() {
        return None;
    }
    // Single-line when both bounds coincide or only one exists; GitHub
    // rejects a range whose start equals its end.
    let line = end.or(start);
    let start_line = start.zip(line).filter(|(s, l)| s != l).map(|(s, _)| s);
    Some(github::CommentSpec {
        path: comment.path.clone(),
        body: comment_body(comment),
        start_line,
        line,
    })
}

/// Render one finding as GitHub-flavored Markdown for an inline comment.
fn comment_body(comment: &OcrComment) -> String {
    let mut body = String::new();
    let badge = [
        comment.severity.as_deref().map(|s| format!("**{s}**")),
        comment.category.as_deref().map(|c| format!("_{c}_")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");
    if !badge.is_empty() {
        body.push_str(&badge);
        body.push('\n');
    }
    body.push_str(&comment.content);
    if let (Some(existing), Some(suggestion)) = (&comment.existing_code, &comment.suggestion_code)
    {
        body.push_str("\n\n<details><summary>Suggested change</summary>\n\n**Before:**\n```\n");
        push_fenced(&mut body, existing);
        body.push_str("**After:**\n```\n");
        push_fenced(&mut body, suggestion);
        body.push_str("\n</details>");
    }
    body
}

fn push_fenced(body: &mut String, code: &str) {
    body.push_str(code);
    if !code.ends_with('\n') {
        body.push('\n');
    }
    body.push_str("```\n");
}

/// Review body: run status header plus findings that could not go inline.
fn review_summary(result: &OcrResult, overflow: &[&OcrComment]) -> String {
    let mut body = format!(
        "🤖 Reviewed {} file(s) with ocr: {} finding(s).",
        result.summary.files_reviewed, result.summary.comments
    );
    if let Some(elapsed) = &result.summary.elapsed {
        body.push_str(&format!(" Took {elapsed}."));
    }
    if result.status != "complete" {
        body.push_str(&format!(
            "\n\n⚠️ Review ended in state `{}`: {}",
            result.status,
            result.message.as_deref().unwrap_or("partial results below")
        ));
    }
    if result.summary.comments == 0 {
        body.push_str("\n\nNo actionable issues found.");
    }
    for comment in overflow {
        body.push_str(&format!(
            "\n\n### 📄 `{}`\n\n{}",
            comment.path,
            comment_body(comment)
        ));
    }
    body
}

/// `~/.cache/review-bot`, or `.review-bot-cache` when HOME is unset.
fn cache_root() -> Result<PathBuf> {
    let root = match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(".cache").join("review-bot"),
        None => PathBuf::from(".review-bot-cache"),
    };
    fs::create_dir_all(&root).context("could not create cache directory")?;
    Ok(root)
}

fn repo_workspace(full_name: &str) -> Result<PathBuf> {
    let dir = cache_root()?
        .join("repos")
        .join(full_name.replace('/', "__"));
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(dir)
}

/// Dedicated HOME for ocr so its config/session state never mixes with the
/// operator's interactive setup.
fn ocr_home() -> Result<PathBuf> {
    let dir = cache_root()?.join("ocr-home");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

async fn sync_repository(token: &str, request: &ReviewRequest, workdir: &Path) -> Result<()> {
    // The token rides in argv for the authed clone/fetch entries and is never
    // persisted into the remote URL (the clone rewrites origin to the clean
    // URL immediately). Single-user host: acceptable; on shared hosts run the
    // bot as a dedicated user.
    let auth_url = with_token(&request.clone_url, token)?;
    if !workdir.join(".git").exists() {
        git(&["clone", "--quiet", &auth_url, &workdir.to_string_lossy()]).await?;
        git(&["-C", &workdir.to_string_lossy(), "remote", "set-url", "origin", &request.clone_url]).await?;
    }
    let base_spec = format!(
        "refs/heads/{}:refs/remotes/origin/{}",
        request.base_ref, request.base_ref
    );
    let head_spec = format!("refs/pull/{}/head:refs/remotes/origin/pr-head", request.number);
    git(&[
        "-C",
        &workdir.to_string_lossy(),
        "fetch",
        "--quiet",
        "--force",
        &auth_url,
        &base_spec,
        &head_spec,
    ])
    .await?;
    // Leave the tree at the PR head: ocr's agent reads full files for context.
    git(&[
        "-C",
        &workdir.to_string_lossy(),
        "checkout",
        "--quiet",
        "--detach",
        "refs/remotes/origin/pr-head",
    ])
    .await?;
    // The PR may have received new commits between webhook delivery and this
    // fetch. Post the review against what ocr actually read (resolved_head
    // pins the review), but make the discrepancy visible in the logs.
    let resolved_head = git(&[
        "-C",
        &workdir.to_string_lossy(),
        "rev-parse",
        "refs/remotes/origin/pr-head",
    ])
    .await?;
    if resolved_head.trim() != request.head_sha {
        info!(
            requested = %request.head_sha,
            found = %resolved_head.trim(),
            "PR head moved since the webhook event; reviewing current head"
        );
    }
    Ok(())
}

fn with_token(clone_url: &str, token: &str) -> Result<String> {
    let rest = clone_url
        .strip_prefix("https://")
        .ok_or_else(|| anyhow!("unsupported clone URL (expected https): {clone_url}"))?;
    Ok(format!("https://x-access-token:{token}@{rest}"))
}

/// Name of the git subcommand in an argv slice, for error messages.
fn git_label<'a>(args: &[&'a str]) -> &'a str {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match *arg {
            "-C" => {
                it.next();
            }
            arg if !arg.starts_with('-') => return arg,
            _ => {}
        }
    }
    "git"
}

async fn git(args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .output()
        .await
        .context("failed to run git (is it installed?)")?;
    if !output.status.success() {
        return Err(anyhow!(
            "git {} failed: {}",
            git_label(args),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn run_ocr(
    state: &AppState,
    request: &ReviewRequest,
    workdir: &Path,
    result_path: &Path,
) -> Result<OcrResult> {
    let home = ocr_home()?;
    if let Some(parent) = result_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Headless LLM wiring, mirroring the official GitHub Action's contract.
    // The API key is never written to the config file; the auth_token_cmd
    // helper reads it from the per-run environment instead.
    ocr_config(&home, &["llm.url", &state.llm_base_url]).await?;
    ocr_config(&home, &["llm.model", &state.llm_model]).await?;
    ocr_config(&home, &["llm.protocol", "openai"]).await?;
    ocr_config(&home, &["llm.use_anthropic", "false"]).await?;
    ocr_config(&home, &["llm.auth_token_cmd", "printf \"%s\" \"$OCR_LLM_TOKEN\""]).await?;

    let from_ref = format!("refs/remotes/origin/{}", request.base_ref);
    let output = Command::new("ocr")
        .current_dir(workdir)
        .env("HOME", &home)
        .env("OCR_LLM_TOKEN", &*state.llm_api_key)
        .args([
            "review",
            "--from",
            &from_ref,
            "--to",
            "refs/remotes/origin/pr-head",
            "--repo",
            &workdir.to_string_lossy(),
            "--format",
            "json",
            "--audience",
            "agent",
            "--output",
            &result_path.to_string_lossy(),
        ])
        .output()
        .await
        .context("failed to run ocr (npm install -g @alibaba-group/open-code-review)")?;

    if !result_path.exists() {
        return Err(anyhow!(
            "ocr produced no result (exit {}): {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .last()
                .unwrap_or("")
                .trim()
        ));
    }
    // A non-zero exit with a published result file is the documented partial
    // outcome (per-item failures); the status field carries the detail.
    if !output.status.success() {
        warn!(
            exit = output.status.code(),
            "ocr exited non-zero; publishing its partial result"
        );
    }
    let text = fs::read_to_string(result_path).context("could not read ocr result")?;
    serde_json::from_str(&text).context("could not parse ocr JSON result")
}

async fn ocr_config(home: &Path, key_value: &[&str]) -> Result<()> {
    let mut args = vec!["config", "set"];
    args.extend_from_slice(key_value);
    let output = Command::new("ocr")
        .env("HOME", home)
        .args(&args)
        .output()
        .await
        .context("failed to run ocr config")?;
    if !output.status.success() {
        return Err(anyhow!(
            "ocr config set {} failed: {}",
            key_value.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(start: Option<u64>, end: Option<u64>) -> OcrComment {
        OcrComment {
            path: "src/main.rs".to_owned(),
            content: "Off-by-one here.".to_owned(),
            existing_code: None,
            suggestion_code: Some("let x = 1;".to_owned()),
            start_line: start,
            end_line: end,
            severity: Some("high".to_owned()),
            category: Some("bug".to_owned()),
        }
    }

    #[test]
    fn multi_line_finding_maps_to_range_comment() {
        let spec = to_review_comment(&comment(Some(10), Some(14))).unwrap();
        assert_eq!(spec.path, "src/main.rs");
        assert_eq!(spec.start_line, Some(10));
        assert_eq!(spec.line, Some(14));
        assert!(spec.body.starts_with("**high** · _bug_"), "body: {:?}", spec.body);
    }

    #[test]
    fn single_line_finding_maps_to_line_comment() {
        let spec = to_review_comment(&comment(Some(7), Some(7))).unwrap();
        assert_eq!(spec.start_line, None);
        assert_eq!(spec.line, Some(7));
    }

    #[test]
    fn start_only_finding_is_single_line() {
        // GitHub rejects start_line == line; a lone bound must collapse to line.
        let spec = to_review_comment(&comment(Some(7), None)).unwrap();
        assert_eq!(spec.start_line, None);
        assert_eq!(spec.line, Some(7));
        let spec = to_review_comment(&comment(None, Some(9))).unwrap();
        assert_eq!(spec.start_line, None);
        assert_eq!(spec.line, Some(9));
    }

    #[test]
    fn lineless_finding_goes_to_summary() {
        assert!(to_review_comment(&comment(None, None)).is_none());
    }

    #[test]
    fn suggestion_block_renders_before_and_after() {
        let mut c = comment(Some(3), Some(3));
        c.existing_code = Some("let x = 0;".to_owned());
        let body = comment_body(&c);
        assert!(body.contains("**Before:**\n```\nlet x = 0;\n```\n**After:**\n```\nlet x = 1;\n```\n"), "{body}");
    }

    #[test]
    fn parses_real_ocr_result_and_extracts_head() {
        let result: OcrResult = serde_json::from_str(REAL_RESULT).unwrap();
        assert_eq!(result.status, "complete");
        assert_eq!(result.comments.len(), 1);
        assert_eq!(result.comments[0].severity.as_deref(), Some("medium"));
        assert_eq!(
            result.manifest.input.resolved_head.as_deref().unwrap(),
            "13ff1a2c2cd78ed21f218ee962d966ac34188ebd"
        );
        let orphan = comment(None, None);
        let body = review_summary(&result, &[&orphan]);
        assert!(body.contains("Reviewed 5 file(s) with ocr: 5 finding(s). Took 36s."), "{body}");
        assert!(body.contains("### 📄 `src/main.rs`"), "{body}");
    }

    #[test]
    fn zero_finding_summary_says_all_clear() {
        let result: OcrResult = serde_json::from_str(EMPTY_RESULT).unwrap();
        let body = review_summary(&result, &[]);
        assert!(body.contains("No actionable issues found."), "{body}");
    }

    #[test]
    fn token_injection_builds_fetch_url() {
        let url =
            with_token("https://github.com/alex-heritier/review-bot.git", "ghs_tok").unwrap();
        assert_eq!(
            url,
            "https://x-access-token:ghs_tok@github.com/alex-heritier/review-bot.git"
        );
        assert!(with_token("git@github.com:o/r.git", "t").is_err());
    }

    const REAL_RESULT: &str = r#"{
      "status": "complete",
      "llm": {"model": "deepseek-chat"},
      "message": "",
      "summary": {"files_reviewed": 5, "comments": 5, "total_tokens": 316644, "elapsed": "36s"},
      "tool_calls": [],
      "comments": [
        {"path": "src/review.rs", "content": "fence breakout", "existing_code": "x", "start_line": 84, "end_line": 86, "category": "security", "severity": "medium"}
      ],
      "warnings": [],
      "session_id": "s",
      "manifest": {"schema_version": "ocr.run-manifest/v1", "input": {"mode": "range", "resolved_head": "13ff1a2c2cd78ed21f218ee962d966ac34188ebd"}}
    }"#;

    const EMPTY_RESULT: &str = r#"{
      "status": "complete",
      "summary": {"files_reviewed": 2, "comments": 0},
      "comments": [],
      "manifest": {"input": {"resolved_head": "abc"}}
    }"#;
}
