# Builds the compux helper for the M86 L0 spike on Ubuntu 22.04, so the one binary
# runs in both fixture images (22.04's glibc is the older of the two). Nothing here
# runs the helper. It needs no C library but the C runtime to link against (M86 L2).
FROM ubuntu:22.04
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      ca-certificates curl gcc libc6-dev \
 && rm -rf /var/lib/apt/lists/* \
 && curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
ENV PATH=/root/.cargo/bin:$PATH
