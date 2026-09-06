# BoostSearch, packaged.
#
# Two stages: one that builds the binary and one that carries it. The runtime
# image holds the binary and the two directories a node writes to, and nothing
# else -- no toolchain, no source, no package manager left behind.
#
#   docker build -t boostsearch .
#   docker run -p 9200:9200 -v boostsearch-data:/var/lib/boostsearch boostsearch
#
# The dictionaries for Japanese, Korean and Chinese are built in and are most
# of what the image weighs. A build without them is a fifth of the size and
# answers everything else the same way:
#
#   docker build --build-arg FEATURES=--no-default-features -t boostsearch .

FROM rust:1-bookworm AS build
ARG FEATURES=""
WORKDIR /src
# the manifest first, so that a change to the source does not throw away the
# compiled dependencies
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && echo '' > src/lib.rs \
    && cargo build --release ${FEATURES} 2>/dev/null || true
COPY src ./src
RUN touch src/main.rs src/lib.rs && cargo build --release --bin boostsearch ${FEATURES}

FROM debian:bookworm-slim
# a node runs as itself rather than as root: it needs to read its config and
# write its data, and nothing else on the machine
RUN groupadd --system --gid 1000 boostsearch \
    && useradd --system --uid 1000 --gid boostsearch --home /var/lib/boostsearch boostsearch \
    && apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/boostsearch /usr/local/bin/boostsearch
RUN mkdir -p /var/lib/boostsearch /etc/boostsearch \
    && chown -R boostsearch:boostsearch /var/lib/boostsearch /etc/boostsearch
USER boostsearch
ENV BOOSTSEARCH_ADDR=0.0.0.0:9200 \
    BOOSTSEARCH_DATA=/var/lib/boostsearch \
    BOOSTSEARCH_CONFIG=/etc/boostsearch
VOLUME ["/var/lib/boostsearch"]
EXPOSE 9200 9300
# a container that answers is a container that is up; a container that has
# started the process but cannot answer is not
HEALTHCHECK --interval=10s --timeout=3s --start-period=30s --retries=3 \
    CMD curl -sf http://127.0.0.1:9200/_cluster/health || exit 1
ENTRYPOINT ["/usr/local/bin/boostsearch"]
