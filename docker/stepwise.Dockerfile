# Stepwise server (data plane + control plane).
#
#   docker build -f docker/stepwise.Dockerfile -t stepwise .
#
# NOTE: written alongside the code but not built in CI here (no Docker in the dev sandbox).
FROM rust:1.91-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY wit ./wit
COPY openapi ./openapi
COPY builder ./builder
COPY ui ./ui
COPY store ./store
RUN cargo build --release -p stepwise

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 --home /var/lib/stepwise --create-home stepwise \
 && mkdir -p /etc/stepwise && chown stepwise /etc/stepwise
COPY --from=build /src/target/release/stepwise /usr/local/bin/stepwise
USER stepwise
WORKDIR /var/lib/stepwise
EXPOSE 8080 9090
ENTRYPOINT ["stepwise"]
CMD ["run", "/etc/stepwise/config.yaml"]
