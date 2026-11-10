FROM debian:trixie-slim

# Package versions follow the security updates in the selected Debian release.
# hadolint ignore=DL3008
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /etc/ganja /var/lib/ganja /work

COPY --chmod=0755 ganja /usr/local/bin/ganja
COPY ganja.toml gitconfig acceptance-script.json /etc/ganja/

# The host is public routing information, not a credential. Scope the placeholder
# header so the platform can replace it only on requests to the intended host.
ARG GIT_HOST
RUN case "$GIT_HOST" in ""|*[!a-zA-Z0-9.-]*|.*|-*) exit 1 ;; esac \
    && git config --file /etc/ganja/gitconfig \
        "http.https://${GIT_HOST}/.extraHeader" "Authorization: placeholder"

# These are paths, switches, and a public username; the password is runtime-only.
# hadolint ignore=DL3064
ENV GANJA_CONFIG_HOME=/etc/ganja \
    XDG_DATA_HOME=/var/lib/ganja \
    SSL_CERT_FILE=/run/ate/egress-trust-bundle/trust-bundle.pem \
    GIT_CONFIG_GLOBAL=/etc/ganja/gitconfig \
    GIT_SSL_CAINFO=/run/ate/egress-trust-bundle/trust-bundle.pem \
    GANJA_PROVIDER=fake \
    GANJA_FAKE_SCRIPT=/etc/ganja/acceptance-script.json \
    GANJA_DISABLE_MODELS_FETCH=1 \
    GANJA_DISABLE_TERM_PROBE=1 \
    GANJA_SERVER_USERNAME=ganja

WORKDIR /work
EXPOSE 80
ENTRYPOINT ["ganja", "worker", "init", "--port", "80"]
