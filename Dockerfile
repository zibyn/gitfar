# gitfard OCI image (spec §6.2): the gitfard binary plus a pinned git, nothing else.
#   docker build -t gitfard .
FROM rust:1.99-trixie AS build
# reqwest's default TLS (rustls + aws-lc-rs) needs a C toolchain and cmake.
RUN apt-get update && apt-get install -y --no-install-recommends cmake && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p gitfard

FROM debian:trixie-slim
# gitfard refuses git < 2.33. Bump together with the base image.
ARG GIT_VERSION=1:2.47.3-0+deb13u1
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates git=${GIT_VERSION} \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /var/lib/gitfar gitfar \
 && mkdir -p /var/lib/gitfar /etc/gitfar && chown gitfar /var/lib/gitfar
COPY --from=build /src/target/release/gitfard /usr/local/bin/gitfard
COPY deploy/gitfard.toml /etc/gitfar/gitfard.toml
USER gitfar
VOLUME /var/lib/gitfar
EXPOSE 8080 9090
ENTRYPOINT ["gitfard"]
CMD ["serve", "--config", "/etc/gitfar/gitfard.toml"]
