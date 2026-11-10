# syntax=docker/dockerfile:1@sha256:4edf897a3ffa55b89f906fc8cc78afdb3f1834cc9c7083565e611a8a7d5fe99e
FROM rust:1.99.1-slim-bookworm@sha256:392f221964669402bf4ae133d2a46c3ae270f64734133f4b2703d40019235c1a AS build
WORKDIR /build
COPY --link Cargo.toml Cargo.lock ./
COPY --link src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo fetch --locked && cargo build --release --locked --offline && mkdir /runtime-data

FROM scratch AS runtime
COPY --from=build /lib/x86_64-linux-gnu/libgcc_s.so.1 /lib/x86_64-linux-gnu/libgcc_s.so.1
COPY --from=build /lib/x86_64-linux-gnu/libc.so.6 /lib/x86_64-linux-gnu/libc.so.6
COPY --from=build /lib/x86_64-linux-gnu/libm.so.6 /lib/x86_64-linux-gnu/libm.so.6
COPY --from=build /lib64/ld-linux-x86-64.so.2 /lib64/ld-linux-x86-64.so.2
COPY --from=build --chown=10001:10001 /build/target/release/minivectordb /minivectordb
COPY --from=build --chown=10001:10001 /runtime-data /data
USER 10001:10001
WORKDIR /data
ENTRYPOINT ["/minivectordb"]
