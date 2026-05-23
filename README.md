<div align="center">

```
              ██████
            ███    ███
              ██████
               █  █
```

# shunt

**Pool every AI provider. Route every coding agent. Never hit a rate limit again.**

[![crates.io](https://img.shields.io/crates/v/shunt-proxy.svg)](https://crates.io/crates/shunt-proxy)
[![downloads](https://img.shields.io/github/downloads/ramc10/shunt/total)](https://github.com/ramc10/shunt/releases)
[![license](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![platform](https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey)

</div>

---

## What is shunt?

Shunt is a local proxy that sits between your AI coding agents and every major AI provider. It pools multiple accounts, rotates across them intelligently, and presents a single unified endpoint — so your tools never see a rate limit, a 429, or a cold start.

```
  Claude Code ─┐                    ┌─ Anthropic  (port 8082)
  Cursor      ─┤                    ├─ OpenAI API (port 8083)
  Codex CLI   ─┤──▶  [ shunt ]  ──▶ ├─ Gemini     (port 8092)
  Windsurf    ─┤                    ├─ Groq        (port 8086)
  any SDK     ─┘                    ├─ Mistral     (port 8087)
                                    ├─ DeepSeek    (port 8090)
                                    ├─ OpenRouter  (port 8089)
                                    ├─ Together    (port 8088)
                                    ├─ Fireworks   (port 8091)
                                    ├─ Ollama      (port 8085/8093)
                                    └─ local LLMs  (port 8093)
```

One binary. Twelve providers. Every coding agent. Zero rate limits.

---

## Supported providers

| Provider | Auth | Port |
|---|---|---|
| **Anthropic** (claude.ai OAuth) | OAuth | 8082 |
| **OpenAI / ChatGPT Pro** (chatgpt.com) | OAuth | 8083 |
| **OpenAI API** (api.openai.com) | API key | 8084 |
| **Gemini** (Google) | API key | 8092 |
| **Groq** | API key | 8086 |
| **Mistral** | API key | 8087 |
| **DeepSeek** | API key | 8090 |
| **OpenRouter** | API key | 8089 |
| **Together AI** | API key | 8088 |
| **Fireworks AI** | API key | 8091 |
| **Ollama Cloud** | API key | 8085 |
| **Local** (Ollama, LM Studio, llama.cpp) | None | 8093 |

Every provider exposes a local endpoint that speaks either the Anthropic or OpenAI wire format — whichever your tool expects.

---

## Why shunt?

AI coding agents are only as fast as their rate limits allow. Hit your Claude 5-hour window, and Claude Code stops dead. Hit your OpenAI tier cap, and Cursor stalls. Shunt eliminates this entirely:

- **Pool multiple accounts** — two Claude Pro accounts = roughly 2× throughput; three = 3×
- **Intelligent routing** — tracks live utilization headers from every response and always picks the account with the most headroom, across both 5h and 7d windows
- **Auto-failover** — 429 or 529? Shunt reads the exact reset timestamp, parks that account, and immediately retries with the next best one
- **Zero-downtime exhaustion handling** — if every account is drained, requests are held open and retried the instant the first account resets. Your agent session never errors out
- **Cross-provider routing** — overflow Claude requests to Groq or DeepSeek when your Anthropic accounts are hot. Mix providers transparently
- **Drop-in** — one env var and your existing tools route through shunt. No code changes, no SDK swaps

---

## Install

```bash
curl -sSf https://raw.githubusercontent.com/ramc10/shunt/main/install.sh | sh
```

Downloads the right pre-built binary for your OS and arch — no Rust, no dependencies.

Or via Cargo:

```bash
cargo install shunt-proxy
```

---

## 3-minute setup

**1. Import your existing session**

```bash
shunt setup
```

Auto-imports the Claude credentials already on disk. Takes seconds.

**2. Add more accounts or providers**

```bash
shunt add-account personal      # Another Claude Pro account (OAuth)
shunt add-account groq          # Groq (API key prompt)
shunt add-account deepseek      # DeepSeek (API key prompt)
shunt add-account codex         # ChatGPT Pro via device-code flow
```

**3. Start the proxy**

```bash
shunt start
```

**4. Point your tools at it**

For Claude Code / Anthropic SDK:
```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:8082
```

For Cursor, Codex CLI, or any OpenAI-compatible tool:
```bash
export OPENAI_BASE_URL=http://127.0.0.1:8083
export OPENAI_API_KEY=dummy   # shunt ignores it; your real creds are stored internally
```

For Gemini:
```bash
export GEMINI_BASE_URL=http://127.0.0.1:8092
```

Add these to your `.zshrc` / `.bashrc` — `shunt setup` offers to do this automatically.

---

## Live dashboard

```bash
shunt monitor
```

Full-screen TUI: live utilization bars per account, per provider, cooldown countdowns, request history, and cost savings — updates in real time as requests flow through.

```bash
shunt status    # snapshot view
```

```
── ACCOUNTS ──────────────────────────────────────────────────

  ✓  work        Anthropic  you@example.com      available   1.2M tok
          5h window  ████████░░░░░░░░░░  61% remaining  resets in 2h 14m
          7d window  ███░░░░░░░░░░░░░░░  79% remaining  resets in 4d 6h

  ✓  personal    Anthropic  alt@example.com      available   fresh
          5h window  ░░░░░░░░░░░░░░░░░░  100% remaining

  ✓  groq-main   Groq       —                    available   fresh

── SAVINGS ───────────────────────────────────────────────────

  Today: 2.3M tok  ·  $6.12  ·  All-time: $48.30
```

---

## Features

### Smart routing

Shunt captures real-time utilization headers after every API response and routes to the account with the **lowest combined utilization** across the most-urgent window. Fresh accounts always get highest priority.

### Auto-failover & auto-resume

- 429 / 529 → reads the exact reset timestamp, parks the account, retries with the next best immediately
- All accounts exhausted → holds the HTTP connection open, sleeps until the soonest reset, retries transparently. No errors surface to your agent
- Pre-fetch on resume → after a cooldown, shunt warms the account's quota state so the next request routes accurately without discovering limits cold

### Savings tracker

Tracks every token proxied and shows how much you'd have paid at API prices. Watching $48 accumulate is extremely motivating.

### Share your pool

```bash
shunt share          # Bind to LAN, print a connect code
shunt share --tunnel # Expose via Cloudflare tunnel (any network)
shunt connect <code> # On another device — auto-configures Claude Code
```

Anyone on your team can route through your shared pool.

### Remote notifications

Get native system notifications on your laptop when a remote shunt instance hits a rate limit, resumes, or needs reauth:

```bash
shunt remote          # Host — prints a watch code
shunt remote <code>   # Client — subscribes to remote notifications
```

### Pin routing

```bash
shunt use work    # Force all requests through 'work'
shunt use auto    # Restore automatic routing
shunt use         # Interactive picker
```

### Protocol translation

Shunt handles format conversion transparently:

- **OpenAI → Anthropic** — route Codex/Cursor requests through your Claude accounts
- **Anthropic → OpenAI** — expose Claude to OpenAI-expecting tools
- **Any provider → any provider** — mix and match as needed

**OpenAI model mapping (when routing through Claude):**

| OpenAI model | Claude model |
|---|---|
| `gpt-4o`, `o1`, `o3`, `gpt-5` | `claude-opus-4-6` |
| `gpt-4o-mini`, `o1-mini`, `o3-mini` | `claude-haiku-4-5-20251001` |
| anything else | `claude-sonnet-4-6` |

---

## Commands

```bash
shunt start              # Start the proxy in the background
shunt start --foreground # Keep in terminal
shunt start --verbose    # Debug logging: routing, token refresh
shunt stop               # Stop the proxy
shunt restart
shunt status             # Accounts, utilization, savings
shunt monitor            # Live fullscreen TUI
shunt logs               # Last 50 log lines
shunt logs -f            # Follow logs
shunt add-account <name> # Add any account/provider
shunt remove-account <name>
shunt logout [name]      # Log out (or --all)
shunt use [account]      # Pin routing to account
shunt use auto           # Restore automatic routing
shunt share              # Share on LAN
shunt share --tunnel     # Share via Cloudflare tunnel
shunt connect <code>     # Connect device to shared proxy
shunt remote             # Generate watch code (host)
shunt remote <code>      # Watch remote instance (client)
shunt update             # Update to latest release
shunt setup              # First-time setup wizard
```

---

## Configuration

`~/Library/Application Support/shunt/config.toml` (macOS) or `~/.config/shunt/config.toml` (Linux):

```toml
[server]
host = "127.0.0.1"
port = 8082
log_level = "info"

[[accounts]]
name = "work"
provider = "anthropic"
plan_type = "pro"

[[accounts]]
name = "personal"
provider = "anthropic"
plan_type = "pro"

[[accounts]]
name = "groq-main"
provider = "groq"
```

Credentials are stored separately in `credentials.json` and never written to the config file.

---

## Requirements

- macOS or Linux
- At least one account on any supported provider
- Claude Code installed if using Anthropic OAuth (shunt borrows its credentials for the first account)

---

## Notes

- Multiple accounts must be **different logins** — two sessions from the same account won't multiply limits
- Shunt intercepts only the relevant API paths per provider — all other traffic passes through unchanged
- `shunt start` automatically replaces any already-running instance

---

## License

MIT
