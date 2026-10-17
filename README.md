# hats

[![ci](https://github.com/frizynn/hats/actions/workflows/ci.yml/badge.svg)](https://github.com/frizynn/hats/actions/workflows/ci.yml)
[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Switch Claude Code and Codex between accounts. One small binary, no daemon,
no credentials touched.

Each account is a full configuration directory (`CLAUDE_CONFIG_DIR` for Claude
Code, `CODEX_HOME` for Codex), which is how Anthropic documents running several
accounts side by side. You sign in to each one with the harness's own login.
hats creates the directories, shares your settings into them, picks which one a
session starts with and, if you want, moves to another account when one gets
close to its plan limit.

```
$ hats list
CLAUDE CODE · new sessions start in ● work
Auto: at 90 % of either limit, it moves to the account with the most room.
  ACCOUNT   5-HOUR LIMIT               WEEKLY LIMIT               UPDATED
● work      ▰▰▰▰▰▰▰▰▱▱  81% 14:20      ▰▰▱▱▱▱▱▱▱▱  23% Thu 09:00  just now
    sam@acme.example · Team · Acme
  personal  ▰▱▱▱▱▱▱▱▱▱  12% 16:05      ▰▰▰▰▱▱▱▱▱▱  41% Mon 11:00  2 h ago
    sam@example.com · Max
  default   not signed in: hats login default
    ~/.claude
```

## Install

macOS and Linux.

**Prebuilt binary.** Each [release](https://github.com/frizynn/hats/releases)
has an archive per platform (`aarch64-apple-darwin`, `x86_64-apple-darwin`,
`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`; the Linux ones are
static and run on any distribution) and a `SHA256SUMS` file. This picks yours
and puts `hats` in `~/.local/bin`, which must be on your `PATH`:

```sh
target=$(uname -m | sed s/arm64/aarch64/)-$(uname -s | sed 's/Darwin/apple-darwin/;s/Linux/unknown-linux-musl/')
mkdir -p ~/.local/bin
curl -fsSL "https://github.com/frizynn/hats/releases/latest/download/hats-$target.tar.gz" | tar -xz -C ~/.local/bin hats
```

**With Cargo.** Requires Rust 1.89 or later; installs `~/.cargo/bin/hats`.

```sh
cargo install --git https://github.com/frizynn/hats --locked
```

**From a clone.**

```sh
git clone https://github.com/frizynn/hats
cd hats
cargo install --path . --locked
```

Then, optionally, shell completions:

```sh
hats completions zsh > ~/.zfunc/_hats   # or bash, fish, elvish, powershell
```

## Quick start

```sh
hats add work          # a new account that shares your settings and history
hats login work        # Claude Code's own sign-in, inside that account
hats add personal && hats login personal
hats setup             # hooks, status line and shell functions, shown before writing
hats                   # see your accounts and their limits
```

From then on, `claude` in a new terminal starts with the current account. Your
existing `~/.claude` login keeps working as the account named `default`.

`hats setup` shows every change before writing and keeps a backup of each file
next to it, never over an earlier one. Running it again changes nothing, or
points the hooks at the new place if you moved the binary. `hats setup --undo`
takes it all out, and writes back your original bytes if you changed nothing
else in the meantime.

For Codex, add `--codex` to the same commands: `hats --codex add personal`,
`hats --codex login personal`.

## How it works

- **Accounts are directories.** `hats add work` creates
  `~/.local/share/hats/claude/work/` and links into it, from `~/.claude`, the
  things that should be the same everywhere: `settings.json`, `CLAUDE.md`,
  `keybindings.json`, `agents/`, `commands/`, `skills/`, `plugins/`,
  `output-styles/`, and the history (`projects/`, `history.jsonl`,
  `file-history/`, `todos/`, `plans/`), so `claude --resume` sees every
  session whichever account ran it. The login and Claude Code's own state
  (`.claude.json`, caches) stay per account. For Codex, `config.toml`,
  `AGENTS.md`, `hooks.json`, `history.jsonl`, `rules/`, `skills/`, `prompts/`
  and `plugins/` are shared; sessions stay per account.
- **Credentials stay with the harness.** Claude Code keys its macOS Keychain
  entry (or its `.credentials.json` on Linux) to each directory; Codex keeps
  `auth.json` or its keyring entry per `CODEX_HOME`. hats never opens them.
  To know which provider account a directory is signed in to, it reads only
  `oauthAccount.accountUuid` and `organizationUuid` from that directory's
  `.claude.json`, or `creator_account_id` from the newest Codex rollout header.
- **Nothing stays running.** `hats run` replaces itself with the harness
  (`exec`), except for interactive Claude Code sessions in auto mode, where a
  small parent stays to move the session when needed (see below). Hooks and the
  status line wrapper are short processes Claude Code already starts.

## Automatic switching

On by default for both harnesses; `hats config set claude.mode manual` turns it
off.

hats knows each account's limits only from what the harness already shows:

- Claude Code passes `rate_limits.five_hour` and `seven_day` to the status line
  command. `hats statusline` keeps the last values per account in
  `limits.json` and then runs your own status line command with the same input.
- Codex writes `rate_limits` into every `token_count` event of its rollouts;
  hats reads the newest one of each account.
- A turn that ends with a rate limit error (`StopFailure` with
  `error: "rate_limit"`) counts that account as full for five hours, or until
  its status line reports other numbers: the hook input carries no reset time.
- `hats import claude-swap` takes claude-swap's last reading of each account,
  with the time claude-swap took it.

A window whose reset time already passed counts as empty, so an old reading
still says when an account is usable again.

**New sessions.** When the current account passes `threshold` (default 90 %
of the 5-hour or weekly limit, whichever is higher), the default moves to
another signed-in account that is at least 10 points under the threshold:
the one with the most room (`strategy = "headroom"`) or the next in `order`
(`strategy = "next"`). An account with no numbers yet may be the spent one, so
it is taken only when no account with numbers has room, the first in `order`. This happens in the status line wrapper, in the
`StopFailure` hook and when `hats run` starts. If no account has room, nothing
moves.

**Running sessions.** Started with `hats run` (or the `claude` function that
`hats setup` installs), an interactive Claude Code session has hats as its
parent: one process, about 1 MB, asleep in `sigwait` until something happens,
plus a forked sentinel asleep on a pipe that ends Claude Code with `SIGHUP` if
the parent is ever killed outright, so the agent is never left running in the
background.
When the session's account passes the threshold or hits the limit, hats moves
it only right after a turn ends: within 2 seconds of the `Stop` hook, with no
prompt after it. It waits one more second, ends Claude Code with `SIGTERM` and
starts it again in the next account with `--resume <session>`, in the same
terminal. A prompt sent in that second postpones the move to the next turn
end, so a turn is never cut. The status line also repaints while the session
sits idle (a permission mode change, the prompt cache expiring); a threshold
seen then waits for the next turn to end, so a prompt you are typing is never
lost. On Claude Code 2.1.301 the status line runs 0.31 to 0.34 s after the
`Stop` hook, well inside the 2 seconds. Claude Code restores the terminal on
`SIGTERM`; the conversation continues from its transcript.

Limits of the running-session move:

- Only Claude Code. Codex sessions keep their account; new Codex sessions
  start on the account with room.
- After an interrupted turn (Esc), Claude Code does not run the `Stop` hook,
  so the move waits for the next completed turn.
- hats rebuilds the command line with `--resume`, keeping your options. If it
  finds an option it does not know followed by a value, it does not supervise
  that session (it starts it with `exec`) rather than guess.
- The move restarts Claude Code: background shells and MCP servers it started
  are restarted too.
- If Claude Code dies of a signal it cannot handle (`kill -9`, the OOM
  killer), hats restores the terminal settings and modes before exiting.

Every change of account is recorded in the registry with its reason
(`manual`, `threshold` or `limit`).

## Token use

```
$ hats usage
USAGE · what these tokens cost at API list price, not what a plan charges

                                          TODAY     7 DAYS    30 DAYS    OCTOBER
Total                                    $31.16    $127.83    $371.27    $330.87
  tokens                                  46.3M     236.9M     702.4M     605.5M

ACCOUNT
  claude work                            $29.34    $108.80    $319.94    $288.97
  claude personal                             -     $14.18     $43.98     $34.55
  codex  personal                         $1.83      $4.85      $7.35      $7.35
MODEL
  Opus 5.5                               $29.34    $107.16    $313.67    $284.49
  Sonnet 5.5                                  -     $14.18     $43.98     $34.55
  GPT-6.1 Sol                             $1.83      $4.85      $7.35      $7.35
  Haiku 5.5                                   -      $1.65      $6.26      $4.48
PROJECT
  api                                    $18.46     $65.50    $190.72    $178.49
  web                                    $12.70     $52.21    $150.46    $129.79
  notes                                       -      $8.48     $23.83     $18.11
  infra                                       -      $1.65      $6.26      $4.48

Logs from 2026-09-27; hats keeps what it counted after Claude Code deletes a
transcript (30 days). Prices of 2026-10-16.
```

`hats usage` adds up the tokens in Claude Code's transcripts and Codex's
rollouts, the files the harnesses already keep, per day, account, model and
project. The cost is what those tokens would cost at the API's list price,
with the prices' date and sources in `src/usage/prices.rs`. Claude and ChatGPT
plans are flat-rate, so it is not what you pay. From 110 columns each cost has
its tokens beside it. `hats usage --json` prints one row per day, harness,
account, model and project, with every kind of token and its cost.

How it counts:

- Claude Code writes a response once per content block and copies messages
  into subagent and resumed transcripts. Each response counts once, by its
  message id and request id.
- Codex writes running totals. Each row counts what it added, and a rollout
  moved to `archived_sessions/` is the same rollout.
- Codex names the account in every rollout. Claude Code does not, so a session
  counts for the account of its own line in `switches.jsonl`, written by the
  `SessionStart` hook that `hats setup` installs. Without one, it counts for
  the last account seen in `~/.claude` before it (claude-swap's log says which,
  if you used claude-swap), else for `unassigned`.
- hats' accounts share one `projects` folder, and it is read once. A git
  worktree counts with its repository.
- Nothing runs in the background. The first `hats usage` reads every log and
  takes seconds; later ones read only what was appended. Days are cut in
  local time, and what hats counted stays after Claude Code deletes a
  transcript at 30 days.

## Commands

| Command | What it does |
|---|---|
| `hats`, `hats tui` | interactive view: accounts, limits, switch, add, sign in, remove, settings |
| `hats list [--json]` | accounts with their limits; `●` marks the one new sessions start with |
| `hats add <label>` | create an account directory that shares your settings |
| `hats login <label> [-- args]` | `claude auth login` (or `codex login`) inside that account |
| `hats use <label>` | make it the default for new sessions |
| `hats auto [on\|off]` | say whether accounts switch by themselves and the last change, or turn it on or off; nothing has to stay open |
| `hats run [label] [-- args]` | start the harness with that account (default: the current one) |
| `hats env [label]` | print `export CLAUDE_CONFIG_DIR=...` for `eval` or a terminal multiplexer pane |
| `hats remove <label>` | sign out with the harness, then delete the directory (links only, never the shared files) |
| `hats config [list\|get\|set\|edit\|path]` | settings; `hats config` lists every key with its meaning |
| `hats setup [--dry-run\|--undo]` | install or remove the hooks, the status line wrapper and the shell functions; run it again after moving the binary |
| `hats usage [--json]` | tokens per day, account, model and project from the agents' own logs, and what they would cost at API list price |
| `hats import claude-swap [--from <dir>]` | take claude-swap's account order, settings and last limits (never its tokens) |
| `hats completions <shell>` | shell completions |
| `hats statusline [-- your command]` | the status line wrapper `setup` installs |

Add `--codex` to act on Codex accounts. Every command has `--help`, and
`hats --help` ends with examples; errors say what to run next.

## Configuration

`~/.config/hats/config.toml` (or `$XDG_CONFIG_HOME/hats`, or `$HATS_CONFIG`).
`hats config set` edits it in place and keeps your comments; `hats config edit`
opens it and checks it when you close the editor.

```toml
[claude]
mode = "auto"          # auto | manual
threshold = 90         # 1-100, percent of the 5-hour or weekly limit
strategy = "headroom"  # headroom | next
order = ["work", "personal"]   # preferred order; others follow by name
relaunch = "idle"      # idle | off: move running `hats run` sessions

[codex]
mode = "auto"
threshold = 90
strategy = "headroom"
order = []
relaunch = "idle"
```

## Where things live

| What | Where | Mode |
|---|---|---|
| Account directories | `$XDG_DATA_HOME/hats/<harness>/<label>/` (default `~/.local/share/hats`), or `$HATS_HOME` | 0700 |
| Registry of account changes | `$XDG_STATE_HOME/hats/switches.jsonl` (default `~/.local/state/hats`), or `$HATS_STATE_DIR` | 0600 |
| Last limits per account | `$XDG_STATE_HOME/hats/limits.json` | 0600 |
| Current account per harness | `$XDG_STATE_HOME/hats/current-claude`, `current-codex` | |
| Token totals and scan offsets | `$XDG_STATE_HOME/hats/usage/` | 0600 |
| Settings | `$XDG_CONFIG_HOME/hats/config.toml` | 0600 |
| Credentials | where the harness keeps them; hats never reads them | |

Keep `$XDG_DATA_HOME` stable: Claude Code names each account's Keychain entry
after the exact directory path, so moving it signs the account out.

## The registry, for other tools

Usage trackers can attribute tokens to accounts with
`switches.jsonl`: one JSON object per line, appended with a single `write(2)`
on an `O_APPEND` descriptor so concurrent writers never interleave.

```json
{"v":1,"at":"2026-01-02T03:04:05.678Z","harness":"claude","account":"9ae25bfbd05c9e76","label":"work","configDir":"/home/user/.local/share/hats/claude/work","reason":"manual"}
{"v":1,"at":"2026-01-02T03:10:00.000Z","harness":"claude","account":"9ae25bfbd05c9e76","label":"work","configDir":"/home/user/.local/share/hats/claude/work","session":"11111111-2222-4333-8444-555555555555"}
```

- `account`: first 16 hex digits of SHA-256 of `"<accountUuid>:<organizationUuid>"`
  (Claude Code) or of `creator_account_id` (Codex). Any tool can derive the
  same id from the harness's own files. Never an email.
- `session`: present when the line comes from the `SessionStart` hook or from
  a running-session move; it binds only that session. Without it, the line
  moves the default account.
- `reason`: `manual`, `threshold` or `limit`, when the account changed.
- Reading rule: a message of session S at time t belongs to the newest line
  with `session == S` and `at <= t`, else to the newest line without
  `session` and `at <= t`, else to no account.

The format is fixed by `tests/registry_format.rs` and
`tests/fixtures/switches.jsonl`. `limits.json` holds
`{"v":1,"accounts":[{"harness","account","label","observedAt","windows":[{"name","usedPercent","resetsAt"}],"limitedUntil"?}]}`.

## Migrating from claude-swap

```sh
hats import claude-swap     # order, threshold and strategy; prints the sign-ins left
hats login account-1        # one sign-in per account it lists, with Claude Code's own login
...
hats setup
```

The import creates one account per enabled claude-swap account (its alias, or
`account-<n>`), keeps claude-swap's order, threshold and strategy, and prints
which accounts to sign in to. The account claude-swap last switched into is
already signed in at `~/.claude`: the import recognizes it by its account id
(`accountUuid` and `organizationUuid`, hashed as in the registry) in
claude-swap's copy of that account's `.claude.json`, keeps it as `default` in
its place in the order and creates nothing for it. It reads
`sequence.json`, `settings.json` and those copies; claude-swap's stored
tokens are never read or copied. When every account is signed in, stop
`cswap auto` and run `cswap purge` to delete them.

| | claude-swap 0.26.0 | hats |
|---|---|---|
| How it switches | rewrites the Keychain item and `~/.claude.json` | one config directory per account |
| Credentials | copies every account's tokens | never reads them |
| Usage data | polls an undocumented endpoint with each token | what the harness already reports |
| Resident process | `cswap auto`, about 27 MB | none (a ~1 MB parent per supervised session) |
| Start-up | about 50 ms, 35 MB | see Measurements |

## What the providers' terms say

Facts and links, so each operator decides. Read on 2026-10-16.

- Anthropic documents one configuration directory per account: "To stay
  signed in to multiple accounts at once, such as work and personal accounts,
  give each account its own configuration directory."
  ([Claude Code authentication](https://code.claude.com/docs/en/authentication#log-in-with-multiple-accounts))
- Anthropic on tools that handle subscription credentials: "developers may
  not collect, store, or intermediate Claude.ai credentials or session
  tokens", and "sign-in to a Claude account must complete through Anthropic's
  own flow."
  ([Legal and compliance](https://code.claude.com/docs/en/legal-and-compliance#authentication-and-credential-use))
  hats does not.
- Anthropic on usage limits: "Advertised usage limits for Pro and Max plans
  assume ordinary, individual usage of Claude Code and the Agent SDK."
  (same page, [acceptable use](https://code.claude.com/docs/en/legal-and-compliance#acceptable-use))
- Anthropic Consumer Terms, section 3, effective October 8, 2025: users must
  not "[bypass] any of our systems or protective measures", nor access the
  services "through automated or non-human means, whether through a bot,
  script, or otherwise" except with an API key or explicit permission.
  ([Consumer Terms](https://www.anthropic.com/legal/consumer-terms))
- Anthropic Usage Policy published with effect from November 12, 2026 adds:
  "Coordinate malicious activity across multiple accounts, or create multiple
  accounts, to avoid detection, circumvent product guardrails, [...]" and
  "Intentionally bypass or interfere with capabilities, restrictions, or
  guardrails established within our products [...]".
  ([Usage Policy](https://www.anthropic.com/legal/aup)) The version in effect
  before that date limits the bypass clause to producing harmful outputs.
- OpenAI documents `CODEX_HOME` as "the root for Codex state, including
  config, auth, logs, sessions" ([environment variables](https://developers.openai.com/codex/environment-variables)).
  The OpenAI Services Agreement, §3.2, forbids to "violate or circumvent Usage
  Limits or otherwise configure the Services to avoid Usage Limits"
  ([PDF](https://cdn.openai.com/osa/openai-services-agreement.pdf)). Search
  results quote OpenAI's consumer [Terms of Use](https://openai.com/policies/row-terms-of-use/)
  with a similar clause ("circumvent any rate limits or restrictions"); that
  page refused automated access, so it was not checked word for word.
- Neither provider has a rule that names switching between one person's own
  subscriptions when one reaches its limit. Automatic switching is on by
  default; `hats config set claude.mode manual` and
  `hats config set codex.mode manual` turn it off.

## Measurements

macOS 26 on Apple silicon, release build, a throwaway HOME with 5 signed-in
accounts whose `.claude.json` is 157 KB each. Peak memory from
`/usr/bin/time -l`; time is the median wall time per process over 100 runs,
which includes about 1.4 ms to start any process (`/usr/bin/true` measures
1.40 to 1.42 ms the same way). Three runs each; the ranges cover them.

| What | Time | Peak memory |
|---|---|---|
| Binary | | 1.5 MB (the interactive view is 0.2 MB of it, `hats usage` 0.2 MB) |
| `hats --version`, `config`, `env` | 2.0 to 2.2 ms | 2.0 to 2.2 MB |
| `hats list` / `list --json` | 2.8 to 3.0 ms | 2.7 to 2.8 MB |
| `hook session-start` / `prompt` / `stop` | 2.0 to 2.2 ms | 2.1 to 2.4 MB |
| `statusline` / chained to another command | 2.7 / 3.7 ms | 2.6 MB |
| Interactive view, open and quit | | 2.7 MB; 0.00 s of CPU in 10 s without keys |
| `hats run` parent, waiting | 0.00 s of CPU in 10 s, 1 thread | 1.4 MB physical footprint (2.6 MB RSS with shared system libraries) |
| Its sentinel, waiting | 0.00 s of CPU in 10 s | 0.8 MB physical footprint (1.0 MB RSS) |
| Processes left running | 0 | |
| `hats usage`, the first time, over a real history of 13.9 GB (3,540 logs) | 2.7 s | 21 to 23 MB |
| `hats usage` after that, with nothing or a few KB new | 18 ms | 14 MB |

`hats usage` was measured on a real history, read only, with a warm disk
cache: wall time and peak memory from `/usr/bin/time -l`, three runs each.

claude-swap 0.26.0 on the same machine: about 50 ms and 35 MB to print its
version, and its auto-switch loop stays resident at about 27 MB.

## Uninstall

```sh
hats setup --undo                      # removes the hooks, status line wrapper and shell functions
hats remove <label>                    # per account: signs out with the harness, deletes the directory
rm -r ~/.local/state/hats ~/.config/hats
rm ~/.local/bin/hats                   # or `cargo uninstall hats` if Cargo installed it
```

## Development

```sh
cargo test                               # every test uses a throwaway HOME and fake harnesses
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

CI runs these on macOS and Linux for every pull request. Pushing a `v*` tag
builds the four archives and publishes a release.

## License

[MIT](LICENSE)
