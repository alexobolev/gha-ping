FROM rust:1.84.1-alpine3.21 AS build
WORKDIR /usr/src/gha_ping

RUN apk add --update pkgconfig openssl-dev libc-dev

COPY Cargo.toml .
COPY ./gha_ping ./gha_ping
RUN cargo install --path ./gha_ping


FROM alpine:3.21
WORKDIR /var/gha_ping
ARG git_repo_url

RUN apk add --update openssl openssh git ca-certificates
RUN mkdir -p /root/.ssh && \
    chmod 0700 /root/.ssh && \
    ssh-keyscan github.com > /root/.ssh/known_hosts

RUN echo "cp ./key ./key.local && chmod 0700 ./key.local && gha_ping" > ./runtime.sh && \
    chmod 0700 ./runtime.sh

COPY --from=build /usr/local/cargo/bin/gha_ping /usr/local/bin/gha_ping
ENV GHAP_TCP_HOST="0.0.0.0" \
    GHAP_TCP_PORT="4331" \
    GHAP_SSH_REPO_URL=$git_repo_url \
    GHAP_SSH_KEY_PATH="./key.local" \
    GHAP_OUT_REPO_PATH="./repo"
EXPOSE 4331/tcp
ENTRYPOINT ["sh", "-c", "./runtime.sh"]
