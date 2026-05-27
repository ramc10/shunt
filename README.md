<div align="center">

<img src="https://shunt-web.vercel.app/assets/shunt_logo-removebg-preview.png" alt="shunt" width="96" style="image-rendering:pixelated"><br>

# shunt

**Pool your Claude rate limits. Never get throttled mid-session again.**

[![crates.io](https://img.shields.io/crates/v/shunt-proxy.svg)](https://crates.io/crates/shunt-proxy)
[![downloads](https://img.shields.io/github/downloads/ramc10/shunt/total)](https://github.com/ramc10/shunt/releases)
[![license](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![platform](https://img.shields.io/badge/macOS%20%7C%20Linux-lightgrey)

<a href="https://shunt-web.vercel.app" target="_blank">shunt-web.vercel.app</a>

</div>

---

Shunt is a local proxy that combines your Claude Code accounts into one endpoint. It auto-routes every request to the account with the most headroom, fails over silently when one hits a limit, and holds your connection open until capacity frees up — your agent session never sees a 429.

<div align="center">
<img src="https://shunt-web.vercel.app/assets/shunt_logo-removebg-preview.png" alt="shunt" width="120" style="image-rendering:pixelated">
</div>

**Works with:** Claude Code · Cursor · Codex CLI · Windsurf · any OpenAI or Anthropic SDK

**Providers:** Anthropic · OpenAI · Gemini · Groq · Mistral · DeepSeek · OpenRouter · Together · Fireworks · Ollama · local models

---

## Install

**macOS / Linux — one command:**

```bash
curl -sSf https://raw.githubusercontent.com/ramc10/shunt/main/install.sh | sh
```

**via Cargo**

```bash
cargo install shunt-proxy
```

---

## Quick start

```bash
shunt setup      # import your Claude Code session + configure your shell
shunt start      # start the proxy
```

That's it. Claude Code and your other tools route through shunt automatically.

Add more accounts to grow your pool:

```bash
shunt add-account personal   # another Claude account (OAuth)
shunt add-account work       # another Claude account
shunt add-account codex      # ChatGPT Pro (device-code flow)
shunt add-account groq       # Groq (prompts for API key)
```

---

## What shunt does

**Combines your rate limits**

N accounts = N × the limit you already pay for. Three Claude Pro accounts means three 5-hour windows and three 7-day windows, pooled and automatically load-balanced.

**Fails over silently**

When an account hits its limit, the next request goes to whichever account has capacity. No 429 errors, no broken loops. If every account is drained, shunt holds the connection open and retries the moment the first one resets.

**Live status**

```bash
shunt status
```

```
  ◆  main                                        Claude Pro
    you@example.com

    ✓  available
    5h  ████████████████░░░░  81% left  ·  resets in 2h 28m
    7d  █████████████████░░░  85% left  ·  resets in 4d 16h

  ────────────────────────────────────────────────────────

  ◆  work                                        Claude Pro
    alt@example.com

    ✓  available
    5h  ██████████████████░░  92% left  ·  resets in 2h 28m
    7d  █████████████░░░░░░░  65% left  ·  resets in 4d 5h
```

**Share with your team**

```bash
shunt share              # LAN sharing — prints a connect code
shunt share --tunnel     # any network via Cloudflare tunnel
shunt connect <code>     # on another machine — configures everything
```

---

## Commands

```bash
shunt setup              # first-time setup
shunt start              # start the proxy
shunt stop               # stop the proxy
shunt restart
shunt status             # account utilization
shunt monitor            # live fullscreen dashboard
shunt logs               # recent logs
shunt logs -f            # follow logs
shunt add-account <name> # add an account or provider
shunt remove-account <name>
shunt logout [name]      # log out of an account
shunt use [account]      # pin routing to a specific account
shunt use auto           # restore automatic routing
shunt share              # share on LAN
shunt share --tunnel     # share via Cloudflare tunnel
shunt connect <code>     # connect to a shared proxy
shunt remote             # watch a remote instance (host)
shunt remote <code>      # watch a remote instance (client)
shunt update             # update to latest
```

---

MIT License
