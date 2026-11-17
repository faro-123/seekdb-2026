FROM rust:1.99.1-slim-bookworm@sha256:392f221964669402bf4ae133d2a46c3ae270f64734133f4b2703d40019235c1a AS build
WORKDIR /build
COPY --link Cargo.toml Cargo.lock ./
COPY --link src ./src
COPY --link vendor/cargo-deps.tar.gz /tmp/cargo-deps.tar.gz
RUN --network=none mkdir vendor && tar -xzf /tmp/cargo-deps.tar.gz -C vendor && \
    cargo --config 'source.crates-io.replace-with="vendored-sources"' \
          --config 'source.vendored-sources.directory="/build/vendor"' \
          build --release --locked --offline && mkdir /runtime-data

FROM scratch AS runtime
COPY --from=build /lib/x86_64-linux-gnu/libgcc_s.so.1 /lib/x86_64-linux-gnu/libgcc_s.so.1
COPY --from=build /lib/x86_64-linux-gnu/libc.so.6 /lib/x86_64-linux-gnu/libc.so.6
COPY --from=build /lib/x86_64-linux-gnu/libm.so.6 /lib/x86_64-linux-gnu/libm.so.6
COPY --from=build /lib64/ld-linux-x86-64.so.2 /lib64/ld-linux-x86-64.so.2
COPY --from=build /build/target/release/minivectordb /minivectordb
COPY --from=build /runtime-data /data
# The driver can bind a private, root-owned case directory at /data.
USER 0:0
WORKDIR /data
ENTRYPOINT ["/minivectordb"]
