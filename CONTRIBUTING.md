# Contributing to Yggdrasil

Thanks for your interest. Yggdrasil is in active development; the public API and schema move quickly, but the contribution flow is meant to be friendly.

## Workflow

1. **Branch from `main`.** Use a short, descriptive name (`scheduler-fanout`, `bench-scenario-3`, `fix-lock-race`).
2. **Work in small, focused commits.** Match the style of recent log: imperative, lowercase, area-prefixed (`scheduler:`, `bench:`, `tui:`, `docs:`). One commit can span multiple files; bundling related work is fine.
3. **Open a PR into `main`.** CI runs `cargo fmt --check`, `cargo check --all-targets`, and `cargo test` (embedded SQLite — every test provisions its own migrated temp database, no service containers). Clippy runs advisory while we burn down existing warnings.
4. **Reference any related tasks** (`yggdrasil-NNN`) in the PR description so the rollup updates.
5. **Squash or rebase merges** are both fine; no merge commits into `main` please.

Direct pushes to `main` are reserved for trivial fixes (typos, generated artifacts) at the maintainer's discretion. Default to a PR.

## Setting up

```bash
make install                      # cargo build --release && copy to ~/.local/bin
ygg init                          # install hooks, create the SQLite DB, run migrations
ygg up                            # tmux dashboard
```

## Tests

- **Library tests** are fast and pure-Rust: `cargo test --lib`.
- **Integration tests** run against an embedded SQLite database — each test creates its own migrated temp DB, so `cargo test` needs no services and runs fully parallel. Set `DATABASE_URL` (a `sqlite://` URL) to point them at a persistent database instead.
- **Bench tests** use a fake `claude` binary at `benches/fixtures/fake-claude.sh` so they run in CI without API tokens. Real `ygg bench` runs invoke the real `claude` CLI; set `YGG_BENCH_CLAUDE_BIN` to override.

## ADRs

Non-obvious architectural choices land as Architecture Decision Records under `docs/adr/`. New ADRs:

1. Copy the most recent ADR for shape.
2. Number sequentially (zero-padded to 4 digits).
3. State alternatives you rejected and *why* — future maintainers need to know what you considered.
4. Link from `docs/adr/README.md`.

## Conventions

- Match existing file style; don't impose a different one.
- Read before write — when in doubt, look at neighboring code.
- Keep PRs focused. A bug fix shouldn't carry surrounding cleanup unless explicitly noted.
- Don't add docstrings, comments, or type annotations to code you didn't change.
- Validate at system boundaries only (CLI args, env, webhooks). Trust internal code.

## AI-agent contributors

Many commits land via AI-driven sessions (see the `Co-Authored-By: Claude Opus...` trailers). Same rules apply: focused PRs, tests, ADRs for architectural choices. The repo's own `CLAUDE.md` and `AGENTS.md` carry the per-agent conventions.

## Reporting issues

Issues welcome. For bugs, include reproduction steps, expected behavior, what you observed, and the relevant `ygg logs` excerpt if applicable. For design questions, prefix the title with `[design]`.

## License

By contributing, you agree your contribution will be licensed under the MIT License (see `LICENSE`).
