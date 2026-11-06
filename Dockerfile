# Контекст сборки это корень проекта:
#     docker build -t batek-rs-tg .
FROM rust:1-alpine AS builder
WORKDIR /build
RUN apk add --no-cache build-base pkgconf

COPY Cargo.toml Cargo.lock* ./
# Без заглушки cargo не находит цель, а слой зависимостей собирается один раз.
RUN mkdir -p src && printf 'fn main() {}\n' > src/main.rs \
    && cargo fetch || true
RUN cargo build --release || true

COPY src ./src
# COPY сохраняет mtime, а заглушка собрана позже; без touch cargo не пересоберёт.
RUN touch src/main.rs && cargo build --release

FROM alpine:3.22
ARG YTDLP_VERSION=2026.10.26
# Контрольная сумма конкретного релиза: без неё подменённый бинарник на
# GitHub или в зеркале попадёт в образ молча.
ARG YTDLP_SHA256=e97a4dd919221d233e7720e18fa3f66db60fbd1d8c0846bf6deb8e88e04475a8
RUN apk add --no-cache ca-certificates curl \
    && curl --proto '=https' --tlsv1.2 -fsSL -o /usr/local/bin/yt-dlp \
       "https://github.com/yt-dlp/yt-dlp/releases/download/${YTDLP_VERSION}/yt-dlp_musllinux" \
    && echo "$YTDLP_SHA256  /usr/local/bin/yt-dlp" | sha256sum -c - \
    && chmod 0755 /usr/local/bin/yt-dlp \
    && apk del curl \
    && addgroup -g 10001 batek \
    && adduser -S -D -H -u 10001 -G batek -h /app batek

WORKDIR /app
RUN chown batek:batek /app
COPY --from=builder /build/target/release/batek-rs-tg /usr/local/bin/batek-rs-tg
COPY --chown=batek:batek music/ /app/music/
COPY --chown=batek:batek fotos/ /app/fotos/
COPY --chown=batek:batek video/ /app/video/

ENV BATEK_ROOT=/app
USER batek
ENTRYPOINT ["batek-rs-tg"]
