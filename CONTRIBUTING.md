# Contributing

Thanks for helping. This page covers the development setup, the checks every change must
pass, and how commits are written. How the code is organised: [docs/architecture.md](docs/architecture.md).

## Setup

You need:

- Rust 1.85 or newer. `rust-toolchain.toml` selects `stable` with `rustfmt` and `clippy`.
- A native PostgreSQL 14 or newer that your user can create databases in. Tests do not
  start containers; they connect to databases you create once.
- `shellcheck`, if you change `install.sh`.

Create the test databases (one per test suite, so suites can run in parallel):

```sh
for db in cvc_core_test cvc_engine_test cvc_apps_test cvc_accounts_test \
          cvc_accounts_fresh_test cvc_mcp_test cvc_bin_test; do
  createdb -w "$db"
done
```

Then point the tests at them. Each suite reads its own variable:

| Variable | Database | Suite |
|---|---|---|
| `CANVAS_CLOUD_TEST_CORE_DATABASE_URL` | `cvc_core_test` | `crates/core` |
| `CANVAS_CLOUD_TEST_ENGINE_DATABASE_URL` | `cvc_engine_test` | `crates/engine` |
| `CANVAS_CLOUD_TEST_APPS_DATABASE_URL` | `cvc_apps_test` | `crates/apps` |
| `CANVAS_CLOUD_TEST_ACCOUNTS_DATABASE_URL` | `cvc_accounts_test` | `crates/accounts` |
| `CANVAS_CLOUD_TEST_ACCOUNTS_FRESH_DATABASE_URL` | `cvc_accounts_fresh_test` | `crates/accounts` (`tests/signup.rs`: drops and recreates its schema, so it needs a database of its own) |
| `CANVAS_CLOUD_TEST_MCP_DATABASE_URL` | `cvc_mcp_test` | `crates/mcp` |
| `CANVAS_CLOUD_TEST_DATABASE_URL` | `cvc_bin_test` | `crates/canvas-cloud` |

The defaults in the test files point at one developer's local setup (a fixed user and port
5433), so set all seven. For a local PostgreSQL on the default port that lets your user
connect over TCP to `localhost`:

```sh
for v in CORE:cvc_core_test ENGINE:cvc_engine_test APPS:cvc_apps_test \
         ACCOUNTS:cvc_accounts_test ACCOUNTS_FRESH:cvc_accounts_fresh_test MCP:cvc_mcp_test; do
  export "CANVAS_CLOUD_TEST_${v%%:*}_DATABASE_URL=postgres://localhost:5432/${v#*:}"
done
export CANVAS_CLOUD_TEST_DATABASE_URL=postgres://localhost:5432/cvc_bin_test
export PGCONNECT_TIMEOUT=10
```

Add `user@` to the URLs when your PostgreSQL role differs from your login, and keep a
password in `~/.pgpass` (or `user:password@` in the URL) when it asks for one. CI uses these same URLs; see `.github/workflows/ci.yml`.

To run the server while developing, follow the [quickstart](README.md#quickstart) with its
own database (not one of the test databases).

## Checks

Every change must pass these three, the same as CI:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -j4 -- -D warnings
cargo test --workspace --locked -j4 -- --test-threads=4
```

If you change `install.sh` or `packaging/canvas-cloud.service`, also:

```sh
shellcheck -s sh install.sh
sh install.sh --print-unit | diff -u packaging/canvas-cloud.service -   # must print nothing
```

`install.sh` is POSIX `sh` (it runs under `dash`): no bash-isms. `install.sh --dry-run` shows
what a fresh install would configure without root.

## Changes

- **Code and docs go together.** The docs in `docs/` describe the code exactly: routes,
  status and error codes, config keys, metrics, CLI flags. A change to any of them updates the
  doc in the same commit. New config keys go in [docs/configuration.md](docs/configuration.md),
  new metrics in [docs/operations.md](docs/operations.md).
- **The client contract is stable.** The canvas REST API, the WebSocket protocol and the
  formats listed in [docs/v0.1.0.md](docs/v0.1.0.md#compatibility) may grow but not break.
  If a change must break them, say so in the pull request and add it under **Breaking** in
  [CHANGELOG.md](CHANGELOG.md).
- **Migrations** go in `crates/canvas-cloud/migrations`, numbered in the owning crate's range
  ([operations.md](docs/operations.md#upgrades-and-migrations)), and are never edited after a
  release: add a new one.
- **The server knows no specific client.** Keep product names of clients out of the code,
  config defaults and docs; integrations go through linked apps and config
  (`web_app_url`, `ops.extra_url_schemes`, `cors.allowed_origins`).
- **Secrets never reach logs.** Do not log tokens, passwords, keys, document contents or
  update bytes.
- Add a line to [CHANGELOG.md](CHANGELOG.md), under an `## Unreleased` heading at the top
  (create it if missing), for anything a user or operator would notice.

## Commits

- One logical change per commit, with its tests and docs.
- Subject: `area: what changed`, lower case, imperative or descriptive, no trailing period,
  about 72 characters or less. The area is the crate or file: `core`, `engine`, `accounts`,
  `apps`, `mcp`, `bin`, `docs`, `ci`, `install.sh`. Examples from the history:

  ```text
  engine: scope the people directory; admin open_rooms/close_room/with_room_shut
  bin: apps set <id> --allow-import/--trust-email; apps list shows canvases
  ci: create cvc_apps_test for the canvas-cloud-apps tests
  ```

- A body when the reason is not obvious from the diff: what was wrong, why this fix.

## Reporting bugs and vulnerabilities

Bugs: open an issue with the version (`canvas-cloud version`), what you did, what you
expected, and the relevant log lines (`doctor` output helps). Vulnerabilities: never in a
public issue; see [SECURITY.md](SECURITY.md).

By contributing you agree that your contributions are licensed under the [MIT
license](LICENSE).
