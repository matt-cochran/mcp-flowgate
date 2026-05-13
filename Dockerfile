FROM rust:1.83-slim AS builder
WORKDIR /src
COPY . .
RUN cargo build --release -p mcp-flowgate

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/mcp-flowgate /usr/local/bin/mcp-flowgate
ENTRYPOINT ["mcp-flowgate"]
CMD ["serve", "--config", "/config/gateway.yaml"]
