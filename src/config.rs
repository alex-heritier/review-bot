//! Config: loads and saves the bot settings.
//!
//! Resolution order is CLI flag > environment variable > `.review-bot.yaml`
//! > default. Interactive runs fill anything still missing via a setup wizard.

use std::{
    fs::{self, OpenOptions},
    io::{self, IsTerminal, Write},
};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use jsonwebtoken::EncodingKey;
use serde::{Deserialize, Serialize};

pub(crate) const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
pub(crate) const DEFAULT_MODEL: &str = "gpt-4o-mini";
pub(crate) const DEFAULT_PORT: u16 = 3000;
const GITHUB_APP_CREATE_URL: &str = "https://github.com/settings/apps/new?name=github-pr-review-bot&description=Self-hosted%20PR%20review%20bot&public=false&pull_requests=write&metadata=read&webhook_active=true&events[]=pull_request";
const GITHUB_APP_SETTINGS_URL: &str = "https://github.com/settings/apps";
const CONFIG_FILE: &str = ".review-bot.yaml";

#[derive(Debug, Parser)]
#[command(
    name = "github-pr-review-bot",
    version,
    about = "Review your GitHub pull requests with an LLM",
    long_about = "Self-hosted GitHub PR review bot.\n\nValues are resolved as: CLI flag > environment variable > .review-bot.yaml > default.\nWhen run interactively, missing required values are prompted for and saved to .review-bot.yaml."
)]
pub(crate) struct Args {
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
    port: Option<u16>,
}

#[derive(Debug)]
pub(crate) struct Config {
    pub(crate) github_app_id: u64,
    pub(crate) github_private_key_path: String,
    pub(crate) github_username: String,
    pub(crate) webhook_secret: String,
    pub(crate) llm_api_key: String,
    pub(crate) llm_base_url: String,
    pub(crate) llm_model: String,
    pub(crate) port: u16,
}

impl Args {
    pub(crate) fn into_config(self) -> Result<Config> {
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
            &DEFAULT_PORT.to_string(),
        ] {
            assert!(help.contains(default), "help shows {default}");
        }
    }
}
