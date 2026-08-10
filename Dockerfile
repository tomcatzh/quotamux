# syntax=docker/dockerfile:1.7
FROM rust:1.97-alpine AS builder
WORKDIR /src
RUN apk add --no-cache build-base ca-certificates cmake make perl
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked --bin quotamux --bin quotamux-smoke
RUN mkdir -p /runtime-data

FROM scratch
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=builder /src/target/release/quotamux /quotamux
COPY --from=builder --chown=65532:65532 /runtime-data /data
USER 65532:65532
EXPOSE 8080
VOLUME ["/data"]
ENTRYPOINT ["/quotamux"]
CMD ["--config", "/config/quotamux.toml"]
