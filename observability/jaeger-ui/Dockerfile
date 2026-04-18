# Build a standalone Jaeger UI that talks to VictoriaTraces' Jaeger-compatible
# query API. VictoriaTraces exposes /select/jaeger/api/... but does not implement
# the gRPC storage-plugin protocol jaeger-query expects, so we can't use the
# official jaeger-query binary. Instead we bundle the Jaeger UI static assets
# with an nginx reverse-proxy that rewrites /api/* → victoria-traces:10428.
#
# Pattern documented at:
#   https://docs.victoriametrics.com/victoriatraces/querying/jaeger-frontend/
ARG JAEGER_UI_VERSION=v2.17.0

FROM node:24-bookworm-slim AS build
ARG JAEGER_UI_VERSION
RUN apt-get update \
 && apt-get install -y --no-install-recommends git python3 make g++ ca-certificates \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
RUN git clone --depth 1 --branch ${JAEGER_UI_VERSION} https://github.com/jaegertracing/jaeger-ui.git .
RUN npm ci
RUN npm run build --workspace=packages/jaeger-ui

FROM nginx:alpine
COPY --from=build /src/packages/jaeger-ui/build /usr/share/nginx/html
COPY default.conf /etc/nginx/conf.d/default.conf
EXPOSE 80
