FROM scratch AS toolchain
# Rust 1.99.1 GNU components from the release manifest.
ADD --link --checksum=sha256:2ff05ff9c08a90861e7f6e4065b40a8b87463b4576186fd613871c3e37f2091f https://static.rust-lang.org/dist/2026-10-15/rustc-1.99.1-x86_64-unknown-linux-gnu.tar.xz /rustc.tar.xz
ADD --link --checksum=sha256:109b074225bf5b9ea3820890876bfdf4ed161adbfaf94b2b4b5984c85ee5d958 https://static.rust-lang.org/dist/2026-10-15/rust-std-1.99.1-x86_64-unknown-linux-gnu.tar.xz /rust-std.tar.xz
ADD --link --checksum=sha256:53f3521949404d595efb06a55c12c5658d4ed1c75566f33d3d514eaebe70bd8a https://static.rust-lang.org/dist/2026-10-15/cargo-1.99.1-x86_64-unknown-linux-gnu.tar.xz /cargo.tar.xz
# Bookworm linker inputs and XZ unpacker. Rust ships the LLD linker.
ADD --link --checksum=sha256:b46f33cc2ec245e435e043807038cecf4b201ef004800e9dfc1455240360e49d https://deb.debian.org/debian/pool/main/g/gcc-12/gcc-12_12.2.0-14+deb12u1_amd64.deb /gcc.deb
ADD --link --checksum=sha256:d720259380a84f2ffc6fe516eff5cbe9c8a005138e8d6a5748747ba4b3404a82 https://deb.debian.org/debian/pool/main/g/gcc-12/libgcc-12-dev_12.2.0-14+deb12u1_amd64.deb /libgcc-dev.deb
ADD --link --checksum=sha256:0218fc2befcd784c1b0c6292c0a137ce89fad054efaa579ad083bee0f2c01aae https://deb.debian.org/debian/pool/main/g/glibc/libc6-dev_2.36-9+deb12u14_amd64.deb /libc-dev.deb
ADD --link --checksum=sha256:cc6dc501e0c06be3f89e9a7f8dfd7a97f92aadfd9daa60fad19b90d6a5558b80 https://deb.debian.org/debian/pool/main/x/xz-utils/xz-utils_5.4.1-1+deb12u1_amd64.deb /xz.deb

FROM debian:12.15-slim@sha256:c67e92c8099ba22853ab4cfda7cfb494d253d6012ca82dee98ba8976772af018 AS build
RUN --network=none --mount=type=bind,from=toolchain,target=/packages set -eu; \
    for package in /packages/*.deb; do dpkg-deb -x "$package" /; done; \
    ln -s x86_64-linux-gnu-gcc-12 /usr/bin/cc; \
    mkdir -p /opt/rust /tmp/rust; \
    for package in /packages/*.tar.xz; do tar -xJf "$package" -C /tmp/rust; done; \
    for installer in /tmp/rust/*/install.sh; do \
        sh "$installer" --prefix=/opt/rust --disable-ldconfig; \
    done; \
    rm -rf /tmp/rust
ENV PATH="/opt/rust/bin:$PATH"
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
COPY --from=build /lib64/ld-linux-x86-64.so.2 /lib64/ld-linux-x86-64.so.2
COPY --from=build /build/target/release/minivectordb /minivectordb
COPY --from=build /runtime-data /data
# The driver can bind a private, root-owned case directory at /data.
USER 0:0
WORKDIR /data
ENTRYPOINT ["/minivectordb"]
