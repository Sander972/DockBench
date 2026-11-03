# rtk's prebuilt aarch64 binary needs glibc 2.39 and bookworm has 2.36, so it is compiled here against bookworm's glibc; a separate stage keeps the Rust toolchain out of the image. The repo also holds rtk-proxy, so the package name is required.
FROM rust:1-slim-bookworm AS rtk
RUN cargo install --git https://github.com/rtk-ai/rtk --locked rtk

FROM debian:bookworm-slim
ARG TARGETARCH
ENV DEBIAN_FRONTEND=noninteractive
# a failed download piped into sh would otherwise feed it empty input and pass the build without the tool
SHELL ["/bin/bash", "-o", "pipefail", "-c"]

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl git gnupg sudo build-essential python3 python-is-python3 jq openssh-client less procps unzip \
    && curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg -o /usr/share/keyrings/githubcli-archive-keyring.gpg \
    && echo "deb [arch=${TARGETARCH} signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" > /etc/apt/sources.list.d/github-cli.list \
    && apt-get update && apt-get install -y --no-install-recommends gh \
    && rm -rf /var/lib/apt/lists/*

# GitHub's published SSH host keys, so clones over SSH never prompt and never trust-on-first-use
RUN mkdir -p /etc/ssh && curl -fsSL https://api.github.com/meta | jq -r '.ssh_keys[] | "github.com \(.)"' > /etc/ssh/ssh_known_hosts && test -s /etc/ssh/ssh_known_hosts

RUN curl -fsSL https://mise.run | MISE_INSTALL_PATH=/usr/local/bin/mise sh

COPY --from=rtk /usr/local/cargo/bin/rtk /usr/local/bin/rtk

RUN case "$TARGETARCH" in amd64) a=x64 ;; arm64) a=arm64 ;; esac \
    && curl -fsSL "https://update.code.visualstudio.com/latest/cli-linux-${a}/stable" | tar -xz -C /usr/local/bin code

# default bun for every container, pinned so rebuilds are reproducible; a repo's .mise.toml bun wins wherever the mise shims come first in PATH (image ENV, login shells).
# Kept after the unpinned rtk/code layers so a Bun bump does not refetch them.
ARG BUN_VERSION=1.4.3
RUN curl -fsSL https://bun.sh/install | BUN_INSTALL=/usr/local bash -s "bun-v${BUN_VERSION}" && bun --version

RUN useradd -m -u 1000 -s /bin/bash dev && echo "dev ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/dev \
    && install -d -o dev -g dev /var/lib/dockbench \
    && echo 'export PATH="$HOME/.local/bin:$HOME/.local/share/mise/shims:$PATH"' > /etc/profile.d/dockbench.sh

COPY entrypoint.sh status_server.py /opt/dockbench/
COPY vscode/ /opt/dockbench/vscode/
RUN chmod +x /opt/dockbench/entrypoint.sh

USER dev
WORKDIR /home/dev
# DISABLE_AUTOUPDATER: the entrypoint runs claude update on every start, so Claude Code never swaps its binary in the middle of a session; a manual claude update still works
ENV PATH=/home/dev/.local/bin:/home/dev/.local/share/mise/shims:$PATH \
    DISABLE_AUTOUPDATER=1 \
    MISE_YES=1 \
    MISE_TRUSTED_CONFIG_PATHS=/home/dev/workspace

# GHCR links the package to the repo in `source` and shows `description` on the package page; last, so a change here keeps the layer cache.
LABEL org.opencontainers.image.source=https://github.com/Sander972/DockBench \
      org.opencontainers.image.description="Dev container image: VS Code tunnel, Claude Code and mise, one container per project" \
      org.opencontainers.image.licenses=MIT

ENTRYPOINT ["/opt/dockbench/entrypoint.sh"]
