#!/bin/sh

# Start main process
/usr/local/bin/aurcache &

if [[ -z "${BUILD_ARTIFACT_DIR}" ]]; then
  echo "Starting Podman service."
  podman system service --time=0 unix:///var/run/docker.sock &
else
  echo "Docker socket should be available."
fi

# Wait for any process to exit
wait -n

# Exit with status of process that exited first
exit $?