# Famedly Sync

This tool synchronizes users from different sources to Famedly's Zitadel instance.

Currently supported sources:
- LDAP
- CSV
- Custom endpoint provided by UKT

## Configuration

The tool expects a configuration file located at `./config.yaml`. See example configuration at [config.sample.yaml](./config.sample.yaml).

The default path can be changed by setting the new path to the environment variable `FAMEDLY_SYNC_CONFIG`.

Also, individual configuration items and the whole configuration can be set using environment variables. For example, the following YAML configuration:

```yaml
sources:
  ldap:
    url: ldap://localhost:1389
```

Could be set using the following environment variable:

```bash
FAMEDLY_SYNC__SOURCES__LDAP__URL="ldap://localhost:1389"
```

Note that the environment variable name always starts with the prefix `FAMEDLY_SYNC` followed by keys separated by double underscores (`__`).

Some configuration items take a list of values. In this cases the values should be separated by space. **If an empty list is desired the variable shouldn't be created.**

Config can have **various sources** to sync from. When a source is configured, the sync tool tries to update users in Famedly's Zitadel instance based on the data obtained from the source.

**Feature flags** are optional and can be used to enable or disable certain features.

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

## Deployment

The easiest way to deploy this tool is using our published docker
container through our [composefile](./docker-compose.yaml).

To prepare for use, we need to provide a handful of files in an `opt`
directory in the directory where `docker compose` will be
executed. This is the expected directory structure of the sample
configuration file:

```
opt
├── certs
│  └── test-ldap.crt   # The TLS certificate of the LDAP server
├── config.yaml        # An example can be found in config.sample.yaml
└── service-user.json  # Provided by famedly
```

Once this is in place, the container can be executed in the parent
directory of `opt` with:

```
docker compose up
```

Or alternatively, without `docker compose`:

```
docker run --rm -it --network host --volume ./opt:/opt/famedly-sync-agent docker-oss.nexus.famedly.de/famedly-sync-agent:latest
```

### Kubernetes Deployment

The provided manifest `ldap-sync-cronjob.yaml` can be used
to deploy this tool as a CronJob on a Kubernetes cluster.

```
kubectl create -f ldap-sync-deployment.yaml
```

It will run `docker-oss.nexus.famedly.de/famedly-sync-agent:v0.4.0` once per day
at 00:00 in the namespace `ldap-sync`. It requires a ConfigMap named `famedly-sync`
to be present in the `ldap-sync` namespace. The ConfigMap can be created using

```
kubectl create configmap --from-file config.yaml famedly-sync --namespace ldap-sync
```

## Quirks & Edge Cases

- When Setting up SSO, note that Zitadel's ldap filter must be
  configured to resolve the "username" as the user's email address for
  our IDP links to work.
- Changing a user's LDAP id (the attribute from the `user_id` setting)
  is unsupported, as this is used to identify the user on the Zitadel
  end.
- Disabling a user on the LDAP side (with `status`) results in the
  user being deleted from Zitadel.
- If a user's email or phone number changes, they will only be
  prompted to verify it if the tool is configured to make users verify
  them.
- Changing a user's email also immediately results in a new
  login/username.
- If SSO is turned on later, existing users will not be linked.

---

# Famedly

**This project is part of the source code of Famedly.**

We think that software for healthcare should be open source, so we publish most
parts of our source code at [github.com/famedly](https://github.com/famedly).

Please read [CONTRIBUTING.md](CONTRIBUTING.md) for details on our code of
conduct, and the process for submitting pull requests to us.

For licensing information of this project, have a look at the [LICENSE](LICENSE.md)
file within the repository.

If you compile the open source software that we make available to develop your
own mobile, desktop or embeddable application, and cause that application to
connect to our servers for any purposes, you have to agree to our Terms of
Service. In short, if you choose to connect to our servers, certain restrictions
apply as follows:

- You agree not to change the way the open source software connects and
  interacts with our servers
- You agree not to weaken any of the security features of the open source software
- You agree not to use the open source software to gather data
- You agree not to use our servers to store data for purposes other than
  the intended and original functionality of the Software
- You acknowledge that you are solely responsible for any and all updates to
  your software

No license is granted to the Famedly trademark and its associated logos, all of
which will continue to be owned exclusively by Famedly GmbH. Any use of the
Famedly trademark and/or its associated logos is expressly prohibited without
the express prior written consent of Famedly GmbH.

For more
information take a look at [Famedly.com](https://famedly.com) or contact
us by [info@famedly.com](mailto:info@famedly.com?subject=[GitLab]%20More%20Information%20)
