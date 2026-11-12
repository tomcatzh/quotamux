# syntax=docker/dockerfile:1@sha256:4edf897a3ffa55b89f906fc8cc78afdb3f1834cc9c7083565e611a8a7d5fe99e
FROM rust:1.97.1-alpine3.23@sha256:c4a364ddbf684fe038e6fa6a4f25b30c8dc85247423e0e660676ece0d17be4a2 AS build
ARG TARGETARCH
WORKDIR /src
RUN apk add --no-cache build-base cmake perl binutils linux-headers
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY assets/tls ./assets/tls
COPY frontend/dist ./frontend/dist
RUN --mount=type=cache,id=quotamux-routing-cargo-registry-${TARGETARCH},target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=quotamux-routing-cargo-git-${TARGETARCH},target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=quotamux-routing-target-${TARGETARCH},target=/src/target,sharing=locked \
    set -eu; \
    case "$TARGETARCH" in \
      arm64) rust_target=aarch64-unknown-linux-musl ;; \
      amd64) rust_target=x86_64-unknown-linux-musl ;; \
      *) exit 2 ;; \
    esac; \
    cargo build --locked --release --target "$rust_target" --features standalone-tls --bin quotamux; \
    mkdir -p /out; \
    cp "target/$rust_target/release/quotamux" /out/quotamux; \
    readelf -l /out/quotamux > /out/program-headers.txt; \
    readelf -d /out/quotamux > /out/dynamic-section.txt; \
    if grep -q INTERP /out/program-headers.txt; then exit 3; fi; \
    if grep -q '(NEEDED)' /out/dynamic-section.txt; then exit 4; fi

FROM scratch AS artifacts
COPY --from=build /out/quotamux /quotamux
