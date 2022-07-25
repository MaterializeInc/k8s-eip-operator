FROM rust:1.61.0-slim-bullseye as builder
RUN apt-get update \
    && apt-get -qy install pkg-config libssl-dev cmake g++ \
    && apt-get clean
WORKDIR /workdir
COPY . .
RUN cargo build --release --no-default-features

FROM gcr.io/distroless/cc-debian11
COPY --from=builder /workdir/target/release/eip-operator /
COPY --from=builder /workdir/target/release/cilium-eip-no-masquerade-agent /
ENTRYPOINT ["./eip-operator"]
