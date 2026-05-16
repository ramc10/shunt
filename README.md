# shunt

A local proxy that pools multiple Claude accounts behind a single endpoint, routing requests across accounts to maximize your available rate limits.

```
  ─┐    shunt  v0.1.0
  ─┼─▶  2 accounts  ·  http://127.0.0.1:8082
  ─┘    Proxying Claude API across multiple accounts
```

## What it does

Claude's rate limits are per-account. If you hit your 5-hour or weekly limit on one account, you're stuck waiting. Shunt sits in front of the Anthropic API and automatically routes each request to whichever account has the most remaining capacity — so you get the combined limits of all your accounts.

- **Least-utilization routing** — tracks live `anthropic-ratelimit-unified-*` headers and always picks the account with the most headroom
- **Auto-failover** — if one account is rate-limited, the next request goes to another
- **Transparent** — drop-in replacement for `api.anthropic.com`; works with Claude Code, the SDK, or any tool that speaks the Anthropic API
- **Account management** — add or remove accounts without restarting manually

## Install

```bash
curl -sSf https://raw.githubusercontent.com/ramc10/shunt/main/install.sh | sh
```

This downloads the right pre-built binary for your OS and arch — no Rust or other dependencies needed.

Or via cargo:

```bash
cargo install shunt-proxy
```

## Setup

Shunt uses OAuth — the same session Claude Code uses — so there's no API key to manage.

**Step 1: Import your existing Claude Code session**

```bash
shunt setup
```

This auto-imports the credentials Claude Code already has on disk. Takes a second.

**Step 2: Add a second account**

Log out of Claude Code, log in with your second Claude account, then:

```bash
shunt add-account secondary
```

Or give it any name you want (`work`, `personal`, etc.):

```bash
shunt add-account work
```

This opens a browser for OAuth authorization.

**Step 3: Start the proxy**

```bash
shunt start
```

The proxy starts in the background and your terminal is immediately returned. To point Claude Code at it:

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:8082
```

Add that to your `.zshrc` / `.bashrc` (shunt setup offers to do this automatically).

## Usage

```bash
shunt start              # Start (or restart) the proxy in the background
shunt start --foreground # Keep it in the terminal (for debugging)
shunt status             # Show accounts, rate limit bars, reset times
shunt add-account <name> # Add another Claude account
shunt remove-account <name> # Remove an account
shunt setup              # First-time setup
```

### Status output

```
── ACCOUNTS ────────────────────────────────────────────────

  ✓  work        Claude Pro  you@example.com          available     1.2M tok
          5h window  ████████░░░░░░░░░░  61% remaining  ok  resets in 2h 14m
          7d window  ███░░░░░░░░░░░░░░░  79% remaining  ok  resets in 4d 6h
          Extra usage  available

  ✓  personal    Claude Pro  alt@example.com          available     fresh
          5h window  ░░░░░░░░░░░░░░░░░░  100% remaining  ok
```

## How routing works

Every response from the Anthropic API includes `anthropic-ratelimit-unified-5h-utilization` headers (a float from 0–1). Shunt captures these and always routes the next request to the account with the **lowest 5-hour utilization**. Fresh accounts (no data yet) are treated as 0% utilized and get highest priority.

If a request returns 429 or 529, shunt marks that account as cooling and retries with the next-best account automatically.

## Configuration

Config lives at `~/Library/Application Support/shunt/config.toml` (macOS) or `~/.config/shunt/config.toml` (Linux):

```toml
[server]
host = "127.0.0.1"
port = 8082
log_level = "info"

[[accounts]]
name = "work"
plan_type = "pro"

[[accounts]]
name = "personal"
plan_type = "pro"
```

Credentials are stored separately in `credentials.json` (never in the config file).

## Files

| File | Location |
|------|----------|
| Config | `~/Library/Application Support/shunt/config.toml` |
| Credentials | `~/Library/Application Support/shunt/credentials.json` |
| Logs | `~/Library/Application Support/shunt/proxy.log` |
| Status API | `http://127.0.0.1:8082/status` |

## Requirements

- Rust 1.75+
- One or more Claude Pro / Max accounts
- Claude Code installed (shunt borrows its OAuth credentials)

## Codex / OpenAI routing

Shunt supports two Codex use cases:

1. **Route Codex through your Claude pool** — translate OpenAI requests to Anthropic format on the fly. No OpenAI/ChatGPT subscription needed.
2. **Use a ChatGPT Pro account directly** — add your ChatGPT Pro account to the pool so Codex CLI authenticates through shunt. No separate login required.

When any OpenAI/Codex account is configured, shunt starts a second proxy on port **8083** that speaks the OpenAI API format.

### Add a Codex account (ChatGPT Pro)

```bash
shunt add-account codex
```

Select **OpenAI / Codex** as the provider when prompted. Shunt uses the Codex device-code flow — it prints a short code, opens your browser, and completes auth automatically.

After adding the account, shunt automatically writes `~/.codex/auth.json` with the correct credentials. You can run `codex` immediately without logging in again.

### Run Codex CLI

```bash
codex
```

Shunt keeps `~/.codex/auth.json` up to date whenever tokens are refreshed, so you never need to re-authenticate in the Codex CLI.

### Route other OpenAI-compatible tools through Claude

If you want to use Codex (or any OpenAI-compatible tool) against your **Claude** accounts instead of ChatGPT, point it at shunt's OpenAI-compat endpoint:

```bash
export OPENAI_BASE_URL=http://127.0.0.1:8083
export OPENAI_API_KEY=dummy   # any non-empty value; shunt ignores it
codex
```

Add these to your shell profile to make them permanent. Requests are translated from OpenAI format to Anthropic format and routed through your Claude pool.

### Model mapping (Claude routing)

When routing through Claude, OpenAI model names are mapped automatically:

| OpenAI model | Claude model |
|---|---|
| `gpt-4o`, `o1`, `o3`, `gpt-5` | `claude-opus-4-6` |
| `gpt-4o-mini`, `o1-mini`, `o3-mini` | `claude-haiku-4-5-20251001` |
| anything else | `claude-sonnet-4-6` |

Claude model names (e.g. `claude-sonnet-4-6`) pass through as-is.

### What's supported

- `POST /v1/chat/completions` — streaming and non-streaming, system messages, temperature, stop sequences
- `GET /v1/models` — returns available Claude models in OpenAI format
- Everything else is forwarded to the ChatGPT upstream as-is

## Notes

- Both accounts need to be **different Claude logins** — two sessions from the same account won't double your limits
- Shunt only proxies `/v1/messages` and `/v1/messages/count_tokens` for the Anthropic endpoint — everything else passes through untouched
- `shunt start` automatically kills and replaces any running instance
