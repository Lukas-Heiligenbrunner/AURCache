#!/bin/sh
# AURCache server entrypoint.
#
# The server no longer builds packages itself — remote workers do (see
# design/implemented/remote-workers.md), so there is no Podman/Docker daemon to start here.
# Run the server in the foreground so signals and its exit code propagate.
/usr/local/bin/aurcache-private-state
exec /usr/local/bin/aurcache
