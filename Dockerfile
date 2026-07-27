# syntax=docker/dockerfile:1

FROM rust:1.97.1-bookworm AS builder

WORKDIR /src
COPY . .
RUN cargo build --release --locked -p crow-agentd

FROM ubuntu:24.04

RUN groupadd --system crow-agent \
    && useradd --system --gid crow-agent --home-dir /var/lib/crow-agent \
        --create-home --shell /usr/sbin/nologin crow-agent \
    && install -d -m 0700 -o crow-agent -g crow-agent /var/lib/crow-agent/soak

COPY --from=builder /src/target/release/crow-agentd /usr/local/bin/crow-agentd
COPY deploy/crow-agentd.service /usr/lib/systemd/system/crow-agentd.service
COPY --chmod=0755 deploy/render-validation-entrypoint.sh /usr/local/bin/render-validation-entrypoint

USER crow-agent
WORKDIR /var/lib/crow-agent

ENTRYPOINT ["/usr/local/bin/render-validation-entrypoint"]
