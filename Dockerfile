FROM rust:1.68-slim AS builder

WORKDIR /app

COPY . .

RUN apt-get update && apt-get install -y libopus0 autoconf libtool build-essential
RUN LIBOPUS_STATIC=1 cargo build --release

#------------

FROM debian:bullseye-slim

COPY --from=builder /app/target/release/disrecord /disrecord

ENTRYPOINT ["/disrecord"]