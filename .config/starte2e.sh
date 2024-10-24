#!/bin/sh

set -eu

# CI does not add /usr/bin to $PATH for some reason, which means we
# lack docker
export PATH="${PATH}:/usr/bin"

# Make sure the containers can write some files that need to be shared
touch tests/environment/zitadel/service-user.json
chmod a+rw tests/environment/zitadel/service-user.json

# We only take down ldap if the cert are too old and need regeneration
ldap_down=""
file_creation=$(date -r ./tests/environment/certs/ca.crt +%s || echo 0)
if [ $(( $(date +%s) - $file_creation )) -gt 2160000 ]; # 25 days old?
then
ldap_down="-v ldap"
fi

# Shut down any still running test-setup first
docker compose --project-directory ./tests/environment down -v test-setup $ldap_down || true
docker compose --project-directory ./tests/environment up --wait
