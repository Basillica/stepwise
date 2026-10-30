# Stepwise management UI (static app + authenticating proxy to the control plane).
#
#   docker build -f docker/ui.Dockerfile -t stepwise-ui .
#
# NOTE: written alongside the code but not built in CI here (no Docker in the dev sandbox).
FROM rust:1.91-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY wit ./wit
COPY openapi ./openapi
COPY builder ./builder
COPY store ./store
COPY ui ./ui
RUN cargo build --release -p stepwise-ui

FROM debian:bookworm-slim
RUN useradd --system --uid 10002 stepwise-ui
COPY --from=build /src/target/release/stepwise-ui /usr/local/bin/stepwise-ui
USER stepwise-ui
ENV UI_LISTEN=0.0.0.0:8081
EXPOSE 8081
ENTRYPOINT ["stepwise-ui"]
