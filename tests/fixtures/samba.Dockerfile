FROM alpine:3.22@sha256:49340b346798875b5592e5d420241f2f9cfba5f47e62ebb1446a1b34734e898f

ARG SAMBA_VERSION=4.21.9-r1
RUN for attempt in 1 2 3; do \
      apk add --no-cache "samba-server=${SAMBA_VERSION}" "samba-common-tools=${SAMBA_VERSION}" && exit 0; \
      sleep 1; \
    done; exit 1
