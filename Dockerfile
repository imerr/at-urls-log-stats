FROM rust:slim AS build
WORKDIR /build
COPY . .
RUN cargo build --release

FROM debian:stable-slim
COPY --from=build /build/target/release/at-urls-log-stats /usr/bin
CMD ["at-urls-log-stats"]