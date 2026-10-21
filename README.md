# hats

[![ci](https://github.com/frizynn/hats/actions/workflows/ci.yml/badge.svg)](https://github.com/frizynn/hats/actions/workflows/ci.yml)
[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Switch Claude Code and Codex between accounts. One small binary, no daemon.

Each account is a full configuration directory (`CLAUDE_CONFIG_DIR` for Claude
Code, `CODEX_HOME` for Codex) where you sign in with the harness's own login.
Claude Code sessions all run in `~/.claude`: changing account moves that
account's login there, and every open session takes it within about 30
seconds, with no restart, the way claude-swap did. hats reads each account's
plan limits from the provider and, if you want, changes account when the one
in use gets close to its limit.

```
$ hats list
CLAUDE CODE · every session runs on ● work
Auto: at 90 % of either limit, it moves to the account with the most room.
  ACCOUNT   5-HOUR LIMIT               WEEKLY LIMIT               UPDATED
● work      ▰▰▰▰▰▰▰▰▱▱  81% 14:20      ▰▰▱▱▱▱▱▱▱▱  23% Thu 09:00  just now
    sam@acme.example · Team · Acme
  personal  ▰▱▱▱▱▱▱▱▱▱  12% 16:05      ▰▰▰▰▱▱▱▱▱▱  41% Mon 11:00  just now
    sam@example.com · Max
  spare     not signed in: hats login spare
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

From then on every Claude Code session, the ones already open included, runs
on the account in use. A login you made with plain `claude` before hats shows
as `default` until another account takes its place; then it gets an account of
its own, named after its mail, so nothing is lost.

`hats setup` shows every change before writing and keeps a backup of each file
next to it, never over an earlier one. Running it again changes nothing, or
points the hooks at the new place if you moved the binary. `hats setup --undo`
takes it all out, and writes back your original bytes if you changed nothing
else in the meantime.

For Codex, add `--codex` to the same commands: `hats --codex add personal`,
`hats --codex login personal`.

## The interactive view

`hats` opens four tabs; `tab` and `shift+tab` go through them, `1` to `4` jump
to one, `q` closes the view. The foot shows the keys of the tab you are on.
The selected row is bold, with nothing dim, on a background a step from the
terminal's own toward its text color: the largest step that keeps every text
color of the row at 4.5:1 or more, from the colors the terminal says when
asked (OSC 11, 10 and 4). A terminal that does not say its background gets the
mark and bold only.

```
 1 Accounts  2 Usage  3 Settings  4 Activity                  tab next · q quit
────────────────────────────────────────────────────────────────────────────────
 Claude Code 5   Codex 2   every session runs on ● default
Auto: at 90 % of either limit, it moves to the account with the most room.
    ACCOUNT  5-HOUR LIMIT               WEEKLY LIMIT               UPDATED
▌ ● default  ▰▰▱▱▱▱▱▱▱▱  21% 17:42      ▰▰▱▱▱▱▱▱▱▱  15% Wed 18:30  2 min ago
    night    hit its limit at 14:30: counted as full until 19:30
    old off  ▱▱▱▱▱▱▱▱▱▱   0%            ▰▰▰▰▰▰▰▱▱▱  71% Mon 15:30  3 days ago
    side     no data yet: appears within a minute
    spare    not signed in: press l to sign in
    work     ▰▰▰▰▰▰▰▰▰▰ 100% 17:00      ▰▰▰▰▰▰▱▱▱▱  64% Mon 20:30  10 min ago
─ default ──────────────────────────────────────────────────────────────────────
 Mail         dana@example.com
 Organization –
 Plan         Max
 Folder       ~/.claude
 Resets       5-hour 21 % until 17:42 · weekly 15 % until Wed 18:30
 Updated      2 min ago
 Auto switch  on: may move here when the account in use fills
 Last change  14:40  → default: you chose it

↑↓ move  ←→ codex  enter use  space disable  ⇧↑↓ order  a add  l login  d remove
```

| Tab | What it shows | Keys |
|---|---|---|
| Accounts | Claude Code or Codex, one line per account with its limits, and the selected account's mail, organization, plan, folder, resets and last change | `↑↓` move, `←→` the other harness, `enter` use, `space` turn off or on, `shift+↑↓` order, `a` add, `l` sign in, `d` remove (asks first) |
| Usage | tokens and their cost at API list price today, in 7 and 30 days and in the month, by account, model and project, with a bar per day; the first scan runs on its own thread | `←→` one account or all, `esc` all again, `↑↓` scroll, `r` scan again |
| Settings | every key of `config.toml` for both harnesses with its help, where hats keeps its files and whether `hats setup` was applied | `↑↓` move, `←→` change, `enter` to type a value and `enter` again to save it, `esc` cancels; a value that is not valid is refused with the reason |
| Activity | changes of account and sessions closed for going unused, newest first | `↑↓` scroll, `home`/`end` |

`q` closes the view and `esc` never does: it cancels what is open, a value
being typed, a prompt or the account filter of Usage.

The view waits on kqueue (macOS) or inotify (Linux) and redraws when a status
line, a hook or another `hats` changes the state; it uses no CPU while nothing
happens.

## How it works

- **Accounts are directories.** `hats add work` creates
  `~/.local/share/hats/claude/work/` and links into it, from `~/.claude`, the
  things that should be the same everywhere: `settings.json`, `CLAUDE.md`,
  `keybindings.json`, `agents/`, `commands/`, `skills/`, `plugins/`,
  `output-styles/`, and the history (`projects/`, `history.jsonl`,
  `file-history/`, `todos/`, `plans/`). `hats login work` runs Claude Code's
  own sign-in inside it. For Codex, `config.toml`, `AGENTS.md`, `hooks.json`,
  `history.jsonl`, `rules/`, `skills/`, `prompts/` and `plugins/` are shared;
  sessions stay per account.
- **Claude Code runs in one seat.** Every Claude Code session runs in
  `~/.claude`. Changing account puts the chosen account's login in that
  directory's credential item (the Keychain item `Claude Code-credentials` on
  macOS, `~/.claude/.credentials.json` elsewhere) and its identity in the
  `oauthAccount` of `~/.claude.json`; everything else in both stays. The login
  that leaves goes back to its account's directory first, as Claude Code last
  renewed it, so nothing is lost. Claude Code reads the stored login again
  after about 30 seconds (at once when `.credentials.json` changes), so open
  sessions continue on the new account, mid-conversation, without a restart.
- **Claude Code's own locks.** The change runs under the locks Claude Code
  takes to renew a token (`~/.claude/.oauth_refresh.lock`, then
  `~/.claude.lock`, measured on Claude Code 2.1.304): a renewal in flight
  either ends before it, and the renewed login is the one put away, or reads
  the new login after it.
- **Credentials.** hats reads and writes the logins that Claude Code stored, in
  the same place Claude Code keeps them, through `/usr/bin/security` on macOS
  with the secret on standard input, never in a command line. It renews the
  token of an account that is not in use when it has expired, to read its
  limits, and saves the new one at once. It never copies a login anywhere
  else and never prints one.
- **Codex** keeps the earlier behavior: `hats run --codex` starts a session
  with the account's `CODEX_HOME`; open Codex sessions keep their account.

## Automatic switching

On by default for both harnesses; `hats auto off` (or
`hats config set claude.mode manual`) turns it off.

Where the limits come from:

- **The provider.** At most once a minute, hats asks
  `api.anthropic.com/api/oauth/usage` for every signed-in Claude Code account,
  with that account's own token, as Claude Code's `/usage` and claude-swap do.
  The status line wrapper starts it in the background, `hats list` and the
  interactive view run it when they open, and the view again every minute.
  `HATS_OFFLINE=1` turns it off.
- **Claude Code's status line.** Claude Code passes `rate_limits.five_hour`
  and `seven_day` to the status line command on every repaint, the numbers of
  the session's last answer. `hats statusline` runs your own status line
  command with the same input and starts the poll above when it is due. It
  keeps the numbers only for a session started in an account's own directory:
  in `~/.claude` the last answer may have come from the account that sat there
  before, since Claude Code keeps a login it read for about 30 seconds and an
  idle session keeps its last numbers.
- **Rate limit errors.** A turn that ends with a rate limit error
  (`StopFailure` with `error: "rate_limit"`) counts that account as full until
  the full window resets, when the last numbers say when, else for five hours.
  In the first minute after a change of account, an error in `~/.claude` is
  taken as the account that left.
- **Codex** writes `rate_limits` into every `token_count` event of its
  rollouts; hats reads the newest one of each account.

A window whose reset time already passed counts as empty.

**When it switches.** When the account in use passes `threshold` (default
90 % of the 5-hour or weekly limit, whichever is higher), hats moves to
another signed-in account that is at least 10 points under the threshold: the
one with the most room (`strategy = "headroom"`) or the next in `order`
(`strategy = "next"`). An account with no numbers yet may be the spent one, so
it is taken only when no account with numbers has room. This happens in the
status line wrapper, the `StopFailure` hook, after each poll and when
`hats run` starts. If no account has room, nothing moves. For Claude Code the
switch reaches every open session; for Codex, new sessions.

**Disabled accounts.** `hats disable <label>` (or space in the view) keeps
auto switching from ever moving to an account; `hats enable <label>` lets it
again. Disabling the account in use, with auto switching on, moves at once to
the account with the most room. `hats use` still takes it, and says it is disabled. The labels go in
`disabled = [...]` of `config.toml`.

**Choosing an account.** `hats use <label>`, enter in the view, or
`hats run <label>` moves to that account at once, every open Claude Code
session included. Choosing the account already in use changes nothing.

Every change of account is recorded in the registry with its reason
(`manual`, `threshold` or `limit`). `hats remove` refuses the account in use:
choose another first.

## Closing unused sessions

A Claude Code session started with `hats run` (or the `claude` function) that
goes unused for `claude.close_idle`, 32 hours by default, is closed: hats ends
Claude Code with `SIGTERM`, does not start it again, and
prints how to resume it before the shell prompt comes back:

```
hats: closed this session after 32h without use. Resume it with: claude --resume <id>
```

`hats config set claude.close_idle 8` makes it 8 hours; `90m` and `30s` are
minutes and seconds, handy to try it; `0` or `off` turns it off. It works in
manual mode too. A change reaches running sessions at their next check.

What counts as use:

- The clock starts when the session opens (the `SessionStart` hook) and when
  a turn ends (`Stop` or `StopFailure`). A new prompt, another turn end or a
  write to the session's transcript restarts it. On Claude Code 2.1.302 a
  background task that finishes wakes the agent through `UserPromptSubmit`
  too; the transcript covers any wake that does not.
- A background shell still running is use, for at most `claude.close_idle` of
  its run time: a task that never ends, such as a dev server, keeps the
  session open for one more `close_idle` at most.
- During a turn there is no clock, also while Claude Code waits for your
  approval: a session is never closed mid-turn.
- Hours are wall-clock hours and count time the computer sleeps. The parent
  wakes at most every 15 minutes to compare with the clock, because the timer
  it sleeps on may not count sleep; after a long sleep, a session already due
  closes within 15 minutes of waking.

Limits:

- Only Claude Code: a Codex session has no hats parent, so there is no
  `codex.close_idle`.
- Only sessions whose `hats run` parent runs a version with this setting.
  Sessions opened before you installed it keep their old parent and are never
  closed; start them again to get it.
- A session whose last turn was interrupted with Esc has no clock and stays
  open: Claude Code 2.1.302 runs no hook then (no `Stop`, and no
  `idle_prompt` notification either), so it looks like a turn in progress.
- Background agents that run no shell at the moment are not seen: they end
  with Claude Code.

Each close is recorded in `closes.jsonl`, next to the registry (format
below), for the interactive view. The registry itself records
changes of account only.

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
- A claude-swap slot counts as the account in it, with its hats label: hats
  reads the account (never the mail) from claude-swap's copy of that slot's
  `.claude.json` once and keeps it, so the days stay with that account after
  `cswap purge`. A slot whose copy hats never saw shows as `Account <n>`.
- hats' accounts share one `projects` folder, and it is read once. A git
  worktree counts with its repository.
- Nothing runs in the background. The first `hats usage` reads every log and
  takes seconds; later ones read only what was appended. Days are cut in
  local time, and what hats counted stays after Claude Code deletes a
  transcript at 30 days. Each save reaches the disk step by step (a barrier
  on macOS, `fsync` elsewhere, about 5 ms a scan), so a power cut leaves the
  last save whole.

## Commands

| Command | What it does |
|---|---|
| `hats`, `hats tui` | interactive view in four tabs: accounts, token use, settings and activity |
| `hats list [--json]` | accounts with their limits; `●` marks the one new sessions start with |
| `hats add <label>` | create an account directory that shares your settings |
| `hats login <label> [-- args]` | `claude auth login` (or `codex login`) inside that account |
| `hats use <label>` | use it: every Claude Code session, the open ones too; new Codex sessions |
| `hats disable <label>`, `hats enable <label>` | keep auto switching from moving to an account, or let it again |
| `hats auto [on\|off]` | say whether accounts switch by themselves and the last change, or turn it on or off; nothing has to stay open |
| `hats run [label] [-- args]` | start the harness; with a label, use that account first |
| `hats env [label]` | print the shell line for `eval`: `unset CLAUDE_CONFIG_DIR` for Claude Code (with a label, that account is used first), `export CODEX_HOME=...` for Codex |
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
close_idle = "32h"     # close a `hats run` session unused this long; 0 | off: never
disabled = ["old"]     # accounts auto switching never moves to

[codex]
mode = "auto"
threshold = 90
strategy = "headroom"
order = []
disabled = []
```

The Settings tab of the view changes the same keys in place.

## Where things live

| What | Where | Mode |
|---|---|---|
| Account directories | `$XDG_DATA_HOME/hats/<harness>/<label>/` (default `~/.local/share/hats`), or `$HATS_HOME` | 0700 |
| Registry of account changes | `$XDG_STATE_HOME/hats/switches.jsonl` (default `~/.local/state/hats`), or `$HATS_STATE_DIR` | 0600 |
| Sessions closed for going unused | `$XDG_STATE_HOME/hats/closes.jsonl` | 0600 |
| Last limits per account | `$XDG_STATE_HOME/hats/limits.json` | 0600 |
| Current account per harness | `$XDG_STATE_HOME/hats/current-claude`, `current-codex` | |
| What each `hats run` parent is told by the hooks | `$XDG_STATE_HOME/hats/run/<pid>/`, removed when the parent ends; one a killed parent left is removed by the next `hats run`, `hats use` or `hats` | 0700 |
| Token totals and scan offsets | `$XDG_STATE_HOME/hats/usage/` | 0600 |
| Settings | `$XDG_CONFIG_HOME/hats/config.toml` | 0600 |
| Credentials | where the harness keeps them: per account directory, and the one in use in `~/.claude`'s item | |

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
`tests/fixtures/switches.jsonl`.

`closes.jsonl`: one line per session closed for going unused, appended the
same way. No credentials, no account id.

```json
{"v":1,"at":"2026-01-02T03:04:05.678Z","harness":"claude","session":"11111111-2222-4333-8444-555555555555","label":"work","idle":"32h","cwd":"/home/user/project"}
```

- `label`: the account the session ran on when it closed.
- `idle`: `claude.close_idle` at that moment, as `hats config` shows it.
- `cwd`: the directory `hats run` started the session in.

`limits.json` holds
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
| How it switches | rewrites the Keychain item and `~/.claude.json` | the same, under Claude Code's locks |
| Credentials | keeps a copy of every account's tokens in its own store | each login stays in its account's directory, where Claude Code put it |
| Usage data | polls the usage endpoint with each token | the same, at most once a minute, plus the status line |
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
  hats reads and writes the logins Claude Code stored, on your own machine,
  to move them in and out of `~/.claude`, as claude-swap did; it sends them
  only to Anthropic. Decide whether that fits your terms.
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
| Binary | | 1.6 MB; the four tabs of the interactive view added 83 KB, `hats usage` 0.2 MB |
| `hats --version`, `config`, `env` | 2.0 to 2.2 ms | 2.0 to 2.2 MB |
| `hats list` / `list --json` | 2.8 to 3.0 ms | 2.7 to 2.8 MB |
| `hook session-start` / `prompt` / `stop` | 2.0 to 2.2 ms | 2.1 to 2.4 MB |
| `statusline` / chained to another command | 2.7 / 3.7 ms | 2.6 MB |
| Interactive view, open | 3.5 ms to the first frame, 0.2 to 0.3 ms of it counting the Claude Code sessions without hats among the user's 430 processes; 0.1 s more on a terminal that does not say its background | 4.0 MB RSS; 0.00 s of CPU in 10 s without keys |
| `hats run` parent, waiting | 0.00 s of CPU in 10 s, 1 thread | 1.4 MB physical footprint (2.6 MB RSS with shared system libraries) |
| Its check of the clock for closing | 0.2 to 0.5 ms of CPU, at most every 15 minutes while the agent is idle; reading Claude Code's background shells is 6 to 18 µs of it | |
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
