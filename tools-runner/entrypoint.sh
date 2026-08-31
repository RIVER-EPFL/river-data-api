#!/bin/sh
set -e

# Read by /etc/apache2/envvars, which expands it unguarded.
export APACHE_CONFDIR=/etc/apache2

# The account Apache reports as its own. It cannot be switched at start (the process is already
# unprivileged), so the directive is aligned with reality to keep the notice out of the log.
export APACHE_RUN_USER=runner
export APACHE_RUN_GROUP=runner
export HOME=/tmp

. /etc/apache2/envvars

# OpenCPU keeps a directory per POST for its session API. That API is off here and nothing reads
# them, so they are pruned rather than left to fill the tmpfs the store lives on.
prune_sessions() {
	while true; do
		sleep 600
		find /tmp/ocpu-store /tmp/ocpu-temp -mindepth 1 -mmin +15 -delete 2>/dev/null || true
	done
}
prune_sessions &

# The port is a start-time decision: binding 80 needs a capability this container does not have on
# every runtime, and the compose service publishes whichever port is chosen here. RUNNER_ADDRESS
# restricts the listener to one address (the Kubernetes deployment sets 127.0.0.1 so the runner is
# reachable only from inside its own pod); unset, it listens on every interface.
exec apache2 -DFOREGROUND -C "Listen ${RUNNER_ADDRESS:+${RUNNER_ADDRESS}:}${RUNNER_PORT:-80}"
