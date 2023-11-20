FROM rust:1.74.0-slim-bookworm AS chef
RUN cargo install --locked cargo-chef
WORKDIR /workdir

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
RUN apt-get update \
    && apt-get -qy install pkg-config libssl-dev cmake g++ \
    && apt-get clean
WORKDIR /workdir
COPY --from=planner /workdir/recipe.json recipe.json
ARG CARGO_RELEASE=--release
ARG CARGO_FEATURES=--no-default-features
RUN cargo chef cook $CARGO_RELEASE $CARGO_FEATURES --recipe-path recipe.json
COPY . .
RUN cargo build $CARGO_RELEASE $CARGO_FEATURES

FROM debian:bookworm-20231030-slim
RUN apt-get update && apt-get install -y iptables ca-certificates && rm -rf /var/ib/apt/lists/*
RUN update-alternatives --set iptables /usr/sbin/iptables-legacy
COPY --from=builder /workdir/target/*/eip-operator /
COPY --from=builder /workdir/target/*/cilium-eip-no-masquerade-agent /
ENTRYPOINT ["./eip-operator"]
