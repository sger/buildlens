# BuildLens team server.
#
# Builds the server and the CLI. The CLI's analysis is macOS-oriented — it reads
# .xcactivitylog files and shells out to sysctl — so inside a Linux container it
# is for debugging and for ingesting logs mounted in, not for collection.
#
#   podman build -t buildlens .
#   podman run --rm -e DATABASE_URL=... -e BUILDLENS_TOKEN=... -p 8788:8788 buildlens
#
# Building for a Synology NAS from an Apple Silicon Mac needs the target
# architecture named explicitly, because the NAS is x86_64 and the Mac is not:
#
#   podman build --platform linux/amd64 -t buildlens .
#
# Base images come from ghcr.io rather than Docker Hub, which this project does
# not depend on. Both are Debian 12 and so share a glibc: a binary built against
# the builder's 2.36 will not start on an image carrying an older one, which
# rules out mixing distributions between the two stages.

FROM ghcr.io/rust-lang/rust:1.96-slim-bookworm AS builder

# The postgres crate talks the wire protocol directly, so there is no libpq to
# link. Only a linker is needed beyond what the rust image already carries.
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# Manifests first, then a throwaway build of the dependency graph. This layer is
# cached and only reinvalidated when a manifest changes, so editing source does
# not recompile every dependency. The dummy sources exist because cargo needs a
# crate root per member to resolve the workspace at all.
COPY Cargo.toml Cargo.lock ./
COPY crates/buildlens-core/Cargo.toml crates/buildlens-core/
COPY crates/buildlens-parser/Cargo.toml crates/buildlens-parser/
COPY crates/buildlens-plugins/Cargo.toml crates/buildlens-plugins/
COPY crates/buildlens-metrics/Cargo.toml crates/buildlens-metrics/
COPY crates/buildlens-diagnostics/Cargo.toml crates/buildlens-diagnostics/
COPY crates/buildlens-tests/Cargo.toml crates/buildlens-tests/
COPY crates/buildlens-xcresult/Cargo.toml crates/buildlens-xcresult/
COPY crates/buildlens-graph/Cargo.toml crates/buildlens-graph/
COPY crates/buildlens-intel/Cargo.toml crates/buildlens-intel/
COPY crates/buildlens-report/Cargo.toml crates/buildlens-report/
COPY crates/buildlens-dashboard/Cargo.toml crates/buildlens-dashboard/
COPY crates/buildlens-server/Cargo.toml crates/buildlens-server/
COPY crates/buildlens-storage/Cargo.toml crates/buildlens-storage/
COPY crates/buildlens-git/Cargo.toml crates/buildlens-git/
COPY crates/buildlens-cli/Cargo.toml crates/buildlens-cli/
    # Stubs for every target a manifest declares, not just src/: cargo parses
    # the whole workspace up front and refuses to build if a declared bench,
    # test or binary path is missing.
RUN set -eux; \
    for crate in crates/*/; do \
        mkdir -p "$crate/src"; \
        echo 'fn main() {}' > "$crate/src/main.rs"; \
        echo '' > "$crate/src/lib.rs"; \
    done; \
    mkdir -p crates/buildlens-parser/benches crates/buildlens-metrics/tests \
             crates/buildlens-cli/tests crates/buildlens-server/tests; \
    echo 'fn main() {}' > crates/buildlens-parser/benches/parser.rs; \
    echo '' > crates/buildlens-metrics/tests/metrics.rs; \
    echo '' > crates/buildlens-cli/tests/cli.rs; \
    echo '' > crates/buildlens-server/tests/postgres.rs; \
    cargo build --release -p buildlens-server -p buildlens-cli; \
    # Remove the stub artifacts, or cargo sees fresh timestamps and skips
    # rebuilding them against the real sources copied in next.
    rm -rf crates/*/src crates/*/benches crates/*/tests \
           target/release/deps/buildlens* \
           target/release/buildlens target/release/buildlens-server

COPY crates crates
COPY dashboard-ui dashboard-ui
# `touch` so the real sources are newer than the cached dependency build.
RUN find crates -name '*.rs' -exec touch {} + \
    && cargo build --release -p buildlens-server -p buildlens-cli

FROM ghcr.io/linuxcontainers/debian-slim:latest AS runtime

# ca-certificates is not needed for Postgres (no TLS on the local network) but
# is here for the CLI, which makes outbound HTTPS calls when pushing to a
# server. curl backs the compose healthcheck.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    # An unprivileged user, so a compromise of the server does not land as root
    # on the host through a mounted volume.
    && useradd --system --create-home --uid 10001 buildlens

COPY --from=builder /src/target/release/buildlens-server /usr/local/bin/
COPY --from=builder /src/target/release/buildlens /usr/local/bin/

USER buildlens
WORKDIR /home/buildlens

# Containers reach their published port only if the server listens on all
# interfaces; the binary's own default is loopback, which is right for a local
# run and wrong here.
ENV BUILDLENS_BIND=0.0.0.0:8788
EXPOSE 8788

# No shell form: the server must be PID 1 so a container stop signal reaches it
# instead of a shell that ignores it.
CMD ["buildlens-server"]
