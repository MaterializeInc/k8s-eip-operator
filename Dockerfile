FROM rust:1.61.0-slim-bullseye AS chef
RUN cargo install cargo-chef
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
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release --no-default-features

FROM gcr.io/distroless/cc-debian11
COPY --from=builder /workdir/target/release/eip-operator /
COPY --from=builder /workdir/target/release/cilium-eip-no-masquerade-agent /
ENTRYPOINT ["./eip-operator"]
