---
sidebar_position: 2
---

# Quick Start

USe the following example docker-compose.yml file to deploy AURCache using Docker and Docker-compose:

```yaml
version: '3'
services:
    aurcache:
        image: ghcr.io/lukas-heiligenbrunner/aurcache:latest
        ports:
        - "8080:8080" # Repository
        - "8081:8081" # Frontend
        volumes:
          - ./aurcache/db:/app/db
          - ./aurcache/repo:/app/repo
        privileged: true 
```

This setup will create a sqlite database in `./aurcache/db` and a repository in `./aurcache/repo`.
I recommend using this only for testing purposes. 
For production use, you should use a proper database and repository. See the [Configuration](/docs/configuration) page for more information.