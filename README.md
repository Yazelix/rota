# rota

Picks which coding-agent subscription runs your next agent session.

You may pay for several coding-agent subscriptions: Claude Code, Codex,
Antigravity, OpenCode and others. rota looks at each one's remaining quota and
how many sessions it already runs, then starts the next agent session on a
backend that has room, with the model and effort level the task asks for.

## Status

Planning. rota is not usable yet: the binary only prints its usage. The first
version is planned in Beads (`br ready`).

## Planned commands

```text
rota launch [--role implement|review] [--level medium|high] [--only BACKEND] <prompt-file>
rota status [--json]
```

- `launch` checks each allowed backend, picks one, and starts the agent CLI
  with the prompt from the file.
- `status` shows each backend's quota readings, running sessions and whether
  it is available. `--json` prints the same as data.

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
