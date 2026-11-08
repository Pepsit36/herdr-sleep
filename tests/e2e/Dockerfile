# End-to-end image: a real Herdr binary, the plugin built from this checkout and a fake
# Claude Code. Build from the repository root:
#
#   docker build -f tests/e2e/Dockerfile --build-arg HERDR_VERSION=0.9.2 -t herdr-sleep-e2e .
#   docker run --rm herdr-sleep-e2e

# ---- build the plugin and the fake agent -----------------------------------------------
FROM rust:1.98-slim-bookworm AS build
WORKDIR /src
# herdr-plugin.tom[l] keeps the copy working when the manifest is missing; Cargo.toml always matches.
COPY Cargo.toml Cargo.lock herdr-plugin.tom[l] ./
COPY crates ./crates
COPY tests/e2e/fake-agent ./tests/e2e/fake-agent
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    --mount=type=cache,target=/src/tests/e2e/fake-agent/target \
    mkdir -p /out \
 && cargo build --release --locked -p herdr-sleep -p herdr-sleep-ph \
 && cp target/release/herdr-sleep target/release/herdr-sleep-ph /out/ \
 && cargo build --release --locked --manifest-path tests/e2e/fake-agent/Cargo.toml \
 && cp tests/e2e/fake-agent/target/release/claude /out/claude
# The suite links the plugin without Herdr's build step: the prebuilt binaries are in bin/.
RUN test -f herdr-plugin.toml || { echo "herdr-plugin.toml is missing from the build context" >&2; exit 1; } \
 && awk '/^\[\[build\]\]/ {skip=1; next} /^\[/ {skip=0} !skip' herdr-plugin.toml > /out/herdr-plugin.toml \
 && ! grep -q '^\[\[build\]\]' /out/herdr-plugin.toml

# ---- the test image ---------------------------------------------------------------------
FROM debian:bookworm-slim
ARG HERDR_VERSION=latest
# Changes on every build of "latest" (the Makefile sets it), so that the download is not cached.
ARG HERDR_FETCHED_AT=
RUN apt-get update \
 && apt-get install -y --no-install-recommends procps bash jq curl ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# Resolves "latest" through herdr.dev, picks the asset for the build platform and checks it runs.
RUN set -eu; \
    : "${HERDR_FETCHED_AT}"; \
    case "$(uname -m)" in x86_64) arch=x86_64 ;; aarch64|arm64) arch=aarch64 ;; *) echo "unsupported arch" >&2; exit 1 ;; esac; \
    version="${HERDR_VERSION}"; \
    if [ "$version" = latest ]; then version="$(curl -fsSL https://herdr.dev/latest.json | jq -r .version)"; fi; \
    curl -fsSL -o /usr/local/bin/herdr "https://github.com/herdrdev/herdr/releases/download/v${version}/herdr-linux-${arch}"; \
    chmod +x /usr/local/bin/herdr; \
    echo "$version" > /etc/herdr-version; \
    herdr --version

COPY --from=build /out/claude /usr/local/bin/claude
COPY --from=build /out/herdr-sleep /out/herdr-sleep-ph /opt/herdr-sleep/bin/
COPY --from=build /out/herdr-plugin.toml /opt/herdr-sleep/herdr-plugin.toml
# Herdr relaunches the Placeholder after a restart by its bare command name, which must be on PATH.
RUN ln -s /opt/herdr-sleep/bin/herdr-sleep-ph /usr/local/bin/herdr-sleep-ph
COPY tests/e2e/run.sh /e2e/run.sh

RUN useradd --create-home --shell /bin/bash tester
USER tester
ENV SHELL=/bin/bash TERM=xterm-256color LANG=C.UTF-8
WORKDIR /home/tester
CMD ["bash", "/e2e/run.sh"]
