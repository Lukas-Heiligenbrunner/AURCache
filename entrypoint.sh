#!/bin/sh

# Start main process
/usr/local/bin/aurcache &

DOCKER_SOCKET="/var/run/docker.sock"
# Check if the Docker socket is available
if [ -S "$DOCKER_SOCKET" ]; then
    echo "Docker socket is available."
else
    echo "Docker socket is not available. Starting Podman service..."

    podman system service --time=0 unix://$DOCKER_SOCKET &
fi

# Wait for any process to exit
wait -n

# Exit with status of process that exited first
exit $?