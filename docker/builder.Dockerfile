ARG TARGETARCH
ARG TARGETVARIANT
ARG TARGETPLATFORM

########## Base images ##########
FROM --platform=linux/amd64 archlinux/archlinux:latest AS arch_amd64
FROM --platform=linux/arm64 lopsided/archlinux:latest AS arch_arm64
FROM --platform=linux/riscv64 ogarcia/archlinux:latest AS arch_riscv64
FROM --platform=linux/arm/v7 lopsided/archlinux-arm32v7:latest AS arch_armv7

########## Select correct base ##########
FROM arch_${TARGETARCH}${TARGETVARIANT:+${TARGETVARIANT}} AS final

ARG TARGETARCH
ARG TARGETVARIANT
ARG TARGETPLATFORM
ENV TARGETARCH=${TARGETARCH}
ENV TARGETVARIANT=${TARGETVARIANT}
ENV TARGETPLATFORM=${TARGETPLATFORM}

########## Files ##########
ADD docker/add-aur.sh /root
ADD docker/pacman.conf.amd64 /etc/pacman.conf.amd64
RUN bash /root/add-aur.sh ab paru