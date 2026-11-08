# The restart helper for the Unraid host (ghcr.io/snds/app-updates-host-helper).
# Built from the repository root: docker build -f host-helper/Dockerfile .
#
# The binary is static (musl), so the run stage is Docker's own CLI image,
# which carries `docker` and its compose plugin and nothing else of ours.

FROM rust:1.90-alpine@sha256:b4b54b176a74db7e5c68fdfe6029be39a02ccbcfe72b6e5a3e18e2c61b57ae26 AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
# The image pins its own toolchain; the repository's stable channel file would download another.
RUN rm -f rust-toolchain.toml \
    && cargo build --release --locked -p snds-app-updates-host-helper \
    && cp target/release/app-updates-host-helper /app-updates-host-helper

FROM docker:28.5-cli@sha256:625d9431a9f54c5a2bc90f24f0e1c3d55b1349fd857dd85035f98c2c9acbdd4d
ARG VERSION=0.1.0
LABEL org.opencontainers.image.source="https://github.com/snds/app-updates" \
      org.opencontainers.image.description="The restart helper for a Docker host: one-click Update and Reload for snds servers, limited to an allow-list" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.version="${VERSION}"
COPY --from=build /app-updates-host-helper /usr/local/bin/app-updates-host-helper
# The docker CLI keeps its settings here; /tmp is a tmpfs in the sample compose file.
ENV DOCKER_CONFIG=/tmp/docker
ENTRYPOINT ["/usr/local/bin/app-updates-host-helper"]
CMD ["serve", "--config", "/config/allow-list.toml"]
