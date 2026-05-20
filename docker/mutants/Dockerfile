FROM rust:1-bookworm

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        tmux \
        git \
        ca-certificates \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

ARG CARGO_MUTANTS_VERSION=27.0.0
RUN cargo install --locked --version "${CARGO_MUTANTS_VERSION}" cargo-mutants

ARG MUTANTS_UID=1000
ARG MUTANTS_GID=1000
RUN groupadd --gid "${MUTANTS_GID}" mutants \
    && useradd --uid "${MUTANTS_UID}" --gid "${MUTANTS_GID}" --create-home --shell /bin/bash mutants

USER mutants
ENV HOME=/home/mutants \
    CARGO_HOME=/home/mutants/.cargo \
    PATH=/home/mutants/.cargo/bin:/usr/local/cargo/bin:/usr/local/bin:/usr/bin:/bin

WORKDIR /work

ENTRYPOINT ["cargo", "mutants"]
