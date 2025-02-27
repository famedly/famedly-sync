## Testing & Development

This repository uses [`nextest`](https://nexte.st/) to perform test
env spin-up and teardown. To install it, either see their website, or
run:

```
cargo install cargo-nextest --locked
```

Tests can then be run using:

```
cargo nextest run [--no-fail-fast] [-E 'test(<specific_test_to_run>)']
```

Note that tests need to be executed from the repository root since we
do not currently implement anything to find files required for tests
relative to it.

In addition, a modern docker with the `compose` subcommand is
required - importantly, this is not true for many distro docker
packages. Firewalls also need to be configured to allow container <->
container as well as container <-> host communication.

### E2E test architecture

For e2e testing, we need an ldap and a zitadel instance to test
against. These are spun up using docker compose.

After ldap and zitadel have spun up, another docker container runs,
which simply executes a script to clean up existing test data and
create the necessary pre-setup organization structures to support a
test run.

Tests are then expected to directly communicate with ldap/zitadel to
create/delete/modify users and confirm test results.

Importantly, since the tests run with no teardown between individual
tests, users created *must* have different LDAP ID/email addresses, so
that they can be managed independently.

E2E tests cannot run concurrently, since this would cause
synchronization to happen concurrently.

For LDAP-over-TLS, openldap is configured to allow connections without
client certificates, but if one is provided, it must be verified
correctly. This allows us to test scenarios with and without client
certificates.

## Contributing

### Pre-commit usage

1. If not installed, install with your package manager, or `pip install --user pre-commit`
2. Run `pre-commit autoupdate` to update the pre-commit config to use the newest template
3. Run `pre-commit install` to install the pre-commit hooks to your local environment
