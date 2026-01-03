FROM archlinux/archlinux:latest
ARG TARGETPLATFORM
ENV TARGETPLATFORM=${TARGETPLATFORM}

ADD docker/add-aur.sh /root
ADD docker/pacman.conf.amd64 /etc/pacman.conf.amd64
RUN bash /root/add-aur.sh ab paru