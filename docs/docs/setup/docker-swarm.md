---
sidebar_position: 5
---

# Docker Swarm setup

If you use a special docker swarm setup the `host build mode` should work as is and needs no changes.

But if you use the Docker-in-Docker (DinD) build mode you need to add some additional permissions to your stack:

```yaml
version: '3.8'
services:
  aurcache:
    image: ghcr.io/lukas-heiligenbrunner/aurcache:latest
    privileged: true
    security_opt:
      - seccomp=unconfined
      - apparmor=unconfined
    cap_add:
      - SYS_ADMIN
      - SYS_PTRACE
    ...
```

Remember to add proper db, volumes and env. This is just for showing the additional permissions required.