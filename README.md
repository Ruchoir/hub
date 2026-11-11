# Ruchoir hub

Ruchoir is a self-hosted, European collaboration suite: chat, files, calendar, with one identity
service. The hub is its notifications and real-time service, installed with every instance. It keeps
each person's inbox, wakes browsers and phones without carrying any content, mails a digest of what
stays unread, and delivers live updates to connected pages and apps (WebSocket, with SSE as a
fallback). It also serves the instance's manifest at `/.well-known/ruchoir-manifest`, where apps and
pages start their discovery, and holds the push relay (`relay/`) that the publisher runs to wake the
phone apps.

[Propolis](https://github.com/Ruchoir/propolis) signs people in, issues the tokens the hub accepts,
and sends the mail digest the hub words. The products, the [chat](https://github.com/Ruchoir/chat),
the [drive](https://github.com/Ruchoir/drive) and the [calendar](https://github.com/Ruchoir/calendar),
publish to the hub what each person should hear about; the hub never decides who may see what. The
[installer](https://github.com/Ruchoir/installer) sets up an instance and writes the hub's keys,
subscriptions and manifest; [core](https://github.com/Ruchoir/core) holds the shared code. How the
pieces fit: [architecture](https://github.com/Ruchoir/.github/blob/main/docs/architecture.md).

## Status

The hub runs on its stores, PostgreSQL and Valkey, and serves its contract. What works: the inbox
(products publish, people read and mark), notification preferences, live delivery to pages (with a
single-use ticket) and apps (with their token), presence among the members of an organisation, server
events between services with retries for a day, web push, phone push through the relay, the mail
digest, and the instance's manifest. Every authenticated call needs Propolis's tokens.

The push relay lives in `relay/`, but it is still released from the former monolith, with its
secrets, until its release moves here.

## Prerequisites

- Rust: the version pinned in `rust-toolchain.toml` (rustup installs it on first use).
- Docker, for PostgreSQL and Valkey (`compose.yaml`).

## Running it

```bash
docker compose up -d
cargo run
```

The stores answer at the addresses the defaults name, and the hub listens on `127.0.0.1:8080`
(`/healthz`, `/api/v1/health`, `/.well-known/ruchoir-product`). It applies its migrations at start-up;
in production `HUB_AUTO_MIGRATE=false` and `hub migrate` applies them. On its own it accepts no
authenticated call: that needs Propolis (`HUB_ISSUER`).

To run it with Propolis and the products, use the installer's development mode
(`ruchoir-install dev`), with the repositories checked out side by side:
[getting started](https://github.com/Ruchoir/.github/blob/main/docs/getting-started.md).

The push relay's image is built from the repository root:

```bash
docker build -f relay/Dockerfile -t ruchoir-push-relay:0.1.0 .
```

## Testing it

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
RUCHOIR_TEST_DATABASE_URL=postgres://hub:hub@localhost:5432/hub \
RUCHOIR_TEST_VALKEY_URL=redis://localhost:6379 \
cargo test --all-features
```

The tests that need the stores read `RUCHOIR_TEST_DATABASE_URL` and `RUCHOIR_TEST_VALKEY_URL`, and are
skipped without them (except with `RUCHOIR_TEST_STORES=required`, as in the CI). Point them at
throwaway stores only, such as those of `compose.yaml`. The tests never call out: they empty
`HUB_PUSH_RELAY_URL`.

## Contract

The contract of `/api/v1` is derived from the code and recorded in `contract/openapi-v1.json`; the
hub serves it at `/api/v1/openapi.json`. Every route registers in `src/openapi.rs`. The contract only
grows within a major version: `tests/contract.rs` fails when a change removes or breaks something the
recorded file promises, and when the file is not up to date. After adding or changing a route:

```bash
RUCHOIR_UPDATE_CONTRACT=1 cargo test --test contract
```

## Settings

Every environment variable, with its default, is listed in [`AGENTS.md`](AGENTS.md), section
"Settings". The push relay's own variables are in [`docs/push-relay.md`](docs/push-relay.md).

## Documentation

- [`docs/design/hub.md`](docs/design/hub.md): what the hub holds, what moved from the monolith and
  what changed, what a product publishes, preferences, presence, live delivery and server events.
- [`docs/push-relay.md`](docs/push-relay.md): the push relay, what it answers, its keys, and how it
  is run and released.
- [`docs/adr/`](docs/adr/README.md): this repository's decisions (web push, the phone push relay).
- [`AGENTS.md`](AGENTS.md): how to work here, written for contributors and their tools. It opens
  with the common rules of the organization.
- Decisions that span several repositories:
  [Ruchoir/.github, `docs/adr`](https://github.com/Ruchoir/.github/tree/main/docs/adr).

## Contributing

Contributions are welcome. Read [`CONTRIBUTING.md`](https://github.com/Ruchoir/.github/blob/main/CONTRIBUTING.md):
your first pull request asks you to sign the [contributor licence agreement](https://github.com/Ruchoir/.github/blob/main/CLA.md).
Report a vulnerability privately, never in an issue: [`SECURITY.md`](https://github.com/Ruchoir/.github/blob/main/SECURITY.md).

## Licence

`AGPL-3.0-only` (`LICENSE`).
