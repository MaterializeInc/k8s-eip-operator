FROM --platform=$BUILDPLATFORM rust:1.74.0-slim-bookworm AS chef
RUN cargo install --locked cargo-chef
ARG TARGETARCH
RUN echo -n "$TARGETARCH" | sed 's#amd64#x86_64#;s#arm64#aarch64#' > /cargo_arch
RUN rustup target add x86_64-unknown-linux-gnu
RUN rustup target add aarch64-unknown-linux-gnu
WORKDIR /workdir

FROM --platform=$BUILDPLATFORM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM --platform=$BUILDPLATFORM chef AS builder
RUN apt-get update \
    && apt-get -qy install pkg-config libssl-dev cmake g++ gcc-x86-64-linux-gnu gcc-aarch64-linux-gnu perl \
    && apt-get clean
WORKDIR /workdir
COPY --from=planner /workdir/recipe.json recipe.json
ARG CARGO_RELEASE=--release
ARG CARGO_FEATURES=--no-default-features
RUN export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc; \
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc; \
    cargo chef cook \
    $CARGO_RELEASE \
    $CARGO_FEATURES \
    --recipe-path recipe.json \
    --target "$(cat /cargo_arch)-unknown-linux-gnu"
COPY . .
RUN export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc; \
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc; \
    cargo build $CARGO_RELEASE $CARGO_FEATURES \
    --target "$(cat /cargo_arch)-unknown-linux-gnu"

FROM debian:bookworm-20231030-slim
RUN apt-get update && apt-get install -y iptables ca-certificates && rm -rf /var/ib/apt/lists/*
RUN update-alternatives --set iptables /usr/sbin/iptables-legacy
COPY --from=builder /workdir/target/*/*/eip-operator /
COPY --from=builder /workdir/target/*/*/cilium-eip-no-masquerade-agent /
ENTRYPOINT ["./eip-operator"]
