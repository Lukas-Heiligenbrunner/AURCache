FROM lopsided/archlinux:latest

ADD docker/add-aur.sh /root
RUN bash /root/add-aur.sh ab paru