FROM lopsided/archlinux:latest
ARG TARGETPLATFORM

ADD docker/add-aur.sh /root
ADD docker/pacman.conf /etc/pacman.conf
RUN bash /root/add-aur.sh ab paru