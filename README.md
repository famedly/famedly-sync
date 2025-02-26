# Famedly Sync

This tool synchronizes users from different sources to Famedly's Zitadel instance.

Currently supported sources:
- LDAP
- CSV
- Custom endpoint provided by UKT

## Deployment

Note that famedly-sync is currently not designed to be deployed to
projects with existing users, or projects that have been synced to
with a different source before.

> [!CAUTION]
>
> Since famedly-sync relies on metadata to perform its tasks, if users
> that were not created by famedly-sync using the same source exist in
> the instance, they may be **deleted** by a sync, or cause desync
> issues if they happen to be users with elevated permissions.

### LDAP

To deploy famedly-sync to sync an LDAP server to a Zitadel instance,
the following need to be collected first:

- LDAP instance details
  - URL of the LDAP/AD server
  - Bind DN (=~username of the admin user) with which to authenticate
  - Bind password with which to authenticate
  - Base DN under which the users that should be synced are available
  - An LDAP user filter to select specifically the users to sync
  - The mapping of LDAP attributes to Zitadel attributes
    - This may differ between instances, hence it's configurable. It
      is often quite straightforward from listing the users with
      `ldapsearch`, but more on this later
  - TLS certificates, if TLS is used and mTLS is required and/or the
    server certificate is not in the root store
- Zitadel instance details
  - A service user must be created.
    - The [Zitadel
      documentation](https://zitadel.com/docs/guides/integrate/service-users/private-key-jwt#steps-to-authenticate-a-service-user-with-private-jwt)
      covers creating such a user.
    - The user should be created with *only* the `Iam User Manager`
      role for the organization that should be synced to.
  - The relevant Project and Org IDs
    - These are the "Resource Ids" listed on their pages in the
      Zitadel UI
  - SSO settings
    - When using SSO, an IDP ID needs to be defined. IDP configuration
      is located in the Zitadel instance's default settings - the ID
      of a specific IDP can be glanced from its URL.
    - Currently, this is not optional, see [this
      issue](https://github.com/famedly/famedly-sync/issues/102). For
      now, setting the ID to `000000000000000000` when not needed is a
      reasonable workaround.

Once this has been gathered, the following steps are advised:

1. Asserting correctness of the LDAP configuration

   This can be done using `ldapsearch`, which is part `openldap` (on
   ubuntu hosts, in the `ldap-utils` package). The basic search command
   can be translated from the previously acquired data to:

   ```
   ldapsearch -H "$LDAP_URL" -D "$LDAP_BIND_DN" -w "$LDAP_BIND_PASSWORD" -b "$LDAP_BASE_DN" "$LDAP_USER_FILTER"
   ```

   If this does not return all user data required for the attribute
   mapping, also add the expected attributes to the end of the command.

2. Writing the famedly-sync configuration

   To prevent this document going out of date, we will not repeat the
   configuration syntax here. See
   [./sample-configs/ldap-config.sample.yaml](./sample-configs/ldap-config.sample.yaml)
   for details.

   Noteworthy is that the `use_attribute_filter` config setting
   *should* be set to true if, and only if, attributes needed to be
   added to the `ldapsearch` command.

3. Deploying the docker container

   A relatively fool-proof way of doing this is with `docker
   compose`. This composefile, and a copy of the service user's JWT
   and the just-created configuration in `./opt/`, is all that should
   be required:

   ```yaml
   services:
     famedly-sync-agent:
       image: docker-oss.nexus.famedly.de/famedly-sync-agent:<version>
       volumes:
         - type: bind
           source: ./opt
           target: /opt/famedly-sync
       network_mode: host
   ```

   Assuming a recent-enough docker version, this can then be invoked
   with `docker compose up`.

   Since the `./opt` directory will be bind-mounted to
   `/opt/famedly-sync` in the docker container (and docker containers
   do not permit access to host files by other means), any files
   referenced in the configuration *should* be relative to prevent
   file access issues.

## Migrations

`famedly-sync` v0.8.0 changed the user ID schema, and therefore
requires a migration step. For this, a `migrate` binary was added,
which reads the same configuration file as the main `famedly-sync`
binary, and simply performs the migration as needed.


Starting with v0.9.1-rc1, the binary is also included in the docker
image, under `/usr/local/bin/migrate`. Due to how the image is set up,
using this binary requires setting the container entrypoint to that
file.

To confirm successful migration, look at any user and confirm that the
`Nickname` field was updated to be a hex-encoded string, rather than a
base64-encoded one. No other values should change during migration.

A Zitadel instance that is already using hex-encoded IDs will not be
altered (though famedly-sync will still make various HTTP calls).

## Kubernetes Deployment

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

### Configuration through env variables

Since kubernetes deployments prefer env-based configuration, famedly-sync supports this.

Individual configuration items and the whole configuration can be set using environment variables. For example, the following YAML configuration:

```yaml
sources:
  ldap:
    url: ldap://localhost:1389
```

Could be set using the following environment variable:

```bash
FAMEDLY_SYNC__SOURCES__LDAP__URL="ldap://localhost:1389"
```

Some configuration items take a list of values. In this cases the values should be separated by space. **If an empty list is desired the variable shouldn't be created.**

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
