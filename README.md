# rota

Picks which coding-agent subscription runs your next agent session.

You may pay for several coding-agent subscriptions: Claude Code, Codex,
Antigravity, OpenCode and others. rota looks at each one's remaining quota and
how many sessions it already runs, then starts the next agent session on a
backend that has room, with the model and effort level the task asks for.

## Usage

Build with `cargo build --release` (Linux, Rust 1.85 or later). Copy
`config.example.toml` to `~/.config/rota/config.toml` and replace its placeholder
paths, commands, models and example percentages.

```text
rota launch [--role implement|review] [--level medium|high] [--only BACKEND] [--dry-run] <prompt-file>
rota status [--json]
```

Launch defaults to `implement` and `high`, runs from the current folder and
replaces itself with the selected CLI. It checks backend guards, account checks,
quota limits and running processes before rotating to the next available backend.
Only implementers without `--only` can use the configured fallback, reported as
`FALLBACK`. Exit codes: 2 for invalid input/config, 3 for refusal, 1 for I/O or
launch failure. Status reports availability for both roles.

`--dry-run` prints the pick and shell command, showing the prompt as `$(cat file)`.
It does not write the state, log, lock or trusted settings, run preparation hooks,
or refresh an account. Quota and guard commands must themselves be read-only.
Status also skips refresh and writes no rota files.

## Configuration

`ROTA_CONFIG` selects a config file; otherwise rota uses
`$XDG_CONFIG_HOME/rota/config.toml`, defaulting to `~/.config/rota/config.toml`.
Paths in TOML are literal: use absolute paths; shell variables and `~` are not
expanded. Keep personal state and logs outside the repository.

- `backends` array order is round-robin order. `roles` controls eligibility;
  the fallback backend has an empty role list. Disabled backends never run checks.
- `commands.medium` and `commands.high` are argv arrays, with one whole argument
  `{prompt}` and optional `{cwd}` arguments. They run directly, without a shell.
  OpenCode uses `opencode mini -m MODEL#VARIANT --prompt {prompt}`.
- `env` adds environment variables to commands and probes, including proxy base
  URLs. `unset_env` removes inherited variables, for example on the main fallback.
- `guard` must exit successfully. Configure it to verify the command guard is
  installed **and hooked into that backend**. `required_files`, `account_check`
  and `excluded_roots` support credential/account checks.
- `quota.command` returns data for `quota.parser`: `agy` (tab-separated usage),
  `claude_text` (five-hour/seven-day used percentages, with `None` for missing),
  `claude_json` (usage endpoint JSON), `eon_codex` / `eon_claude` (workspace JSON),
  or `remaining_json` (object mapping window names to remaining percentages).
  `agy` reads the rows of one model group: `agy_group` names it and defaults to
  `Claude and GPT models`. The group needs a weekly row; its five-hour row is
  optional. A limit that names `five_hour` still refuses a group without that
  row unless `allow_missing` is set. A `disabled` value reads as 0% remaining,
  and each row's RFC 3339 reset time is kept when present.
  Reader commands have a configurable timeout covering output collection, and a
  1 MiB output limit. Optional `refresh` runs only after a failed real-launch probe,
  below the backend's cap, then retries once.
- `limits.implement` / `limits.review` compare `min_remaining` inclusively and
  `max_used` exclusively. `strictly_above_min` makes the minimum exclusive;
  `allow_missing` permits absent windows in an otherwise valid reading. `"*"`
  checks every returned window. Eon window keys are `DURATIONm/INDEX` to preserve
  scoped windows with the same duration. Eon snapshots must report `state = "fresh"`;
  missing, stale, error and unknown snapshots refuse. A refused role lists every
  window under its limit as `NAME_left=N%`, adding `(resets TIME)` when the reader
  reports a reset time, for example
  `quota limit: weekly_left=0% (resets 2030-01-08T14:00:00Z)`.
- A backend without a quota reader requires explicit `allow_unknown_quota = true`.
  An unsuccessful configured reader always refuses. The example disables OpenCode
  until a model and guard are chosen.
- `cap` scans Linux `/proc` for an executable basename and optional environment
  values; account-directory aliases are canonicalized. `exclude_args` omits usage
  probes. Counts include every matching process, regardless of role. A lock beside
  `state_file` serializes rota launches through exec; independently started CLIs
  are observed by the process scan rather than controlled by this lock. Caps are
  rechecked before refresh and exec, including resolved executable identities.
  Set `runtime_executables` to the actual binary paths behind wrappers that change
  argv[0]. For an interpreted CLI, set the executable to its interpreter basename
  and `script` to the absolute CLI script path to distinguish other interpreter jobs.
- `state_file` is written atomically. `log_file` receives JSON lines with time,
  folder, role, level, decision and readings, without prompt contents. Fallback and
  refusal are explicit log decisions. `trusted_settings` atomically appends the
  folder to `trustedWorkspaces`, preserving other JSON keys and the file mode.
  `prepare` is an optional list of checked commands run just before exec.

Configuration and reader fixtures are tested with `cargo test`. Before committing,
run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test`.

## Design principles

- **Choose across backends, not across accounts.** rota chooses between
  providers. Several accounts of one provider are only a config option, used
  at your own responsibility under each provider's terms.
- **Concurrency caps per backend.** A backend can allow a set number of
  sessions at a time, for example one on a small plan.
- **Refuse rather than fall back silently.** When no backend fits, rota says
  so and exits. A fallback happens only when the config names one, and rota
  reports it.
- **Require the command guard.** rota refuses a backend whose configured
  command guard (for example [dcg](https://github.com/Dicklesworthstone/destructive_command_guard))
  is missing.
- **Proxy-backed backends.** A backend can point its CLI's base URL at a local
  proxy such as CLIProxyAPI, so adopting a proxy is a config entry.

Personal details (paths, accounts, thresholds) live in your config file. The
repository will ship only an example config.

## Related tools and how rota differs

These tools solve nearby problems well, and rota borrows ideas from several of
them. [docs/references.md](docs/references.md) has the details.

- [caam](https://github.com/Dicklesworthstone/coding_agent_account_manager)
  switches between accounts of each coding CLI, ranks them by live usage, and
  can switch account after a rate limit. rota chooses between backends instead,
  and adds concurrency caps and a model or effort level per task.
- [ntm](https://github.com/Dicklesworthstone/ntm) runs many agents in tmux
  panes and can ask caam to choose an account before it starts a pane. rota is
  a single launcher that any orchestrator can call.
- [T3 Code](https://github.com/pingdotgg/t3code) is a GUI for coding agents
  that shows usage limits across providers, but by design does not route on
  them. rota routes, and borrows its quota-reading approach.
- [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) exposes
  subscription CLIs as an API and pools requests across accounts. rota works
  per session rather than per request, and can use the proxy as a backend.
- [claude-swap](https://github.com/realiti4/claude-swap) and
  [clauth](https://github.com/uwuclxdy/clauth) manage several Claude Code
  accounts. rota covers other providers too.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
