FROM rust:1.53-slim-buster as builder
RUN apt-get update && apt-get -qy install pkg-config libssl-dev && apt-get clean
WORKDIR /workdir
COPY . .
RUN cargo build --release


FROM gcr.io/distroless/cc
COPY --from=builder /workdir/target/release/eip-operator /
ENTRYPOINT ["./eip-operator"]