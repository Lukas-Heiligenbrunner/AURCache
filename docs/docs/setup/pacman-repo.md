---
sidebar_position: 3
---

# Pacman Repository

Add the following to your `/etc/pacman.conf` on your target machine to use the repo:

```bash
# nano /etc/pacman.conf
[repo]
SigLevel = Optional TrustAll
Server = http://<server_ip>:8080/$arch
```
