FROM docker-oss.nexus.famedly.de/rust-container:nightly as builder
ARG CARGO_NET_GIT_FETCH_WITH_CLI=true
ARG FAMEDLY_CRATES_REGISTRY
ARG CARGO_HOME
ARG CARGO_REGISTRIES_FAMEDLY_INDEX
ARG GIT_CRATE_INDEX_USER
ARG GIT_CRATE_INDEX_PASS
ARG RUSTC_WRAPPER
ARG CARGO_BUILD_RUSTFLAGS
ARG CI_SSH_PRIVATE_KEY

# Add CI key for git dependencies in Cargo.toml. This is only done in the builder stage, so the key
# is not available in the final container.
RUN mkdir -p ~/.ssh
RUN echo "${CI_SSH_PRIVATE_KEY}" > ~/.ssh/id_ed25519
RUN chmod 600 ~/.ssh/id_ed25519
RUN echo "Host *\n\tStrictHostKeyChecking no\n\n" > ~/.ssh/config

COPY . /app
WORKDIR /app
RUN cargo auditable build --release

FROM debian:bookworm-slim
RUN apt update && apt install ca-certificates curl -y
RUN mkdir -p /opt/famedly-sync
WORKDIR /opt/famedly-sync
COPY --from=builder /app/target/release/famedly-sync /usr/local/bin/famedly-sync
COPY --from=builder /app/target/release/migrate /usr/local/bin/migrate
ENV FAMEDLY_SYNC_CONFIG="/opt/famedly-sync/config.yaml"
ENTRYPOINT ["/usr/local/bin/famedly-sync"]
