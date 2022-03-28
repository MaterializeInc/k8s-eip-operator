FROM rust:1.56.1-slim-buster as builder
RUN apt-get update \
    && apt-get -qy install pkg-config libssl-dev cmake g++ \
    && apt-get clean
WORKDIR /workdir
COPY . .
RUN cargo build --release --no-default-features

FROM gcr.io/distroless/cc
COPY --from=builder /workdir/target/release/eip-operator /
ENTRYPOINT ["./eip-operator"]
