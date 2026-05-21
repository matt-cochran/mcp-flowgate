FROM rust:1.83-slim AS builder
WORKDIR /src
COPY . .
RUN cargo build --release -p mcp-flowgate

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/mcp-flowgate /usr/local/bin/mcp-flowgate

# Ownership annotation for the official MCP Registry. The value MUST
# match the `name` field in server.json — the registry reads this label
# off the published image to confirm the publisher owns the namespace.
LABEL io.modelcontextprotocol.server.name="io.github.matt-cochran/mcp-flowgate"

ENTRYPOINT ["mcp-flowgate"]
CMD ["serve", "--config", "/config/gateway.yaml"]
