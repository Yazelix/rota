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
rota run --brief FILE --name NAME --timeout DURATION [--role implement|review] [--only BACKEND] [--cwd DIR] [--effort LEVEL] [--add-dir DIR]...
rota ps [--json] [--all]
rota status [--json]
```

Launch defaults to `implement` and `high`, runs from the current folder and
replaces itself with the selected CLI. It checks backend guards, account checks,
quota limits and running processes before rotating to the next available backend.
Only implementers without `--only` can use the configured fallback, reported as
`FALLBACK`. Exit codes: 2 for invalid input/config, 3 for refusal, 1 for I/O or
launch failure. Status reports availability for both roles; a role a backend
runs only through `rota run` shows as `available for rota run`.

`--dry-run` prints the pick and shell command, showing the prompt as `$(cat file)`.
It does not write the state, log, lock or trusted settings, run preparation hooks,
or refresh an account. Quota and guard commands must themselves be read-only.
Status also skips refresh and writes no rota files.

`rota run` runs one headless agent session (a plan, review or investigation)
and waits for it. It checks guards, account checks, quota limits and caps like
`launch`, with no fallback, then runs the backend's `run` command for the role
in `--cwd` (default: the current folder). It does not apply `trusted_settings`
or `--level`. Its stdin is the brief file or
`/dev/null`, never rota's own. Output goes to a log under a run directory named
by the session, `NAME-N` with the first free `N`; rota prints the pick, session,
PID, log and report paths before the run starts. `--timeout` takes seconds or
`Ns`, `Nm`, `Nh`, at most 30 days, and counts from the moment the run starts.

`rota run` needs Linux 5.14 or newer with cgroup v2 and a systemd user manager,
systemd 254 or newer (tested on 255; systemd-run gets `--expand-environment=no`,
so arguments stay literal on 258 and later too). It starts the command with
`systemd-run --user --scope` in a transient unit `rota-ID-HEX.scope`, where HEX
is 128 random bits, so the unit's cgroup holds every process the run starts,
whatever their process group, session, parent or threads. Before it claims a
session, rota starts a throwaway scope that checks its own `cgroup.events` and
`cgroup.kill`; if that fails or takes more than 10 seconds, rota exits 1. A run
always gets rota's own `XDG_RUNTIME_DIR` and `DBUS_SESSION_BUS_ADDRESS`, the
variables that reach the manager, and `[run]` may not unset or set them.

rota acts on the unit only once it has seen the run's main process inside it,
so a name that was already taken is never signalled. When the run's main
process exits, at the timeout, or when rota gets SIGINT, SIGTERM or SIGHUP, rota
sends SIGTERM and SIGCONT to every process in the unit's cgroup, waits up to 5
seconds for it to empty, then sends SIGKILL (and writes `cgroup.kill`) for up to
10 seconds. rota signals the processes directly through pidfds, never through
the systemd manager, so a stalled manager cannot delay cleanup, and it skips any
listed PID that has since left the unit. The run ends only when
`cgroup.events` confirms the cgroup is empty. If rota cannot confirm that by
then (processes stuck in uninterruptible I/O, or a cgroup or process membership
it cannot read) the run is abandoned: rota prints the PIDs it can see, records
them, and exits 125, never 0. If systemd-run fails before the command starts
(rota read the main process's cgroup and it is not the unit's), the outcome is
`failed` and rota exits 1; the log says why.
rota exits with the run's status: 128+N when a signal N ended it, 124 after a
timeout, 130 after an interruption and 125 when it abandoned processes. If the
final record write fails, rota exits 1. A refusal exits 3 and an unusable
option, backend or brief exits 2, as for `launch`, before anything starts. A run's own status can also be 1, 2 or 3: a run started
when the `session:` line was printed, and its record's `outcome` says how it
ended.

Each run is recorded in the session registry: `sessions/ID.json` beside
`state_file`, naming the run's `unit`, rewritten when the run ends with `finished` (Unix seconds),
`outcome` (`exited`, `signaled`, `timeout`, `interrupted`, `failed` or `abandoned`) and
`exit_code`; an abandoned run also lists `abandoned_pids`.
Run directories are `runs/ID/` beside `state_file`, holding `log` and the
`report.md` path given to the command. Readers ignore fields they do not know.
Writers hold `sessions/.lock` and merge their fields into the record as it is
on disk, so fields another command adds during a run are kept. A record that
exists but cannot be read or parsed is never replaced. `rota run` takes the lock
before the run starts; the final write waits for it at most 10 seconds.

`rota ps` lists the registry's sessions that have not finished, oldest
first; `--all` adds finished ones. Each has a state: `running` while its
process is alive, `dead` when the process is gone but the record never got
`finished` (rota itself was stopped), or `finished`. A process is alive while
`/proc/PID` exists and, for a record that names a `unit`, the process is in
that unit, so a reused PID never passes for the session. The text form prints
one line per session: ID, state, backend, role, PID, age, folder, log, and the
pane, outcome and exit code when the record has them. `--json` prints the
records as stored, each with `state`, `alive` and `age_seconds` (seconds since
`started`) added. ps reads the registry without its lock and writes nothing.
A record it cannot read or parse is reported on stderr, the others are still
printed, and rota exits 1.

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
  probes. Counts include only the matching processes of the user running rota,
  regardless of role; a process that exits or changes owner mid-scan is
  skipped, and a live matching process whose environment or working directory
  cannot be read fails the check. A lock beside
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
- `[backends.run]` makes a backend usable by `rota run`. `commands.implement`
  and `commands.review` are argv arrays; a role with a command can run headless
  even when `roles` leaves it out, and its quota limits apply. `{brief}`,
  `{cwd}`, `{report}` and `{effort}` are replaced inside any argument;
  `{add_dirs}` is a whole argument, never the first, that repeats `add_dir` (an
  argv fragment with one `{dir}` argument) once per `--add-dir`. `stdin` is `"brief"` or `"null"`;
  with `"null"` every command must name `{brief}`. `--effort` must be one of
  `efforts` and defaults to `default_effort`.
- `[run]` changes the environment of headless runs only, never of checks.
  `unset_env` removes inherited variables, where a trailing `*` matches a name
  prefix, then `[run.env]` sets variables, then the backend's `unset_env` and
  `env` apply. Use it to keep a run away from a desktop or terminal session.

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
