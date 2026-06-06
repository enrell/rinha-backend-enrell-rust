FROM rust:1.85-alpine AS builder
WORKDIR /app
RUN apk add --no-cache musl-dev
COPY Cargo.toml .
COPY src ./src
RUN cargo build --release --bin lb --bin api --bin preprocess

FROM builder AS indexer
COPY resources /app/resources
RUN gunzip -k /app/resources/references.json.gz
RUN /app/target/release/preprocess /app/resources/references.json /app/index

FROM alpine:3.21
WORKDIR /app
COPY --from=builder /app/target/release/lb /app/lb
COPY --from=builder /app/target/release/api /app/api
COPY --from=indexer /app/index/index.bin /index/index.bin
EXPOSE 9999
