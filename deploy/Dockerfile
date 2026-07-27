# HBIT: the Hacash full node and the payout pool, built once and run as two
# services on the same box.
#
# No GPU here on purpose. This image is for a VPS or a mini PC whose job is to
# hold the chain and pay miners; the mining happens on other people's hardware.
# Leaving OpenCL and CUDA out keeps the image small and removes a whole class of
# driver problem from a machine that must simply stay up.

# ---------------------------------------------------------------- build stage
FROM rust:1-bookworm AS build

# Everything the default (non-GPU) feature set needs to link.
RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config libssl-dev clang \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .

# Cap the compiler parallelism, because the default is one job per core and each
# rustc on this tree can hold well over a gigabyte.
#
# On the machines this image is FOR, a VPS or a mini PC with 2 to 8 GB, an
# unbounded build is not slow, it is fatal: the kernel kills rustc and the build
# fails with a message about a signal that says nothing about memory. It bites on
# large machines too. Building this on a 32 core host killed the Docker engine
# outright, mid-compile, leaving only an RPC EOF behind.
#
# 4 keeps the peak near 6 to 8 GB. Raise it when the builder has the memory for
# it, roughly 2 GB per job:
#   docker build --build-arg CARGO_JOBS=16 -f deploy/Dockerfile -t hbit:local .
ARG CARGO_JOBS=4
ENV CARGO_BUILD_JOBS=${CARGO_JOBS}

# --locked so the image is built from the committed dependency set and a
# release cannot silently pick up a different one.
RUN cargo build --locked --release --bin fullnode \
 && cargo build --locked --release -p hbit-pool --bin hbit-pool-server --bin hbit-pool-payout

# -------------------------------------------------------------- runtime stage
FROM debian:bookworm-slim AS runtime

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*

# A dedicated unprivileged user. The wallet and the chain belong to it, not to
# root, so a flaw in either program is not a flaw with root's reach.
RUN useradd --system --create-home --home-dir /var/lib/hbit --shell /usr/sbin/nologin hbit

COPY --from=build /src/target/release/fullnode          /usr/local/bin/fullnode
COPY --from=build /src/target/release/hbit-pool-server  /usr/local/bin/hbit-pool-server
COPY --from=build /src/target/release/hbit-pool-payout  /usr/local/bin/hbit-pool-payout
COPY --from=build /src/docs/POOL-OPERATOR.md            /usr/share/doc/hbit/POOL-OPERATOR.md
COPY --from=build /src/docs/POOL-README.md              /usr/share/doc/hbit/POOL-README.md

USER hbit
WORKDIR /var/lib/hbit
