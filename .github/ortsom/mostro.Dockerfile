# syntax=docker/dockerfile:1
#
# mostrod for the Ortsom regtest stack, built from the build context: the
# baseline builds `main`, a pull request run builds the pull request
# (docs/ORTSOM_PR_E2E_SPEC.md § 5, § 6.1). Both are then started with
# `ortsom stack up --mostro-image`, so every comparison is between images
# from this one recipe.
#
# Mirrors Ortsom's ci/regtest/mostro.Dockerfile (the recipe
# `ortsom stack up --ref` uses) with one difference: it compiles the build
# context instead of fetching a commit from GitHub. Keep the rest in step
# with Ortsom's copy when `ortsom_ref` is bumped.
#
#   docker build -f .github/ortsom/mostro.Dockerfile \
#     --build-arg MOSTRO_SHA=$(git rev-parse HEAD) -t ortsom-local/mostro .
#
# mostro.Dockerfile.dockerignore keeps target/ and local files out of the
# context. .git stays in: build.rs embeds `git rev-parse HEAD`.
#
# Builder and runtime are the same Debian release on purpose: a binary
# linked against a newer glibc than the runtime image ships refuses to
# start.

FROM debian:bookworm-slim AS builder

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates curl git build-essential cmake pkg-config \
      libssl-dev libsqlite3-dev protobuf-compiler \
 && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/rustup \
    CARGO_HOME=/cargo \
    PATH=/cargo/bin:$PATH

# No toolchain yet: the source decides which one.
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain none --no-modify-path

WORKDIR /src
COPY . .

# `sharing=locked` on the target directory: two builds writing one
# target at once corrupt it, and waiting is cheaper than rebuilding.
#
# The cache covers all of RUSTUP_HOME, not just its toolchains: rustup
# unpacks into RUSTUP_HOME/tmp and renames into place, and a rename
# across two mounts fails with EXDEV.
RUN --mount=type=cache,id=ortsom-mostro-rustup,target=/rustup,sharing=locked \
    --mount=type=cache,id=ortsom-mostro-registry,target=/cargo/registry \
    --mount=type=cache,id=ortsom-mostro-git,target=/cargo/git \
    --mount=type=cache,id=ortsom-mostro-target,target=/src/target,sharing=locked \
    if [ -f rust-toolchain.toml ] || [ -f rust-toolchain ]; then \
      rustup toolchain install; \
    else \
      rustup default stable; \
    fi \
 && cargo build --release --locked --bin mostrod \
 && cp target/release/mostrod /usr/local/bin/mostrod

FROM debian:bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/bin/mostrod /usr/local/bin/mostrod

ARG MOSTRO_REPO=https://github.com/MostroP2P/mostro.git
ARG MOSTRO_SHA
# What `ortsom stack status` reads to say which commit is running.
LABEL org.opencontainers.image.source="$MOSTRO_REPO" \
      org.opencontainers.image.revision="$MOSTRO_SHA"

# compose runs this as the invoking user's uid, which has no home and no
# entry in /etc/passwd; /config is the only place mostrod writes.
ENV HOME=/tmp
CMD ["mostrod", "-d", "/config"]
