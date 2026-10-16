# syntax=docker/dockerfile:1
# Cross-compiles a static musl binary on the build platform (no QEMU needed for arm64).
FROM --platform=$BUILDPLATFORM ghcr.io/rust-cross/cargo-zigbuild:0.23.4 AS build
ARG TARGETARCH
WORKDIR /src
RUN case "$TARGETARCH" in \
      amd64) echo x86_64-unknown-linux-musl ;; \
      arm64) echo aarch64-unknown-linux-musl ;; \
      *) echo "unsupported arch $TARGETARCH" >&2; exit 1 ;; \
    esac > /rust-target && rustup target add "$(cat /rust-target)"
COPY Cargo.toml Cargo.lock ./
COPY .cargo .cargo
COPY src src
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,id=s4cp-target-$TARGETARCH \
    cargo zigbuild --release --locked --target "$(cat /rust-target)" \
    && cp "target/$(cat /rust-target)/release/s4cp" /s4cp \
    && mkdir -p /out/var/lib/s4cp

FROM gcr.io/distroless/static:nonroot
COPY --from=build /s4cp /usr/local/bin/s4cp
# state_dir (prefilter state + catalog cache): mount a volume here to keep it across restarts.
COPY --from=build --chown=65532:65532 /out/var/lib/s4cp /var/lib/s4cp
ENTRYPOINT ["/usr/local/bin/s4cp"]
CMD ["watch"]
