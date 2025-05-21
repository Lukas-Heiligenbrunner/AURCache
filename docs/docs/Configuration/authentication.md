---
sidebar_position: 2
---

# Authentication via OAuth2

AURCache supports OAuth2 authentication via various Oauth2 providers such as Authentik or Keycloak. 
This allows you to restrict access to your AURCache instance to only users who have authenticated with one of these services.

Setup the following Environment Variables to enable OAuth2 authentication:

| Variable            | Type   | Description                                                       | Default |
|---------------------|--------|-------------------------------------------------------------------|---------|
| OAUTH_AUTH_URI      | String | Oauth authorize endpoint                                          | null    |
| OAUTH_TOKEN_URI     | String | Oauth token endpoint                                              | null    |
| OAUTH_REDIRECT_URI  | String | Oauth redirect uri back to AURCache (https://yourdomain/api/auth) | null    |
| OAUTH_USERINFO_URI  | String | Oauth userinfo endpoint                                           | null    |
| OAUTH_CLIENT_ID     | String | Oauth client ID                                                   | null    |
| OAUTH_CLIENT_SECRET | String | Oauth client Secret                                               | null    |

I've tested this with Authentik, but it should work with any OAuth2 provider if it follows the spec.

To disable Authentiation leave all `OAUTH_*` variables undefined. 

### Example Compose with Oauth2

```yaml
services:
  aurcache:
    restart: unless-stopped
    image: ghcr.io/lukas-heiligenbrunner/aurcache:latest
    ports:
      - "9091:8080"
      - "9090:8081"
    volumes:
      - ./aurcache/repo:/app/repo
    privileged: true
    environment:
      - DB_TYPE=POSTGRESQL
      - DB_USER=aurcache
      - DB_PWD=<DB_PWD_HERE>
      - DB_HOST=dbhost
      - MAX_CONCURRENT_BUILDS=2
      - AUTO_UPDATE_SCHEDULE=0 0 1 * * *
      - LOG_LEVEL=DEBUG
      - OAUTH_AUTH_URI=https://sso.heili.eu/application/o/authorize/
      - OAUTH_TOKEN_URI=https://sso.heili.eu/application/o/token/
      - OAUTH_REDIRECT_URI=https://aurcache.heili.eu/api/auth
      - OAUTH_USERINFO_URI=https://sso.heili.eu/application/o/userinfo/
      - OAUTH_CLIENT_ID=<CLIENT_ID_HERE>
      - OAUTH_CLIENT_SECRET=<CLIENT_SECRET_HERE>
    networks:
      aurcache_network:

  aurcache_database:
    restart: unless-stopped
    image: postgres:17.4
    volumes:
      - ./aurcache/db:/var/lib/postgresql/data
    environment:
      - POSTGRES_PASSWORD=<DB_PWD_HERE>
      - POSTGRES_USER=aurcache
    networks:
      aurcache_network:
        aliases:
          - "dbhost"

networks:
  aurcache_network:
    driver: bridge
```