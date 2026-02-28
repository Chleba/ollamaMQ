# Build stage
FROM rust:alpine AS builder

# Add dependencies for building
RUN apk add --no-cache musl-dev llvm-dev clang pkgconfig openssl-dev

WORKDIR /build

# Create dummy project for caching dependencies
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && cargo build --release && rm -rf src

# Copy source code
COPY src ./src

# Build the real binary
# Touch main.rs to ensure it's recompiled
RUN touch src/main.rs && cargo build --release

# Runtime stage
FROM alpine:3.20

WORKDIR /app

# Install ca-certificates and other runtime deps
RUN apk add --no-cache ca-certificates libgcc

# Copy the binary from builder
COPY --from=builder /build/target/release/ollamaMQ /app/ollamaMQ

# Copy entrypoint script
COPY docker-entrypoint.sh /app/docker-entrypoint.sh
RUN chmod +x /app/docker-entrypoint.sh

# Expose the default port
EXPOSE 11435

# Set entrypoint
ENTRYPOINT ["/app/docker-entrypoint.sh"]
