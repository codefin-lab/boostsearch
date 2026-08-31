# Build the benchmark image the OpenSearch comparison runs against.
#
# Both engines have to be measured under the same runtime, so this exists to
# put boostsearch in a container next to the OpenSearch one rather than comparing
# a native binary against a containerised JVM.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --bin boostsearch

FROM debian:bookworm-slim
COPY --from=build /src/target/release/boostsearch /usr/local/bin/boostsearch
EXPOSE 9200
CMD ["/usr/local/bin/boostsearch"]
