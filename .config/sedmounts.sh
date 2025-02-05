#!/bin/sh

set -eu

# Change the volume mounts to point to the correct host paths in CI
# runners
sed 's|source: \./|source: /home/runner/work/famedly-sync/famedly-sync/|g' -i tests/environment/docker-compose.yaml
