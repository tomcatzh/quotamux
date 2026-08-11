# QuotaMux

QuotaMux is a local, layered model gateway. It exposes user-defined model names,
routes across equivalent provider credentials inside each layer, and falls back
to later capacity/price layers before a response is committed.

## Run

```sh
cp quotamux.example.toml quotamux.toml
# Add provider API keys and validate every route reference.
cargo run --bin quotamux -- --check
docker compose up --build -d
```

The API and dashboard listen on `http://127.0.0.1:8080`.

## API

- `POST /v1/chat/completions`
- `POST /v1/responses`
- `POST /v1/messages`
- `POST /v1/messages/count_tokens`
- `GET /v1/models`

Clients cannot select a provider. Send `X-Relay-Include-Metadata: 1` to receive `X-Relay-*` routing headers.

See [the v2 routing architecture](docs/routing-architecture-v2.md) for the
provider → route layer → served model configuration and
[the affinity design](docs/prompt-prefix-affinity.md) for the optional
memory-only prefix selector. Captured verification data is in
[the test evidence report](docs/test-evidence.md).

## Test

```sh
cargo test --all-targets
cargo run --bin quotamux-smoke -- --base-url http://127.0.0.1:8080
```

`quotamux.toml`, prompts, and model responses are never committed or stored in the statistics database.

## License

MIT
