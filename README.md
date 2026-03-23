# tmux-claude-unofficial-rate-limit

Display Claude subscription rate limit remaining percentage in tmux, terminal, or JSON output. Confirmed on Max; Pro/Team remains unverified.

[한국어](README.ko.md)

```
5h:24%(1h18m) 1w:40%
```

- **5h:24%** — 5-hour window remaining 24%
- **(1h18m)** — time until reset
- **1w:40%** — weekly remaining 40%

## Requirements

- macOS (uses CommonCrypto, Keychain)
- Claude Code or Claude Desktop logged in
- Rust toolchain (for building)
- tmux (for status bar display)

## FAQ

**Q: Does this work on Pro too?**

**A:** It works on plans that return the rate limit headers. Confirmed on Max; unverified on Pro/Team.

## Install

```bash
git clone https://github.com/lastrites2018/tmux-claude-unofficial-rate-limit.git
cd tmux-claude-unofficial-rate-limit
cargo build --release
cp target/release/rate-limit ~/.local/bin/claude-rate-limit
```

If `~/.local/bin` is not in your `PATH`, either add it or run the binary with its full path: `~/.local/bin/claude-rate-limit`.

## Token Setup

If `~/.claude/.credentials.json` already exists, you can skip this step. Otherwise, extract the OAuth token from Claude Desktop's encrypted storage. When the macOS Keychain popup appears, click **Allow**.

```bash
claude-rate-limit extract-token
```

This writes the token atomically to `~/.claude/.credentials.json` with owner-only permissions (`0600`).

## Quick Start

Most users only need these commands:

```bash
# check current rate limit in the terminal
claude-rate-limit

# only if ~/.claude/.credentials.json does not exist yet
claude-rate-limit extract-token

# output format for tmux status bar
claude-rate-limit tmux
```

## Usage

```bash
# tmux status bar (no color)
claude-rate-limit tmux

# terminal with ANSI colors
claude-rate-limit

# structured output for scripts
claude-rate-limit --json

# force refresh (ignore cache)
claude-rate-limit --refresh

# override cache TTL (1..60 minutes, default: 15)
claude-rate-limit --ttl-minutes 5

# override HTTP timeout (1..30 seconds, default: 10)
claude-rate-limit --http-timeout-seconds 3

# hide reset hints (enabled by default)
claude-rate-limit --hide-reset-dates

# extract/refresh token
claude-rate-limit extract-token
```

`--json` cannot be combined with `tmux`. `--refresh`, `--hide-reset-dates`, `--ttl-minutes`, and `--http-timeout-seconds` apply only to display mode, not `extract-token`.

Running `claude-rate-limit --ttl-minutes 5` in a shell only affects that single invocation. If you want `tmux` to keep using a 5-minute TTL, add the flag to the command inside `~/.tmux.conf`.

Reset hints are enabled by default, but they only appear when remaining is 30% or lower. For 5h, same-day resets stay relative, but cross-day resets switch to `M/D H[:MM]`. For 1w, the reset date is shown as `M/D H[:MM]`. Use `--hide-reset-dates` to suppress these hints. In `tmux` output, `[Xm ago]` takes priority over the weekly reset date to keep the line short.

## tmux Configuration

Add to `~/.tmux.conf`:

```tmux
set -g status-right '#(~/.local/bin/claude-rate-limit tmux) | %Y-%m-%d %H:%M '
```

To keep `tmux` on a 5-minute cache TTL, put the flag inside the `tmux` command itself:

```tmux
set -g status-right '#(~/.local/bin/claude-rate-limit --ttl-minutes 5 tmux) | %Y-%m-%d %H:%M '
```

Reload:

```bash
tmux source-file ~/.tmux.conf
```

## How It Works

1. Read OAuth token from `~/.claude/.credentials.json`
2. Make a minimal API request (Haiku, 1 token) to Anthropic → parse rate limit from response headers
3. Cache result in `~/.claude/rate-limit-cache.json` (default 15-min TTL, configurable with `--ttl-minutes 1..60`)
4. If cache is valid, output immediately without API call

Concurrent calls are coordinated with `flock` — one process makes the API call, while others wait briefly for a fresh cache and use cached data when it becomes available.

## Files

| File | Purpose |
|---|---|
| `~/.claude/.credentials.json` | OAuth token (atomically written, `0600`) |
| `~/.claude/rate-limit-cache.json` | API response cache (atomically written, `0600`) |
| `~/.claude/rate-limit.lock` | Concurrent access lock (`0600`) |

## Troubleshooting

If display mode shows `[err]`, or `--json` returns an error object, check the cause first:

- If the error indicates token expiry or `401`, rerun `claude-rate-limit extract-token`.
- If `~/.claude/.credentials.json` already exists and still works, you do not need to rerun `extract-token`.
- Errors such as missing `HOME`, missing token file, network failure, or missing rate-limit headers require fixing the environment or retrying later.

## Security

**Why this is relatively safe:**

- **Application network traffic is narrow** — the application code sends its API request to `api.anthropic.com` and does not contain telemetry, analytics, or other application-level network destinations.
- **Token is stored locally** — `~/.claude/.credentials.json` is written atomically with owner-only permissions (`0600`). During rate-limit fetches, the token is sent only to `api.anthropic.com`.
- **Minimal API surface** — the API call sends a 1-token Haiku request solely to read response headers. The response body is discarded.
- **Small runtime surface** — the main logic runs in a single Rust binary. `extract-token` also invokes macOS's built-in `/usr/bin/security` and CommonCrypto.
- **Auditable implementation** — the core runtime logic is concentrated in a single `src/main.rs` file, without a custom build script or hidden background services.
- **No write access to Claude config** — the binary never modifies Claude Desktop or Claude Code configuration. `extract-token` only reads from Claude's config and writes to its own credential file.

## Stability of Token Extraction

`extract-token` depends on Claude Desktop's (Electron app) internal storage implementation.

**Unofficial implementation details relied upon:**

- Claude Desktop stores an encrypted token at `~/Library/Application Support/Claude/config.json` under the `oauth:tokenCache` key
- Electron's safeStorage uses `v10` prefix + AES-128-CBC (PBKDF2-SHA1, salt=`saltysalt`, iterations=1003) on macOS
- The encryption key is stored in macOS Keychain as `Claude Safe Storage` / `Claude Key`

**When this may break:**

- Claude Desktop update changes the token storage location or key name
- Electron changes the safeStorage encryption scheme (v10 → v11, etc.)
- Claude Code officially supports `.credentials.json`, making `extract-token` unnecessary

**Why breakage is safe:**

- Display commands only read `~/.claude/.credentials.json`, independent of `extract-token`
- If `extract-token` fails, existing token continues to work until it expires
- No need to re-run `extract-token` if the token file already exists and the token is still valid

## Platform

macOS only (Keychain, CommonCrypto)

## Disclaimer

This is an **unofficial, community-built tool** and is not affiliated with, endorsed by, or supported by Anthropic.

- This tool accesses undocumented internal storage of Claude Desktop (Electron safeStorage). This behavior may break at any time without notice.
- The OAuth token extracted by `extract-token` grants API access to your Anthropic account. Treat `~/.claude/.credentials.json` with the same care as SSH private keys or API secrets.
- Use this tool at your own risk. The author assumes no liability for account suspension, token leakage, rate limit miscalculation, unexpected API charges, or any other damages arising from the use of this tool.
- By using this tool, you accept full responsibility for its operation and any consequences.

## License

MIT
