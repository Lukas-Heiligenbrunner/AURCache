---
sidebar_position: 2
---

# Quick Start

Use the following example docker-compose.yml file to deploy AURCache using Docker and Docker-compose:

```yaml
services:
  aurcache:
    image: ghcr.io/lukas-heiligenbrunner/aurcache:latest
    ports:
      - "8080:8080" # Repository
      - "8081:8081" # Frontend
    volumes:
      - ./aurcache/repo:/app/repo
    privileged: true
    environment:
      - DB_TYPE=POSTGRESQL
      - DB_USER=aurcache
      - DB_PWD=YOUR_SECURE_PWD
      - DB_HOST=dbhost
    networks:
      aurcache_network:
    restart: unless-stopped
  aurcache_database:
    image: postgres:latest
    volumes:
      - ./aurcache/db:/var/lib/postgresql/data
    environment:
      - POSTGRES_PASSWORD=YOUR_SECURE_PWD
      - POSTGRES_USER=aurcache
    restart: unless-stopped
    networks:
      aurcache_network:
        aliases:
          - "dbhost"

networks:
  aurcache_network:
    driver: bridge
```

This setup will use a Postgresql db which is recommended and create a repository in `./aurcache/repo`.

For more advanced setup see the [Configuration](/docs/configuration) page for more information.