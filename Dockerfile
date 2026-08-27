# Build the benchmark image the OpenSearch comparison runs against.
#
# Both engines have to be measured under the same runtime, so this exists to
# put obsearch in a container next to the OpenSearch one rather than comparing
# a native binary against a containerised JVM.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --bin obsearch

FROM debian:bookworm-slim
COPY --from=build /src/target/release/obsearch /usr/local/bin/obsearch
EXPOSE 9200
CMD ["/usr/local/bin/obsearch"]
