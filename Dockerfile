FROM rust:1-slim-bookworm AS build
WORKDIR /app
# Cache dependencies separately from source.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && cargo build --release && rm -rf src
COPY src ./src
RUN touch src/main.rs && cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /app/target/release/higgsfield-mcp /usr/local/bin/higgsfield-mcp
USER nobody
ENV PORT=8080
EXPOSE 8080
CMD ["higgsfield-mcp", "--http"]
