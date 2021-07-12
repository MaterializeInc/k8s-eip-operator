FROM rust:1.53-slim-buster as builder
WORKDIR /workdir
# Cache the dep builds, in case they change later
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && cargo build --release

# Now build the real thing
COPY . .
RUN cargo build --release


FROM gcr.io/distroless/cc
COPY --from=builder /workdir/target/release/eip-operator /
CMD ["./eip-operator"]
