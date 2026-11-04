# References

A survey of existing tools that route coding-agent sessions across accounts or
providers, made while planning rota (November 2026). It records what each tool
does, what rota can borrow, and why rota is built rather than adopted.

File references are to each upstream repository at the time of the survey.

## Findings

- **caam** (Coding Agent Account Manager),
  <https://github.com/Dicklesworthstone/coding_agent_account_manager>. Go,
  MIT, possibly with the same OpenAI/Anthropic license rider as ntm
  ([ntm `LICENSE`](https://github.com/Dicklesworthstone/ntm/blob/main/LICENSE), line 1).
  - Backends: Claude Code, Codex, Antigravity (agy), Gemini, Grok and Cursor
    Agent. No OpenCode.
  - Each profile gets its own `HOME`, `CODEX_HOME` or `GEMINI_HOME`.
    `caam shallow-spawn` clears an inherited `CLAUDE_CONFIG_DIR`, so accounts
    can run in parallel.
  - `caam limits --rank earliest-reset-headroom` reads live usage; `--cached`
    reads Claude's local cache; `--forecast` exists.
  - `caam run` wraps a CLI and switches account after a rate limit, with a
    cooldown.
  - It has no concurrency cap per backend and no model or effort level per
    task.
- **ntm**, <https://github.com/Dicklesworthstone/ntm>.
  - With `[integrations.caam] seat_selection`, it asks caam to rank accounts
    before launching a pane, and refuses rather than falling back to a pinned
    account (`README.md`, lines 1179-1233).
  - `auto_failover` swaps credentials mid-session (`README.md`, lines
    1225-1229).
  - `--auto-rotate-accounts` restarts a stalled pane on another account and
    keeps the conversation (`README.md`, lines 975-1004). It needs the global
    credentials and rejects isolated `CLAUDE_CONFIG_DIR` profiles.
  - It detects rate limits from CLI output patterns
    (`internal/ratelimit/tracker.go`, lines 504-546) and reads Claude
    percentages from `/status` (`internal/quota/claude.go`, lines 13-18).
- **OpenCode**, <https://github.com/sst/opencode>, has a typed usage schema
  for its Go plan with rolling, weekly and monthly windows (percent, limited,
  resetsAt) (`packages/schema/src/usage-limits.ts`, lines 12-27), served by
  `packages/server/src/handlers/usage.ts`.
- **Codex**, <https://github.com/openai/codex> (tag `rust-v0.163.0`), has a
  `RateLimitSnapshot` with primary and secondary windows plus credits
  (`codex-rs/protocol/src/protocol.rs`, line 2471).
- **T3 Code**, <https://github.com/pingdotgg/t3code>, has no quota routing.
  Multi-account support is a feature request
  (<https://github.com/pingdotgg/t3code/discussions/13660>), and the
  usage-limits display, PR #1732, was open at the time of the survey.
- **Claude-only switchers**: [clauth](https://github.com/uwuclxdy/clauth) (a
  Rust crate), [claude-swap](https://github.com/realiti4/claude-swap),
  claude-auto-switch and Tokenmaxx. None covers Antigravity or OpenCode,
  concurrency, or levels per task.
- **claude-code-router** routes API models, not subscriptions. herdr,
  agentaps, pi-mono, FrankenTerm and Wave Terminal have no account routing.

## T3 Code in more detail

Findings from pingdotgg/t3code at commit `9e855385` (MIT).

- **No router, by design.** The FAQ says: "If we add an auto-switcher, we are
  risking TOS bans for our users" (`docs/user/faq.md`, lines 56-60).
- **Several accounts per backend, picked by hand.** One "instance" per
  account: Claude through `CLAUDE_CONFIG_DIR`, Codex through `CODEX_HOME` plus
  a shadow home, Antigravity through one instance per Google account
  (`docs/user/providers-*.md`).
- **Limits shown, never used to route.** Usage → Limits pools the 5-hour,
  weekly and monthly windows for Codex, Claude, Antigravity, OpenCode Go,
  Cursor and Grok, sorted by soonest refill (`docs/user/usage.md`, lines
  111-175). The readers are in `packages/provider-*/src/server/usageLimits.ts`
  and `packages/shared/src/usageLimits.ts`. These are good reference quota
  readers.
- **Mid-thread provider switching with context handoff**, only when a person
  triggers it (`docs/orchestration-v2/provider-switching-and-context.md`).
- Other repositories from the same author: slotslop (picks a random harness,
  model and effort, as a joke) and tstack (skills). Neither routes.
- **CLIProxyAPI**, <https://github.com/router-for-me/CLIProxyAPI> (MIT, Go),
  is a proxy used in that ecosystem. It pools per request with round-robin,
  fill-first and failover. Its guide warns about ban risk.

## Verdict: build, but borrow

Nothing found covers every requirement: several backends, concurrency caps
per backend, a model and effort level per task, and quota-aware choice. caam
and ntm come closest, but they choose an account within one provider, not the
best backend for a task, and their mid-session credential swap breaks
isolation through `CLAUDE_CONFIG_DIR`.

What rota borrows:

- From caam: the earliest-reset-with-headroom ranking; the refusal to fall
  back silently; possibly `caam limits --json` as the quota reader for Claude,
  Codex and Antigravity.
- Quota readings from OpenCode's Go usage endpoint and Codex's
  `RateLimitSnapshot`.
- T3 Code's quota readers and its context-handoff design.
- ntm's rate-limit output patterns, to spot a limit mid-task.

What rota builds: the concurrency caps and the model and effort levels per
task.

## CLIProxyAPI kept in view

rota starts as its own launcher, with CLIProxyAPI documented as an option:

- A rota backend can be **proxy-backed**: its environment points the agent
  CLI's base URL at a local CLIProxyAPI. Adopting the proxy later is then a
  config entry, not a rewrite.
- Revisit the proxy, or fork it, if per-request pooling or failover within a
  provider in the middle of a session (without restarting the agent) becomes
  necessary.
- Its ban-risk warning applies to that use.
