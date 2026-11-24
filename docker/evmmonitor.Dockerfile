# evmmonitor - EVM chain payment monitor
#
# Expects pre-built binary from CI artifacts:
#   target/release/evmmonitor

FROM archlinux:base
# See docker/ethpayserver.Dockerfile for why this is openssl-3, not openssl.
RUN pacman -Sy --noconfirm ca-certificates openssl-3 && pacman -Scc --noconfirm
RUN useradd -r -s /bin/false evmmonitor
COPY --chmod=755 target/release/evmmonitor /usr/local/bin/
USER evmmonitor

ENV RUST_LOG=info

ENTRYPOINT ["evmmonitor"]
