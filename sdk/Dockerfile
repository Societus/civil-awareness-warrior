# lh-contributor-client — single static musl binary + scratch runtime.
#
# Build:  podman build -t lh-contributor:latest .
# Run:    podman run --rm -e LH_COORDINATOR_URL=... -e LH_PUBLIC_KEY_HEX=... \
#             -v $PWD/lh-work:/work lh-contributor:latest run
#
# The official rust:alpine image targets musl natively and ships a C
# toolchain, so zstd-sys's C build needs nothing extra beyond musl-dev.

# Build-stage base image pinned by digest so rebuilds are byte-identical and
# the image is not silently retargeted to a newer rust/alpine patch. Re-pin
# this digest on every toolchain bump (rust:1-alpine = sha256:2cccd8b212b37dad15a555589c1b6787da4ab913fe24d8a9b2d99d3498869409 at last audit).
FROM docker.io/library/rust@sha256:2cccd8b212b37dad15a555589c1b6787da4ab913fe24d8a9b2d99d3498869409 AS build
WORKDIR /src
RUN apk add --no-cache musl-dev
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# Cache-friendly: build deps first, then the crate itself.
RUN mkdir -p src/bin && echo 'fn main() {}' > src/main.rs && cargo build --release --locked && rm -rf target/release/deps/lh_contributor*
COPY . .
RUN touch src/main.rs && cargo build --release --locked && strip target/release/lh-contributor

FROM scratch
COPY --from=build /src/target/release/lh-contributor /usr/local/bin/lh-contributor
# reqwest uses rustls with bundled webpki roots — no CA bundle needed.
ENV LH_WORK_DIR=/work LH_CONCURRENCY=2 RUST_LOG=info
USER 65532:65532
WORKDIR /work
VOLUME /work
ENTRYPOINT ["/usr/local/bin/lh-contributor"]
