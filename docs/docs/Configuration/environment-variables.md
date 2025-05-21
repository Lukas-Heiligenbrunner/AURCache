---
sidebar_position: 1
---

# Environment Variables
AURCache can be configured using the following environment variables:

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
