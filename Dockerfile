# syntax=docker/dockerfile:1
FROM rust:1.90-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
    && cp target/release/codex-relay /codex-relay \
    && mkdir -p /out/data

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /codex-relay /usr/local/bin/codex-relay
# Owned by the distroless `nonroot` user (65532) so a fresh named volume is writable.
COPY --from=build --chown=65532:65532 /out/data /data
USER nonroot
ENV CODEX_RELAY_DATA_DIR=/data
VOLUME ["/data"]
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD ["/usr/local/bin/codex-relay", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/codex-relay"]
CMD ["serve"]
