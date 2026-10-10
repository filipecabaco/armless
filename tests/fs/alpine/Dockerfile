# An arm64 Alpine guest prepared the way snowglobe's boot layer prepares one
# (internal/boot/boot.Dockerfile), for tests/arm/alpine.mjs.
FROM --platform=linux/arm64 alpine:3.24

RUN apk add --no-cache openrc alpine-base agetty linux-virt linux-firmware-none curl ca-certificates

# The test CA (tests/fixtures; test-only keys): HTTPS in the guest is terminated with it
COPY test-ca.crt /usr/local/share/ca-certificates/armless-test.crt
RUN update-ca-certificates

# ttyS0: root shell for the test; hvc0: the "app" console
RUN sed -i '/^tty[0-9]/d' /etc/inittab && \
    echo 'ttyS0::respawn:/sbin/agetty --autologin root -s ttyS0 115200 vt100' >> /etc/inittab && \
    echo 'hvc0::respawn:/sbin/agetty --autologin root --noclear --noissue hvc0 xterm-256color' >> /etc/inittab && \
    echo "root:" | chpasswd && \
    echo 'hostname="armless"' > /etc/conf.d/hostname && \
    rm -f /etc/motd && touch /root/.hushlogin

RUN for i in devfs dmesg mdev hwdrivers; do rc-update add $i sysinit; done && \
    for i in modules sysctl hostname bootmisc; do rc-update add $i boot; done && \
    rc-update add killprocs shutdown

RUN mkinitfs -F "base virtio 9p" $(cat /usr/share/kernel/virt/kernel.release)
RUN rm -rf /usr/share/doc /usr/share/man /var/cache/apk/* /lib/firmware/*
