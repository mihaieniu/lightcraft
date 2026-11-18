# LightCraft worker (fork addition, CRAFT-9): builds smart previews of Immich photos for LightCraft on other computers.
# Build from the repository root:  docker compose -f docker/worker/compose.yml up -d --build
FROM rust:1.95-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p lightcraft-cli --no-default-features \
 && cp target/release/lightcraft-cli /lightcraft-cli

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --create-home lightcraft
COPY --from=build /lightcraft-cli /usr/local/bin/lightcraft-cli
USER lightcraft
EXPOSE 7991
ENTRYPOINT ["lightcraft-cli", "preview-serve"]
CMD ["--listen", "0.0.0.0:7991", "--tmp", "/dev/shm/lightcraft"]
