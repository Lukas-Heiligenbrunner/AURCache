# AURCache

AURCache is a build server and repository for Archlinux packages sourced from the AUR (Arch User Repository). It features a Flutter frontend and Rust backend, enabling users to add packages for building and subsequently serves them as a pacman repository. Notably, AURCache automatically detects when a package is out of date and displays it within the frontend.

<p><img src="res/imgs/screenshot1.png" alt=""/> 

<details>
<summary>More Images:</summary>
<br>
<img src="res/imgs/screenshot2.png" alt=""/> 
<img src="res/imgs/screenshot3.png" alt=""/></p>
</details>

## Deployment with Docker and Docker-compose

To deploy AURCache using Docker and Docker-compose, you can use the following example docker-compose.yml file:

```yaml
version: '3'
services:
    aurcache:
        image: luki42/aurcache:latest
        ports:
        - "8080:8080"
        - "8081:8081"
        volumes:
          - ./aurcache/db:/app/db
          - ./aurcache/repo:/app/repo
```

Make sure to define the db path and repo path as volumes.

The default Port 8081 serves the Frontend and Port 8080 serves the Repository.

To start AURCache with Docker-compose, run:

```bash
docker-compose up -d
```

Access AURCache through your web browser at http://localhost:8081.

You can now start adding packages for building and utilizing the AURCache repository.

Add the following to your `pacman.conf` on your target machine to use the repo:

```bash
# nano /etc/pacman.conf
[repo]
SigLevel = Optional TrustAll
Server = http://localhost:8080/
```

## Configuration
Environment Variables

| Variable   | Type                  | Description                                       | Default |
|------------|-----------------------|---------------------------------------------------|---------|
| DB_TYPE    | (POSTGRESQL\| SQLITE) | Type of Database (SQLite, PostgreSQL)             | SQLITE  |
| DB_USER | String                | POSTGRES Username  (ignored if sqlite)            | null    |
| DB_PWD | String                | POSTGRES Password  (ignored if sqlite)            | null    |
| DB_HOST | String                | POSTGRES Host   (ignored if sqlite)               | null    |
| VERSION_CHECK_INTERVAL | Integer | Interval in seconds for checking package versions | 3600    |

## Build Info

The AURCache project comprises two main components: a Flutter frontend and a Rust backend.
### Frontend (Flutter)

To build the Flutter frontend, ensure you have Flutter SDK installed. Then, execute the following commands:

```bash
cd frontend
flutter pub get
flutter build web
```

### Backend (Rust)

To build the Rust backend, make sure you have Rust installed. Then, navigate to the backend directory and run:

```bash
cd backend
cargo build --release
```

## Things still missing

* proper error return from api
* proper logging
* auto update packages
* implement repo-add in rust
* keep older pkg versions in repo (repo-add limitation)


## Contributors

    Lukas-Heiligenbrunner

## License

This project is licensed under the MIT License. Feel free to contribute and modify as per the guidelines outlined in the license agreement.
