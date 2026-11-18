# Toolchain for the RPZA port proof of concept: Rust + gcc + nasm.
FROM rust:1-bookworm
RUN apt-get update \
 && apt-get install -y --no-install-recommends nasm rsync curl ca-certificates patch \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /build
