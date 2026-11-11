# The owner supplies the base reference at build time, already resolved to a
# digest, so the build cannot silently float on a tag.
# hadolint ignore=DL3006
ARG BASE_IMAGE
FROM ${BASE_IMAGE}

# Refuse anything but `<name>@sha256:<64 hex>`: the digest is the record of
# what was reviewed, and a tag would make that record unverifiable later.
ARG BASE_IMAGE
RUN digest="${BASE_IMAGE##*@sha256:}" \
    && test "$digest" != "$BASE_IMAGE" \
    && test "${#digest}" -eq 64 \
    && case "$digest" in ""|*[!a-fA-F0-9]*) exit 1 ;; esac

# Package versions follow the security updates in the selected Debian release.
# hadolint ignore=DL3008
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /etc/ganja /var/lib/ganja /work /home/ganja

COPY --chown=0:0 --chmod=0555 ganja /usr/local/bin/ganja
COPY --chown=0:0 --chmod=0444 ganja.toml gitconfig acceptance-script.json /etc/ganja/

# The copied Linux binary must be the one the owner built and reviewed; its
# hash is required at build time and checked against the copied bytes. The
# pipe needs no pipefail: printf of a fixed format cannot fail, and the
# verdict is sha256sum's own exit status.
ARG GANJA_SHA256
# hadolint ignore=DL4006
RUN test "${#GANJA_SHA256}" -eq 64 \
    && case "$GANJA_SHA256" in ""|*[!a-fA-F0-9]*) exit 1 ;; esac \
    && printf '%s  /usr/local/bin/ganja\n' "$GANJA_SHA256" \
        | sha256sum --check --strict -

# The host is public routing information, not a credential. Scope the placeholder
# header so the platform can replace it only on requests to the intended host.
ARG GIT_HOST
RUN case "$GIT_HOST" in ""|*[!a-zA-Z0-9.-]*|.*|-*) exit 1 ;; esac \
    && git config --file /etc/ganja/gitconfig \
        "http.https://${GIT_HOST}/.extraHeader" "Authorization: placeholder"

# These are paths, switches, and a public username; the Basic password is not
# in the image — the platform template supplies it at runtime.
# hadolint ignore=DL3064
ENV HOME=/home/ganja \
    GANJA_CONFIG_HOME=/etc/ganja \
    XDG_DATA_HOME=/var/lib/ganja \
    SSL_CERT_FILE=/run/ate/egress-trust-bundle/trust-bundle.pem \
    GIT_CONFIG_GLOBAL=/etc/ganja/gitconfig \
    GIT_SSL_CAINFO=/run/ate/egress-trust-bundle/trust-bundle.pem \
    GANJA_PROVIDER=fake \
    GANJA_FAKE_SCRIPT=/etc/ganja/acceptance-script.json \
    GANJA_DISABLE_MODELS_FETCH=1 \
    GANJA_DISABLE_TERM_PROBE=1 \
    GANJA_SERVER_USERNAME=ganja

# Only the checkout, data and home trees are writable by the worker user.
# Binary and configuration stay root-owned and read-only; the shared temp
# directories lose world write because the fixed acceptance path does not
# need them: checkout uses a temporary directory inside the work tree, the
# TCP server binds no session socket, and tool-output spills use the data
# directory. Clearing and then re-scanning the setuid/setgid bits makes
# the build itself fail if any such file survives.
RUN chmod 0555 /etc/ganja /usr/local/bin/ganja \
    && chmod 0444 /etc/ganja/* \
    && find / -xdev \( -type f -o -type d \) -perm /0022 -exec chmod go-w {} + \
    && chown -R 10001:10001 /work /var/lib/ganja /home/ganja \
    && chmod 0755 /tmp /var/tmp \
    && find / -xdev -type f -perm /6000 -exec chmod a-s {} + \
    && remaining="$(find / -xdev -type f -perm /6000)" \
    && test -z "$remaining"

# Record the reviewed supply inputs where an operator can read them back.
LABEL org.opencontainers.image.base.name="${BASE_IMAGE}" \
    io.ganja.binary.sha256="${GANJA_SHA256}"

USER 10001:10001
WORKDIR /work
EXPOSE 8080
ENTRYPOINT ["ganja", "worker", "init", "--port", "8080"]
