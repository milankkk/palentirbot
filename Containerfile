# =============================================================================
# Stage 1: Build the Rust Discord Bot
# =============================================================================
FROM rust:bookworm AS builder

WORKDIR /usr/src/palentirbot

# Install required build tools
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy manifests and local workspace crates first to optimize layer caching
COPY Cargo.toml Cargo.lock* ./
COPY command-macros ./command-macros

# Copy source code and database migrations
COPY src ./src
COPY migrations ./migrations

# Build the release binary with cargo cache mounting
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/usr/src/palentirbot/target \
    cargo build --release && \
    cp /usr/src/palentirbot/target/release/palentirbot /usr/src/palentirbot-bin

# =============================================================================
# Stage 2: Runtime Environment (Bot + Danser dependencies + FFmpeg + Xvfb)
# Ubuntu 24.04 provides GLIBC 2.39+ required by Danser 12.1+
# =============================================================================
FROM ubuntu:24.04

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    ffmpeg \
    libgl1 \
    libglib2.0-0 \
    libgtk-3-0 \
    libx11-6 \
    libxcursor1 \
    libxi6 \
    libxinerama1 \
    libxrandr2 \
    libxxf86vm1 \
    mesa-utils \
    libssl3 \
    zlib1g \
    libzstd1 \
    unzip \
    xvfb \
    procps \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy compiled bot binary
COPY --from=builder /usr/src/palentirbot-bin /app/palentirbot

# Pre-create data and log directories
RUN mkdir -p /app/data/Downloads \
             /app/data/Replays \
             /app/data/Skins \
             /app/data/Songs \
             /app/data/settings \
             /app/logs

# Copy startup entrypoint
COPY entrypoint.sh /app/entrypoint.sh
RUN chmod +x /app/entrypoint.sh

# Ensure danser shared libraries inside /app/data are discovered by dynamic linker
ENV LD_LIBRARY_PATH="/app/data:${LD_LIBRARY_PATH}"

# Default paths
ENV DANSER_PATH="/app/data" \
    FOLDERS_PATH="/app/data" \
    AI_SYSTEM_PROMPT="/app/data/prompt.txt"

ENTRYPOINT ["/app/entrypoint.sh"]
