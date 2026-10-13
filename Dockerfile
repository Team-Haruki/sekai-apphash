# Static musl build (cargo-chef caches the dependency layer), slim Alpine runtime.
FROM rust:1.99-alpine AS chef
RUN apk add --no-cache musl-dev \
    && cargo install cargo-chef --version 0.1.78 --locked
WORKDIR /app

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json ./recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json --bin sekai-apphash
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked --bin sekai-apphash

FROM alpine:3.24
# Unpinned on purpose: exact Alpine package pins break the build whenever the
# branch ships a fix (tzdata moves several times a year).
RUN apk --no-cache add ca-certificates tzdata \
    && addgroup -S haruki \
    && adduser -S -G haruki haruki \
    && install -d -o haruki -g haruki /app /app/state
WORKDIR /app
COPY --chown=haruki:haruki --from=builder /app/target/release/sekai-apphash .
ENV TZ=Asia/Shanghai \
    RUST_LOG=info \
    MIMALLOC_ARENA_EAGER_COMMIT=0 \
    SEKAI_APPHASH_CONFIG=/app/config.toml
ARG VERSION=dev
LABEL org.opencontainers.image.version="${VERSION}"
USER haruki
ENTRYPOINT ["./sekai-apphash"]
CMD ["serve"]
