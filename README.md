# QuotaMux

QuotaMux is a local gateway for DeepSeek V4 Flash 0731. It uses OpenCode Go first and falls back to the official DeepSeek API before a response is committed.

## Run

```sh
cp quotamux.example.toml quotamux.toml
# Add both API keys.
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

## Test

```sh
cargo test --all-targets
cargo run --bin quotamux-smoke -- --base-url http://127.0.0.1:8080
```

`quotamux.toml`, prompts, and model responses are never committed or stored in the statistics database.

## License

MIT
