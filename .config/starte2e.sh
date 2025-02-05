#!/bin/sh

set -eu

# CI does not add /usr/bin to $PATH for some reason, which means we
# lack docker
export PATH="${PATH}:/usr/bin"
# Assume the default profile on older nextest versions
NEXTEST_PROFILE="${NEXTEST_PROFILE:-default}"

# If we're running in CI, we are in a docker container, so containers
# launched by us do not bind-mount into local directories. Instead, we
# copy the test environment to the host, so that we can bind-mount
# from there.

TEST_ENV="$(realpath "$(dirname "$0")/../tests/environment")"
export ENV_PATH="$TEST_ENV"

if [ "$NEXTEST_PROFILE" = "ci" ]; then
	cp -r tests/environment "$RUNNER_TEMP/test-env"
	TEST_ENV="$RUNNER_TEMP/test-env"
	ENV_PATH="/home/runner/work/_temp/test-env"
	export ENV_PATH
fi

# Make sure the containers can write some files that need to be shared
touch "$TEST_ENV/zitadel/service-user.json"
chmod a+rw "$TEST_ENV/zitadel/service-user.json"

# We only take down ldap if the certs are too old and need regeneration
ldap_down=""
file_creation=$(date -r "$TEST_ENV/certs/ca.crt" +%s || echo 0)
if [ $(($(date +%s) - file_creation)) -gt $((25 * 24 * 60 * 60)) ]; then
	ldap_down="-v ldap"
fi

# Shut down any still running test-setup first
docker compose --project-directory "$TEST_ENV" down -v test-setup "$ldap_down" || true
docker compose --project-directory "$TEST_ENV" up --wait || (
	docker compose --project-directory "$TEST_ENV" logs test-setup
	exit 1
)

echo "TEST_ENV=$TEST_ENV" >> "$NEXTEST_ENV"
cat "$NEXTEST_ENV"
