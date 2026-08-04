FROM rust:1-bookworm AS build
WORKDIR /work
COPY . .
RUN cargo build --locked --release || cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN useradd --create-home --uid 10001 app
COPY --from=build /work/target/release/apme-web-mash /usr/local/bin/apme-web-mash
USER app
ENV BIND_ADDR=0.0.0.0:8080
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/apme-web-mash"]
