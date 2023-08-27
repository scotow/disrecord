FROM rust:1.72-slim AS builder

WORKDIR /app

COPY . .

RUN apt-get update && apt-get install -y libopus0 autoconf libtool build-essential cmake
RUN LIBOPUS_STATIC=1 cargo build --release

#------------

FROM debian:bookworm-slim

COPY --from=mwader/static-ffmpeg:6.0 /ffmpeg /usr/local/bin/
COPY --from=builder /app/target/release/disrecord /disrecord

ENTRYPOINT ["/disrecord"]