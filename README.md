<div align="center">

<img src="https://raw.githubusercontent.com/ramc10/shunt/main/logo.svg" alt="shunt" width="120"><br>

# shunt

**A proxy for AI coding agents — pool accounts, beat rate limits.**

[![crates.io](https://img.shields.io/crates/v/shunt-proxy.svg)](https://crates.io/crates/shunt-proxy)
[![downloads](https://img.shields.io/github/downloads/ramc10/shunt/total)](https://github.com/ramc10/shunt/releases)
[![license](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![platform](https://img.shields.io/badge/macOS%20%7C%20Linux-lightgrey)

</div>

---

Shunt sits between your coding agents and your AI providers. It pools multiple accounts behind a single local endpoint, always routing to whoever has the most capacity left — so you never hit a rate limit mid-session.

<div align="center">
<img src="https://raw.githubusercontent.com/ramc10/shunt/main/diagram.svg" width="600">
</div>

**Works with:** Claude Code · Cursor · Codex CLI · Windsurf · any OpenAI or Anthropic SDK

**Providers:** Anthropic · OpenAI · Gemini · Groq · Mistral · DeepSeek · OpenRouter · Together · Fireworks · Ollama · local models

---

## Install

**macOS / Linux**

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

That's it. Your tools will route through shunt automatically.

To add more accounts:

```bash
shunt add-account personal   # another Claude account (OAuth)
shunt add-account groq       # Groq (prompts for API key)
shunt add-account codex      # ChatGPT Pro (device-code flow)
```

---

## What you get

**No more waiting on rate limits**

If an account hits a limit, shunt switches to the next one instantly. If every account is drained, it holds your request open and retries the moment the first one resets — your agent session never fails.

**Live dashboard**

```bash
shunt monitor
```

```
  ◆  work                                        Claude Pro
    you@work.com

    ✓  available
    5h  ████████████░░░░░░░░  61% left  ·  resets in 2h 14m
    7d  ███░░░░░░░░░░░░░░░░░  13% left  ·  resets in 1d 14h

  ◆  personal                                    Claude Pro
    alt@example.com

    ✓  available
    5h  ────────────────────  fresh
```

**Share with your team**

```bash
shunt share              # LAN sharing — prints a connect code
shunt share --tunnel     # any network via Cloudflare tunnel
shunt connect <code>     # on another machine — configures everything
```

**Savings tracker**

Every request is tracked against API pricing. Shunt shows you how much you've saved by using subscriptions instead.

---

## Commands

```bash
shunt setup              # first-time setup
shunt start              # start the proxy
shunt stop               # stop the proxy
shunt restart
shunt status             # account utilization and savings
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
