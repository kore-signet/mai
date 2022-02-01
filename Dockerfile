FROM rust:1.58-slim-buster as builder

WORKDIR /usr/src/mai
COPY . .
RUN cargo build --release

FROM debian:buster-slim
RUN apt-get update && apt-get install -y curl 
COPY --from=builder /usr/src/mai/target/release/mai .

CMD ["./mai"]