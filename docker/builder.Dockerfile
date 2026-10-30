# Stepwise builder: compiles TypeScript / JavaScript / Python hook source into components.
# Toolchain versions are the ones the test-suite was run against.
#
#   docker build -f docker/builder.Dockerfile -t stepwise-builder .
#
# NOTE: written alongside the code but not built in CI here (no Docker in the dev sandbox).
FROM rust:1.91-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY builder ./builder
COPY ui ./ui
COPY store ./store
# the root manifest is a workspace; build only the (small) builder crate
RUN cargo build --release -p stepwise-builder

FROM node:22-bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends python3 python3-venv ca-certificates \
 && rm -rf /var/lib/apt/lists/*
RUN npm install -g --no-audit --no-fund @bytecodealliance/jco@1.37.0 @bytecodealliance/componentize-js@0.23.0 \
 && python3 -m venv /opt/py \
 && /opt/py/bin/pip install --no-cache-dir componentize-py==0.25.1

COPY --from=build /src/target/release/stepwise-builder /usr/local/bin/stepwise-builder
COPY wit /opt/stepwise/wit

ENV BUILDER_WIT_DIR=/opt/stepwise/wit \
    BUILDER_JCO=jco \
    BUILDER_COMPONENTIZE_PY=/opt/py/bin/componentize-py \
    BUILDER_LISTEN=0.0.0.0:7070
# BUILDER_TOKEN must be supplied at run time (>= 16 characters).

USER node
EXPOSE 7070
ENTRYPOINT ["stepwise-builder"]
