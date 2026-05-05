---
sidebar_position: 1
---

# Environment Variables
AURCache can be configured using the following environment variables:

## Settings precedence

Most settings can also be set in the UI — globally on the Settings page or
per-package on the package's Settings page. When the same setting is
configured in multiple places, the resolution order is, from highest to
lowest priority:

1. **Per-package setting** — explicit user intent for one package always
   wins. This lets you override an env-imposed baseline for a single
   package (e.g. give one heavy build more memory) without touching the
   deployment defaults.
2. **Environment variable** — the admin's deploy-time override of the
   global default. Locks the global Settings page tile, but does **not**
   prevent per-package overrides.
3. **Global setting** — UI-set baseline stored in the database.
4. **Static default** — built-in fallback (the "Default" column below).

The UI badges reflect the source: `(default)`, `(inherited)` (= global),
`(inherited from env)`, or no badge when this scope owns the value.

## Database Configuration
| Variable               | Type                  | Description                                                         | Default                   |
|------------------------|-----------------------|---------------------------------------------------------------------|---------------------------|
| DB_TYPE                | (POSTGRESQL\| SQLITE) | Type of Database (SQLite, PostgreSQL)                               | SQLITE                    |
| DB_USER                | String                | POSTGRES Username  (ignored if sqlite)                              | null                      |
| DB_PWD                 | String                | POSTGRES Password  (ignored if sqlite)                              | null                      |
| DB_HOST                | String                | POSTGRES Host   (ignored if sqlite)                                 | null                      |
| DB_NAME                | String                | Database name                                                       | 'db.sqlite' or 'postgres' |

## General Settings

| Variable               | Type          | Description                                                           | Default |
|------------------------|---------------|-----------------------------------------------------------------------|---------|
| VERSION_CHECK_INTERVAL | Integer       | Interval in seconds for checking package versions                     | 3600    |
| AUTO_UPDATE_SCHEDULE   | String (CRON) | Auto update schedule in cronjob syntax with seconds (null to disable) | null    |
| BUILD_ARTIFACT_DIR     | String        | pkg share directory between aurcache container and build containers   | null    |
| LOG_LEVEL              | String        | Log level                                                             | INFO    |
| MAX_CONCURRENT_BUILDS  | Integer       | Max concurrent builds                                                 | 1       |
| CPU_LIMIT              | Integer       | CPU limit of build container in milli CPUs                            | 0       |
| MEMORY_LIMIT           | Integer       | Memory limit of build container in MB                                 | -1      |
| JOB_TIMEOUT            | Integer       | Job timeout for build in Seconds                                      | 3600    |
| SECRET_KEY             | String        | \>32Byte Random String for singing cookies                            | Random  |

## Advanced Settings

| Variable      | Type   | Description                                               | Default                                               |
|---------------|--------|-----------------------------------------------------------|-------------------------------------------------------|
| BUILDER_IMAGE | String | Docker image of Builder which spawns with every build job | ghcr.io/lukas-heiligenbrunner/aurcache-builder:latest |
