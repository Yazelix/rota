# Agent Guidelines

rota picks which coding-agent subscription runs the next agent session. See
[README.md](README.md) for the design and
[docs/references.md](docs/references.md) for prior art.

## Code

- Rust, edition 2024.
- Before every commit, all of these must pass:
  - `cargo fmt --check`
  - `cargo clippy --all-targets -- -D warnings`
  - `cargo test`

## Planning

- Plan work in Beads with `br`. Start from `br ready`.
- Never edit files under `.beads/` by hand; use `br` commands.
- Run `br sync --flush-only` before committing so `.beads/issues.jsonl` is
  current.

## Personal details stay out

Machine-specific details (home paths, account names, quota thresholds) never
go in the repository: not in code, tests, docs or beads. They belong in the
user's config file. The repository ships only an example config with
placeholder values. Test fixtures use made-up data.
