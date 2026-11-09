#!/bin/sh
# AURCache server entrypoint.
#
# Workers build the packages (see design/implemented/remote-workers.md), so
# there is no daemon to start here. Run the server in the foreground so signals
# and its exit code propagate.
/usr/local/bin/aurcache-private-state
exec /usr/local/bin/aurcache
