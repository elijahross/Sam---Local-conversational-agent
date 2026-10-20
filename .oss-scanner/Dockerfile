# Offline audit image for Anthropic's OSS Scanner (https://github.com/anthropics/oss-scanner).
# Sam is a macOS app: the Rust agent server (server/) builds and its unit tests run on Linux;
# the Swift app (app/, sidecar/applestt, sidecar/computer, sidecar/workspace) and the Python
# sidecars (MLX / Apple frameworks) only build on macOS 26 and are in the image as source.
FROM rust:1.92-bookworm
ENV DEBIAN_FRONTEND=noninteractive \
    CC=clang \
    CXX=clang++
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates clang pkg-config git python3 python3-venv sqlite3 \
    && rm -rf /var/lib/apt/lists/*

# cargo / rustc also in login shells (bash -l resets PATH to the system default).
RUN echo 'export PATH=/usr/local/cargo/bin:$PATH' > /etc/profile.d/cargo.sh

# scanner contract: the checkout lives inside the image, at /src
COPY . /src
WORKDIR /src/server

# The server, its CLIs (scrape, think, browse, agent, code) and the test harness, with debug info
# and frame pointers. All crates are fetched here (the scan itself runs offline).
ENV CARGO_PROFILE_RELEASE_DEBUG=true \
    RUSTFLAGS="-C force-frame-pointers=yes"
RUN cargo fetch \
    && cargo build --release --bins \
    && cargo test --release --no-run

# Cheap tests to confirm the image works; a failure must not abort the build.
RUN cargo test --release --lib -- --test-threads=4 || echo "WARNING: cargo test reported failures"
