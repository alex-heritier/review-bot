# GitHub PR Review Bot

Small self-hosted GitHub PR review bot. It receives a GitHub App `pull_request` webhook, runs [OpenCodeReview](https://github.com/alibaba/open-code-review) (`ocr`) as its review engine against a local checkout of the pull request, and publishes the findings as GitHub review comments — inline where line information exists, in the review summary otherwise.

The bot is deliberately thin: the GitHub App plumbing (webhook, installation auth, publishing) is here; the actual reviewing (file selection, rulesets, LLM agent, comment placement) is `ocr`'s job.

## Requirements

- Rust stable
- [OpenCodeReview](https://github.com/alibaba/open-code-review) on PATH: `npm install -g @alibaba-group/open-code-review`
- Git >= 2.41
- A public HTTPS URL for GitHub to reach this service
- A GitHub App with `Pull requests: Read and write`, `Contents: Read-only` and `Metadata: Read` repository permissions
- The downloaded GitHub App private key and App ID
- Your GitHub username
- An OpenAI-compatible chat completions API key (the model `ocr` should use)

## Run

Or provision the whole machine in one shot (prerequisite checks, `ocr`
install, release build, dedicated service user, hardened systemd unit):

```sh
sudo ./install.sh
```

The script is idempotent and prints the remaining manual steps (GitHub App
setup and the first-run wizard). The rest of this section describes the
manual path.

Configuration resolves as: CLI flag > environment variable > `.review-bot.yaml` > default. Run with `--help` to see every flag with its env var and default.

```sh
cargo run --release
```

When run interactively, missing required values start a setup wizard (secret input is hidden); the completed configuration is saved to `.review-bot.yaml` with `0600` permissions. Flags and env vars are never written back to the file. Non-interactive runs fail fast, naming every missing flag.

For unattended startup, pass the required values as flags or env vars:

```sh
cargo run --release -- \
  --github-username your-github-username \
  --github-app-id 123456 \
  --github-private-key-path /path/to/your-app.private-key.pem \
  --webhook-secret replace-with-a-random-secret \
  --openai-api-key sk-...
```

Optional values are `--openai-model`, `--openai-base-url`, and `--port`; defaults are listed in `--help`. The API key, base URL, and model are forwarded to `ocr` at review time (the key only ever passes through the per-run environment).

The server listens on `0.0.0.0:3000` by default. Put it behind a TLS reverse proxy such as Caddy or nginx; do not expose the service directly over plain HTTP to GitHub.

The wizard prints the GitHub App creation and settings URLs. Install the App once on your account and choose all repositories or selected repositories. No per-repository webhook is needed.

## How a review runs

1. The webhook handler verifies the HMAC signature and accepts only non-draft `pull_request` events (`opened`, `reopened`, `synchronize`) for PRs you authored.
2. The installation token fetches `origin/<base>` and `refs/pull/<n>/head` into a repo cache under `~/.cache/review-bot/repos/` and checks out the PR head (so `ocr`'s agent can read full files for context).
3. `ocr review --from origin/<base> --to refs/remotes/origin/pr-head --format json --audience agent` runs with a dedicated `HOME` under the cache directory; the LLM endpoint is configured headlessly exactly like the official GitHub Action does.
4. Findings are posted as one or more `COMMENT` reviews (batched at 50 inline comments per request), pinned to the head commit `ocr` reported. Findings without line information appear in the review summary instead.

Reviews run one at a time — simple, conflict-free, appropriate for a single-user bot.

## GitHub App setup

Create a GitHub App at `https://github.com/settings/apps/new` with:

- Webhook URL: `https://your-domain.example/webhooks/github`
- Webhook secret: the same value as `webhook_secret` in `.review-bot.yaml`
- Repository permissions: `Pull requests: Read and write`, `Contents: Read-only`, `Metadata: Read`
- Subscribe to the `Pull request` event
- Generate and download a private key

Install the App from its settings page and select **All repositories** or **Only select repositories**. GitHub includes the App installation ID in each webhook; the bot exchanges it for a short-lived installation token before reading or reviewing the PR.

Only PRs opened by `GITHUB_USERNAME` are reviewed.

Check availability with `GET /health`.

## Security

- Use a long random webhook secret, for example `openssl rand -hex 32`.
- Restrict the App installation to only the repositories that need reviews, if preferred.
- Keep the downloaded private key and `.review-bot.yaml` private and out of version control.
- The API key is passed to `ocr` per review via the environment (`OCR_LLM_TOKEN`); it is never written to ocr's config file.
- The installation token briefly appears in one `git fetch` argv entry; run the bot as a dedicated user so other users cannot inspect the process table.
- Do not put tokens in shell history for long-lived deployments; use the wizard or environment variables via your service manager's secret handling.
- Run behind HTTPS and a reverse proxy with request-size limits.
