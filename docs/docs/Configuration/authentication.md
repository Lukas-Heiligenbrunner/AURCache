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
| OAUTH_ALLOWED_USERS | String | Email addresses allowed to sign in (see below)                    | null    |

I've tested this with Authentik, but it should work with any OAuth2 provider if it follows the spec.

To disable Authentiation leave all `OAUTH_*` variables undefined. 

## Restricting who may sign in

By default any account your provider will authenticate can sign in. With a
public provider such as Google that means anyone with an account, which is
rarely what you want for a build server.

Set `OAUTH_ALLOWED_USERS` to a list of email addresses to allow only those:

```
OAUTH_ALLOWED_USERS=me@gmail.com,colleague@gmail.com
```

Separate entries with commas or semicolons. Matching is case-insensitive and
surrounding whitespace is ignored. Leave the variable unset — or set it to an
empty value — to allow everyone, which is the behaviour if you never set it.

A few things worth knowing before you rely on it:

- **It matches the `email` claim, not the display name.** A display name is
  something the account holder chooses, so anyone could adopt yours; an address
  is issued by the provider. Make sure your provider returns `email` in its
  userinfo response — AURCache requests the `email` scope, but a provider still
  has to be configured to release the claim.
- **A user with no email is refused** whenever the list is set. Otherwise a
  provider that quietly stopped returning the claim would turn the restriction
  off rather than fail visibly. If nobody can sign in after setting this,
  check the server log: each refusal is logged with the address it saw, or
  `no email reported` when there was none.
- **It is checked when signing in.** Someone already signed in keeps their
  session until it ends, and an API token issued earlier keeps working. To cut
  off an existing user, remove them from the list and delete their API token.

### Example Compose with Oauth2

```yaml
services:
  aurcache:
    restart: unless-stopped
    image: ghcr.io/lukas-heiligenbrunner/aurcache-server:latest
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
      - AUTO_UPDATE_SCHEDULE=@daily
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
    image: postgres:18-trixie
    volumes:
      # The parent of PGDATA, not PGDATA itself: since 18 the data lives in
      # <major>/docker below it, so an upgrade builds the new directory beside the old.
      - ./aurcache/db:/var/lib/postgresql
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