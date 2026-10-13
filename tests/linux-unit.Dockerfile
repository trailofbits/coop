# Keep the tag's Rust version aligned with rust-toolchain.toml. The digest is
# the multi-architecture index for rust:1.94.0-bookworm (see docs/testing.md).
FROM rust:1.94.0-bookworm@sha256:365468470075493dc4583f47387001854321c5a8583ea9604b297e67f01c5a4f

# CI installs cmake and the network tools in .github/workflows/ci.yml.
# tests/test-linux-unit-runner.py checks this list against those CI packages.
RUN apt-get update && apt-get install -y --no-install-recommends \
    cmake musl-tools iproute2 iptables iputils-ping jq rsync util-linux \
    openssh-client openssh-server \
    sudo python3 && rm -rf /var/lib/apt/lists/*

RUN useradd --create-home --uid 1000 --shell /bin/bash coop \
    && printf 'coop ALL=(ALL) NOPASSWD:ALL\n' > /etc/sudoers.d/coop \
    && chmod 0440 /etc/sudoers.d/coop \
    && mkdir -p /workspace/target /usr/local/cargo/registry /usr/local/cargo/git \
    && chown -R coop:coop /workspace /usr/local/cargo
WORKDIR /workspace
ENV HOME=/home/coop
# The base image already contains this exact toolchain. Override rustup's
# component sync from rust-toolchain.toml; cargo test needs only rustc/cargo.
ENV RUSTUP_TOOLCHAIN=1.94.0
# Select the container-safe socket probe fixture only in this test image.
ENV COOP_LINUX_UNIT_TEST_CONTAINER=1
USER coop
