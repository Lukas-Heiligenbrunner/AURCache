#!/bin/sh
# Close the server's private state to other users before it starts.
#
# On a kernel without Landlock, aurcache-sandbox confines PKGBUILD parsing by
# running it as `aurcache-parse` and relying on ordinary file permissions --
# and refuses to parse when one of these directories is open to other users.
# Setting the modes here, at every start, keeps that a property of the image
# rather than something each place that creates a file must remember, and
# fixes a volume created by an older image on its first boot with this one.
#
# Directory modes are enough: a 0700 directory shuts out other users whatever
# modes the files inside carry. The repository is not here: it is public over
# HTTP, and the nginx `repo` container reads it as another user.
#
# The paths follow the server's own defaults and environment overrides.
set -eu

for dir in \
    ./db \
    "${AURCACHE_CA_DIR:-./data/ca}" \
    "${AURCACHE_BUILD_LOG_PATH:-./build_logs}" \
    "${AURCACHE_SOURCE_CACHE_PATH:-./source_cache}"; do
    mkdir -p "$dir"
    chmod 0700 "$dir"
done
