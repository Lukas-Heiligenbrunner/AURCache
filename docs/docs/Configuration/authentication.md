# Authentication via OAuth2

AURCache supports OAuth2 authentication via various Oauth2 providers such as Authentik or Keycloak. 
This allows you to restrict access to your AURCache instance to only users who have authenticated with one of these services.

Setup the following Environment Variables to enable OAuth2 authentication:

| Variable            | Type   | Description                         | Default |
|---------------------|--------|-------------------------------------|---------|
| OAUTH_AUTH_URI      | String | Oauth authorize endpoint            | null    |
| OAUTH_TOKEN_URI     | String | Oauth token endpoint                | null    |
| OAUTH_REDIRECT_URI  | String | Oauth redirect uri back to AURCache | null    |
| OAUTH_USERINFO_URI  | String | Oauth userinfo endpoint             | null    |
| OAUTH_CLIENT_ID     | String | Oauth client ID                     | null    |
| OAUTH_CLIENT_SECRET | String | Oauth client Secret                 | null    |

To disable Authentiation leave all `OAUTH_*` variables undefined. 