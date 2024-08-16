# Docker Compose setup

There are two ways the packages can be built:
In both ways for each package built a seperate container is spawned and destroyed afterwards.

## DinD (Docker in Docker) build mode
The build container will spawn a new container for each package inside the main container.
For this to work the container needs to be priviledged!


Example with PostgreSQL database (recommended):
```yaml
version: '3'
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
      - MAX_CONCURRENT_BUILDS=1
      - CPU_LIMIT=100
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

Example with SQLite database:
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
## Host build mode
For every package built a new container is spawned on the host system and destroyed afterwards.
For this method the docker socket needs to be mounted to the aurcache container.

Example with PostgreSQL database (recommended):
```yaml
version: '3'
services:
    aurcache:
        image: ghcr.io/lukas-heiligenbrunner/aurcache:latest
        ports:
        - "8080:8080" # Repository
        - "8081:8081" # Frontend
        volumes:
          - ./aurcache/repo:/app/repo
          - /var/run/docker.sock:/var/run/docker.sock
          - artifact_cache:/app/builds
        environment:
          - BUILD_ARTIFACT_DIR=artifact_cache # also absolute path is possible
          - DB_TYPE=POSTGRESQL
          - DB_USER=aurcache
          - DB_PWD=YOUR_SECURE_PWD
          - DB_HOST=dbhost
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

volumes:
  artifact_cache:
        driver: local
```

Example with SQLite database:
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
          - /var/run/docker.sock:/var/run/docker.sock
          - artifact_cache:/app/builds
        environment:
          - BUILD_ARTIFACT_DIR=artifact_cache # also absolute path is possible
volumes:
  artifact_cache:
        driver: local
```
For this method to work you need to mount a exchange volume to pass the built packages to the aurcache container.
In this example the `artifact_cache` volume is mounted to the aurcache container and the `BUILD_ARTIFACT_DIR` environment variable is set to the volume.