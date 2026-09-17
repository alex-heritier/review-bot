use std::{
    fs::{self, OpenOptions},
    io::{self, IsTerminal, Write},
    net::SocketAddr,
    sync::Arc,
};

use anyhow::{anyhow, Context, Result};
use axum::{
    body::Bytes,
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use clap::Parser;
use hmac::{Hmac, Mac};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};
use subtle::ConstantTimeEq;
use tracing::{error, info, warn};

type HmacSha256 = Hmac<Sha256>;

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL: &str = "gpt-4o-mini";
const DEFAULT_MAX_DIFF_CHARS: usize = 100_000;
const DEFAULT_PORT: u16 = 3000;
const GITHUB_APP_CREATE_URL: &str = "https://github.com/settings/apps/new?name=github-pr-review-bot&description=Self-hosted%20PR%20review%20bot&public=false&pull_requests=write&metadata=read&webhook_active=true&events[]=pull_request";
const GITHUB_APP_SETTINGS_URL: &str = "https://github.com/settings/apps";
const GITHUB_API_URL: &str = "https://api.github.com";
const GITHUB_API_VERSION: &str = "2022-11-28";
const CONFIG_FILE: &str = ".review-bot.yaml";

#[derive(Debug, Parser)]
#[command(
    name = "github-pr-review-bot",
    version,
    about = "Review your GitHub pull requests with an LLM",
    long_about = "Self-hosted GitHub PR review bot.\n\nValues are resolved as: CLI flag > environment variable > .review-bot.yaml > default.\nWhen run interactively, missing required values are prompted for and saved to .review-bot.yaml."
)]
struct Args {
    /// GitHub username; only PRs opened by this user are reviewed
    #[arg(long, value_name = "USERNAME", env = "GITHUB_USERNAME")]
    github_username: Option<String>,
    /// GitHub App ID (the number shown in the App settings page)
    #[arg(long, value_name = "ID", env = "GITHUB_APP_ID")]
    github_app_id: Option<u64>,
    /// Path to the downloaded GitHub App private key PEM file
    #[arg(long, value_name = "PATH", env = "GITHUB_PRIVATE_KEY_PATH")]
    github_private_key_path: Option<String>,
    /// GitHub App webhook secret (must match the secret configured on the App)
    #[arg(long, value_name = "SECRET", env = "WEBHOOK_SECRET")]
    webhook_secret: Option<String>,
    /// API key for the OpenAI-compatible chat completions API
    #[arg(long, value_name = "KEY", env = "OPENAI_API_KEY")]
    openai_api_key: Option<String>,
    /// LLM model [default: gpt-4o-mini]
    #[arg(long, value_name = "MODEL", env = "OPENAI_MODEL")]
    openai_model: Option<String>,
    /// Base URL of the chat completions API [default: https://api.openai.com/v1]
    #[arg(long, value_name = "URL", env = "OPENAI_BASE_URL")]
    openai_base_url: Option<String>,
    /// Maximum diff characters sent to the LLM [default: 100000]
    #[arg(long, value_name = "CHARS", env = "MAX_DIFF_CHARS")]
    max_diff_chars: Option<usize>,
    /// Port to listen on [default: 3000]
    #[arg(long, value_name = "PORT", env = "PORT")]
    port: Option<u16>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct SavedConfig {
    github_app_id: Option<u64>,
    github_private_key_path: Option<String>,
    github_username: Option<String>,
    webhook_secret: Option<String>,
    openai_api_key: Option<String>,
    openai_model: Option<String>,
    openai_base_url: Option<String>,
    max_diff_chars: Option<usize>,
    port: Option<u16>,
}

#[derive(Debug)]
struct Config {
    github_app_id: u64,
    github_private_key_path: String,
    github_username: String,
    webhook_secret: String,
    llm_api_key: String,
    llm_base_url: String,
    llm_model: String,
    max_diff_chars: usize,
    port: u16,
}

#[derive(Clone)]
struct AppState {
    github_app_id: u64,
    github_private_key: Arc<[u8]>,
    github_username: Arc<str>,
    webhook_secret: Arc<str>,
    llm_api_key: Arc<str>,
    llm_base_url: Arc<str>,
    llm_model: Arc<str>,
    max_diff_chars: usize,
    client: Client,
}

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

#[tokio::main]
async fn main() -> Result<()> {
    let log_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(log_filter)
        .with_ansi(io::stderr().is_terminal())
        .init();

    let config = Args::parse().into_config()?;
    let address = SocketAddr::from(([0, 0, 0, 0], config.port));
    info!(
        username = %config.github_username,
        github_app_id = config.github_app_id,
        model = %config.llm_model,
        base_url = %config.llm_base_url,
        max_diff_chars = config.max_diff_chars,
        "starting"
    );
    let state = AppState::from_config(config)?;

    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/webhooks/github", post(github_webhook))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(address).await?;
    info!(address = %listener.local_addr()?, "listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    info!("shutdown signal received");
}

impl AppState {
    fn from_config(config: Config) -> Result<Self> {
        let private_key = std::fs::read(&config.github_private_key_path).with_context(|| {
            format!(
                "could not read GitHub App private key at {}",
                config.github_private_key_path
            )
        })?;

        Ok(Self {
            github_app_id: config.github_app_id,
            github_private_key: private_key.into(),
            github_username: config.github_username.into(),
            webhook_secret: config.webhook_secret.into(),
            llm_api_key: config.llm_api_key.into(),
            llm_base_url: config.llm_base_url.trim_end_matches('/').to_owned().into(),
            llm_model: config.llm_model.into(),
            max_diff_chars: config.max_diff_chars,
            client: Client::builder()
                .user_agent("github-pr-review-bot")
                .build()?,
        })
    }
}

impl Args {
    fn into_config(self) -> Result<Config> {
        let mut saved = SavedConfig::load()?;
        self.apply_to(&mut saved);
        saved.normalize();

        // Interactive runs prompt for missing values; anything already provided
        // via flag, env, or the config file is kept as-is.
        if io::stdin().is_terminal() && saved.prompt_missing()? {
            saved.save()?;
            println!("\nConfiguration saved to {CONFIG_FILE}.");
        }

        saved.to_config()
    }

    fn apply_to(self, saved: &mut SavedConfig) {
        if self.github_app_id.is_some() {
            saved.github_app_id = self.github_app_id;
        }
        if self.github_private_key_path.is_some() {
            saved.github_private_key_path = self.github_private_key_path;
        }
        if self.github_username.is_some() {
            saved.github_username = self.github_username;
        }
        if self.webhook_secret.is_some() {
            saved.webhook_secret = self.webhook_secret;
        }
        if self.openai_api_key.is_some() {
            saved.openai_api_key = self.openai_api_key;
        }
        if self.openai_model.is_some() {
            saved.openai_model = self.openai_model;
        }
        if self.openai_base_url.is_some() {
            saved.openai_base_url = self.openai_base_url;
        }
        if self.max_diff_chars.is_some() {
            saved.max_diff_chars = self.max_diff_chars;
        }
        if self.port.is_some() {
            saved.port = self.port;
        }
    }
}

impl SavedConfig {
    fn load() -> Result<Self> {
        match fs::read_to_string(CONFIG_FILE) {
            Ok(contents) => serde_yaml::from_str(&contents)
                .with_context(|| format!("could not parse {CONFIG_FILE}")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("could not read {CONFIG_FILE}")),
        }
    }

    fn save(&self) -> Result<()> {
        let contents = serde_yaml::to_string(self)?;
        let mut options = OpenOptions::new();
        options.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(CONFIG_FILE)
            .with_context(|| format!("could not create {CONFIG_FILE}"))?;
        file.write_all(contents.as_bytes())?;
        Ok(())
    }

    /// Treat blank strings as unset so `--flag ""` and empty YAML values
    /// prompt (or error) the same as missing values.
    fn normalize(&mut self) {
        for value in [
            &mut self.github_private_key_path,
            &mut self.github_username,
            &mut self.webhook_secret,
            &mut self.openai_api_key,
            &mut self.openai_model,
            &mut self.openai_base_url,
        ] {
            if value.as_ref().is_some_and(|v| v.trim().is_empty()) {
                *value = None;
            }
        }
    }

    fn has_all_required(&self) -> bool {
        self.github_app_id.is_some()
            && self.github_private_key_path.is_some()
            && self.github_username.is_some()
            && self.webhook_secret.is_some()
            && self.openai_api_key.is_some()
    }

    /// Prompt for missing values. Returns true when anything was prompted for.
    /// Optional values keep their defaults silently once all required values
    /// are present, so routine restarts never prompt.
    fn prompt_missing(&mut self) -> Result<bool> {
        if self.has_all_required() {
            return Ok(false);
        }

        println!("GitHub PR review bot setup (press Enter to use defaults)");

        if self.github_username.is_none() {
            println!();
            self.github_username = Some(prompt_line("GitHub username", None)?);
        }
        if self.github_app_id.is_none() {
            println!();
            println!("Create a GitHub App here:");
            println!("  {GITHUB_APP_CREATE_URL}");
            println!("Configure: Pull requests - Read and write; Metadata - Read.");
            println!("Subscribe to Pull request events.");
            println!("Set the webhook URL to https://YOUR_SERVER/webhooks/github.");
            self.github_app_id = Some(loop {
                let value = prompt_line("GitHub App ID", None)?;
                match value.parse::<u64>() {
                    Ok(id) => break id,
                    Err(_) => println!("GitHub App ID must be a number; find it in the App settings page. Try again."),
                }
            });
        }
        if self.github_private_key_path.is_none() {
            println!();
            self.github_private_key_path = Some(loop {
                let path = prompt_line("Path to downloaded GitHub App private key", None)?;
                match fs::read(&path) {
                    Ok(bytes) => match EncodingKey::from_rsa_pem(&bytes) {
                        Ok(_) => break path,
                        Err(_) => println!("That file is not a valid RSA private key. Try again."),
                    },
                    Err(_) => println!("Could not read that file. Check the path and try again."),
                }
            });
        }
        if self.webhook_secret.is_none() {
            let username = self
                .github_username
                .as_deref()
                .ok_or_else(|| anyhow!("GitHub username is required before App setup"))?;
            println!();
            println!("Configure the App webhook here:");
            println!("  {GITHUB_APP_SETTINGS_URL}");
            println!("Install the App on your account with all or selected repositories.");
            println!("The webhook URL is https://YOUR_SERVER/webhooks/github.");
            println!("The App webhook secret must match the value entered below.");
            println!("GitHub account: {username}");
            self.webhook_secret = Some(secret_prompt("GitHub App webhook secret")?);
        }
        if self.openai_api_key.is_none() {
            println!();
            self.openai_api_key = Some(secret_prompt("OpenAI API key")?);
        }
        if self.openai_model.is_none() {
            println!();
            self.openai_model = Some(prompt_line("OpenAI model", Some(DEFAULT_MODEL))?);
        }
        if self.openai_base_url.is_none() {
            println!();
            self.openai_base_url = Some(prompt_line("OpenAI base URL", Some(DEFAULT_BASE_URL))?);
        }
        if self.max_diff_chars.is_none() {
            println!();
            let default = DEFAULT_MAX_DIFF_CHARS.to_string();
            self.max_diff_chars = Some(loop {
                let value = prompt_line("Maximum diff characters", Some(&default))?;
                match value.parse::<usize>() {
                    Ok(n) if n > 0 => break n,
                    _ => println!("Maximum diff characters must be a positive integer. Try again."),
                }
            });
        }
        if self.port.is_none() {
            println!();
            let default = DEFAULT_PORT.to_string();
            self.port = Some(loop {
                let value = prompt_line("Port", Some(&default))?;
                match value.parse::<u16>() {
                    Ok(port) if port > 0 => break port,
                    _ => println!("Port must be a number between 1 and 65535. Try again."),
                }
            });
        }
        Ok(true)
    }

    fn to_config(&self) -> Result<Config> {
        let mut missing = Vec::new();
        if self.github_username.is_none() {
            missing.push("--github-username");
        }
        if self.github_app_id.is_none() {
            missing.push("--github-app-id");
        }
        if self.github_private_key_path.is_none() {
            missing.push("--github-private-key-path");
        }
        if self.webhook_secret.is_none() {
            missing.push("--webhook-secret");
        }
        if self.openai_api_key.is_none() {
            missing.push("--openai-api-key");
        }
        if !missing.is_empty() {
            return Err(anyhow!(
                "missing required configuration: {}. Pass them as flags or environment variables (see --help), set them in {CONFIG_FILE}, or run interactively for the setup wizard",
                missing.join(", ")
            ));
        }
        Ok(Config {
            github_app_id: self.github_app_id.expect("checked above"),
            github_private_key_path: self.github_private_key_path.clone().expect("checked above"),
            github_username: self.github_username.clone().expect("checked above"),
            webhook_secret: self.webhook_secret.clone().expect("checked above"),
            llm_api_key: self.openai_api_key.clone().expect("checked above"),
            llm_base_url: self
                .openai_base_url
                .clone()
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned()),
            llm_model: self
                .openai_model
                .clone()
                .unwrap_or_else(|| DEFAULT_MODEL.to_owned()),
            max_diff_chars: self.max_diff_chars.unwrap_or(DEFAULT_MAX_DIFF_CHARS),
            port: self.port.unwrap_or(DEFAULT_PORT),
        })
    }
}

/// Prompt for one line, retrying on empty input for required values.
/// `default` is returned on empty input when present.
fn prompt_line(label: &str, default: Option<&str>) -> Result<String> {
    loop {
        match default {
            Some(value) => print!("{label} [{value}]: "),
            None => print!("{label}: "),
        }
        io::stdout().flush()?;
        let mut value = String::new();
        if io::stdin().read_line(&mut value)? == 0 {
            return Err(anyhow!("setup cancelled: input closed"));
        }
        let value = value.trim();
        if value.is_empty() {
            if let Some(value) = default {
                return Ok(value.to_owned());
            }
            println!("{label} cannot be empty. Try again.");
        } else {
            return Ok(value.to_owned());
        }
    }
}

fn secret_prompt(label: &str) -> Result<String> {
    loop {
        print!("{label}: ");
        io::stdout().flush()?;
        let value = rpassword::read_password()
            .context("setup cancelled: input closed")?
            .trim()
            .to_owned();
        if value.is_empty() {
            println!("{label} cannot be empty. Try again.");
        } else {
            return Ok(value);
        }
    }
}

async fn github_webhook(
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
        if let Err(error) = review_pull_request(
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

async fn review_pull_request(
    state: &AppState,
    repository: &str,
    number: u64,
    author: &str,
    installation_id: u64,
) -> Result<()> {
    info!(%repository, number, %author, "reviewing pull request");
    let token = installation_token(state, installation_id).await?;

    let diff_url = format!("https://api.github.com/repos/{repository}/pulls/{number}.diff");
    let diff = state
        .client
        .get(diff_url)
        .bearer_auth(&token)
        .header(header::ACCEPT, "application/vnd.github.v3.diff")
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;

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

    let review_url = format!("https://api.github.com/repos/{repository}/pulls/{number}/reviews");
    state
        .client
        .post(review_url)
        .bearer_auth(&token)
        .header(header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
        .json(&GithubReview {
            body: &review,
            event: "COMMENT",
        })
        .send()
        .await?
        .error_for_status()
        .context("GitHub rejected the review")?;

    info!(%repository, number, "review posted");
    Ok(())
}

async fn installation_token(state: &AppState, installation_id: u64) -> Result<String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn complete_config() -> SavedConfig {
        SavedConfig {
            github_app_id: Some(123456),
            github_private_key_path: Some("key.pem".to_owned()),
            github_username: Some("octocat".to_owned()),
            webhook_secret: Some("secret".to_owned()),
            openai_api_key: Some("sk-test".to_owned()),
            openai_model: None,
            openai_base_url: None,
            max_diff_chars: None,
            port: None,
        }
    }

    #[test]
    fn missing_config_lists_all_flag_names() {
        let error = SavedConfig::default().to_config().unwrap_err().to_string();
        for flag in [
            "--github-username",
            "--github-app-id",
            "--github-private-key-path",
            "--webhook-secret",
            "--openai-api-key",
        ] {
            assert!(error.contains(flag), "error names {flag}: {error}");
        }
    }

    #[test]
    fn blank_values_count_as_missing() {
        let mut saved = complete_config();
        saved.github_username = Some("  ".to_owned());
        saved.normalize();
        assert!(!saved.has_all_required());
        let error = saved.to_config().unwrap_err().to_string();
        assert!(error.contains("--github-username"), "{error}");
        assert!(!error.contains("--github-app-id"), "{error}");
    }

    #[test]
    fn optionals_fall_back_to_defaults() {
        let config = complete_config().to_config().unwrap();
        assert_eq!(config.llm_model, DEFAULT_MODEL);
        assert_eq!(config.llm_base_url, DEFAULT_BASE_URL);
        assert_eq!(config.max_diff_chars, DEFAULT_MAX_DIFF_CHARS);
        assert_eq!(config.port, DEFAULT_PORT);
    }

    #[test]
    fn complete_config_never_prompts() {
        let mut saved = complete_config();
        assert!(!saved.prompt_missing().unwrap());
    }

    #[test]
    fn cli_flags_override_saved_values() {
        let mut saved = complete_config();
        Args::try_parse_from(["bot", "--github-username", "new-name", "--port", "4000"])
            .unwrap()
            .apply_to(&mut saved);
        assert_eq!(saved.github_username.as_deref(), Some("new-name"));
        assert_eq!(saved.port, Some(4000));
        assert_eq!(saved.github_app_id, Some(123456));
    }

    #[test]
    fn help_shows_real_defaults() {
        let help = Args::command().render_help().to_string();
        for default in [
            DEFAULT_MODEL,
            DEFAULT_BASE_URL,
            &DEFAULT_MAX_DIFF_CHARS.to_string(),
            &DEFAULT_PORT.to_string(),
        ] {
            assert!(help.contains(default), "help shows {default}");
        }
    }

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
