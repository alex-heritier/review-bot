# GitHub PR Review Bot

Small self-hosted GitHub PR review bot. It receives a GitHub App `pull_request` webhook, sends the PR diff to an OpenAI-compatible Chat Completions API, and posts the result as one GitHub review.

## Requirements

- Rust stable
- A public HTTPS URL for GitHub to reach this service
- A GitHub App with `Pull requests: Read and write` and `Metadata: Read` repository permissions
- The downloaded GitHub App private key and App ID
- Your GitHub username
- An OpenAI API key, or another API that implements `/v1/chat/completions`

## Run

On first run, with no configuration flags, the bot starts a small setup wizard and hides secret input. The completed configuration is saved to `.review-bot.yaml`; later runs only prompt for values that are missing:

```sh
cargo run --release
```

For unattended startup, pass the required values as flags. If any configuration flag is present, the wizard is not shown and missing required values must come from `.review-bot.yaml` or another flag:

```sh
cargo run --release -- \
  --github-username your-github-username \
  --github-app-id 123456 \
  --github-private-key-path /path/to/your-app.private-key.pem \
  --webhook-secret replace-with-a-random-secret \
  --openai-api-key sk-...
```

Optional flags are `--openai-model`, `--openai-base-url`, `--max-diff-chars`, and `--port`. Run with `--help` to see all flags.

The server listens on `0.0.0.0:3000` by default. Put it behind a TLS reverse proxy such as Caddy or nginx; do not expose the service directly over plain HTTP to GitHub.

The wizard prints the GitHub App creation and settings URLs. Install the App once on your account and choose all repositories or selected repositories. No per-repository webhook is needed.

## GitHub App setup

Create a GitHub App at `https://github.com/settings/apps/new` with:

- Webhook URL: `https://your-domain.example/webhooks/github`
- Webhook secret: the same value as `webhook_secret` in `.review-bot.yaml`
- Repository permissions: `Pull requests: Read and write`, `Metadata: Read`
- Subscribe to the `Pull request` event
- Generate and download a private key

Install the App from its settings page and select **All repositories** or **Only select repositories**. GitHub includes the App installation ID in each webhook; the bot exchanges it for a short-lived installation token before reading or reviewing the PR.

The bot reviews non-draft pull requests when they are opened, reopened, or updated with new commits. It ignores all other webhook events. It posts a single `COMMENT` review rather than inline comments, keeping the MVP independent of diff line parsing.

Only PRs opened by `GITHUB_USERNAME` are reviewed.

Check availability with `GET /health`.

## Security

- Use a long random webhook secret, for example `openssl rand -hex 32`.
- Restrict the App installation to only the repositories that need reviews, if preferred.
- Keep the downloaded private key and `.review-bot.yaml` private and out of version control.
- Do not put tokens in shell history for long-lived deployments; use the wizard or a service manager's secret handling.
- Run behind HTTPS and a reverse proxy with request-size limits.
