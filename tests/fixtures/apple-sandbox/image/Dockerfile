# Guest image for tests/integration-apple-sandbox.sh.
# A persistent systemd machine userspace: sshd + Docker, no baked identity.
FROM --platform=linux/arm64 ubuntu:24.04@sha256:008173c23f95b170204355c12626cb5a965d779a7e1283b09e9cffbb1bf33ca3

ENV container=container \
    DEBIAN_FRONTEND=noninteractive

COPY machine-setup.sh /tmp/machine-setup.sh
COPY sshd.conf /etc/ssh/sshd_config.d/10-coop-test.conf
COPY verify.sh /usr/local/sbin/coop-test-verify
RUN bash /tmp/machine-setup.sh && rm -f /tmp/machine-setup.sh

STOPSIGNAL SIGRTMIN+3
ENTRYPOINT ["/sbin/init"]
