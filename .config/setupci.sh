#!/bin/sh

set -eu

cp -r tests/environment /__w/test-env

# Change the volume mounts to point to the correct host paths in CI
# runners
sed 's|source: \./|source: /home/runner/work/test-env/|g' -i tests/environment/docker-compose.yaml
