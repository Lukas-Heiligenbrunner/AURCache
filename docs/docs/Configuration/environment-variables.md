---
sidebar_position: 1
---

# Environment Variables


| Variable               | Type                  | Description                                                         | Default |
|------------------------|-----------------------|---------------------------------------------------------------------|---------|
| DB_TYPE                | (POSTGRESQL\| SQLITE) | Type of Database (SQLite, PostgreSQL)                               | SQLITE  |
| DB_USER                | String                | POSTGRES Username  (ignored if sqlite)                              | null    |
| DB_PWD                 | String                | POSTGRES Password  (ignored if sqlite)                              | null    |
| DB_HOST                | String                | POSTGRES Host   (ignored if sqlite)                                 | null    |
| VERSION_CHECK_INTERVAL | Integer               | Interval in seconds for checking package versions                   | 3600    |
| BUILD_ARTIFACT_DIR     | String                | pkg share directory between aurcache container and build containers | null    |
| LOG_LEVEL              | String                | Log level                                                           | INFO    |
| MAX_CONCURRENT_BUILDS  | Integer               | Max concurrent builds                                               | 1       |
| CPU_LIMIT              | Integer               | CPU limit of build container in milli CPUs                          | 0       |
| MEMORY_LIMIT           | Integer               | Memory limit of build container in MB                               | -1      |