# Cargo dependencies

The cargo-deps.tar.gz archive contains the crates.io source packages in Cargo.lock, including their licenses and .cargo-checksum.json files. Docker builds the source offline. The final image contains the application and runtime libraries.

After updating Cargo.lock, run “cargo vendor --locked /path/to/vendor”. Pack the crate directories at the archive root and keep their checksum files. Run a locked offline Docker build to verify the archive.
