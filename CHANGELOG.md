# Changelog

All notable changes to this project will be documented in this file.

## [0.10.0] - 2025-03-20

### Features

- Add filtering by org and project ids

### Refactor

- Refactor get_next_zitadel_user into stream combinators
- Refactor to rust 2024 edition
- Bump rustfmt style edition to 2024
- Add anyhow_trace, use anyhow_ext

### Documentation

- *(sample-config)* Add AD sample configuration

### Testing

- Minor improvements to test-setup.sh
- Assert that users from other projects are not deleted

### Miscellaneous Tasks

- Bump zitadel_rust_client to v0.3.0
- Add project automation
- Update Dockerfile base image
- Add maintainers to codeowners file

## [0.9.1] - 2025-02-14

### Bug Fixes

- Update file paths in documentation
- Update sample configuration path
- Rework internals to make localparts non-optional

### Documentation

- Add warning about service user scopes
- Document migration steps

### Features

- Make user comparison log lines less verbose
- Expose migration binary in the docker image

### Refactor

- Switch from ldap_poller to a direct implementation

### Styling

- Fix indentation issue that snuck in

## [0.9.0] - 2024-12-13

### Features

- [**breaking**] Add localpart to CSV import and use localpart as Zitadel user ID and localpart metadata

## [0.8.0] - 2024-12-10

### Features

- [**breaking**] Stop relying on a local cache to track changes
- [**breaking**] Use external ID encoding supporting lexicographical order
- Add migration script

### Bug Fixes

- Pre-empt accidentally leaking PII in logs
- [**breaking**] Rework handling of binary fields from LDAP

### Refactor

- Move uuid method to user struct impl

### Documentation

- Split apart sample configurations
- Update README

### Testing

- Split apart multi-source oriented test setup
- Update Zitadel version for test env
- Support single-source sync
- SSO Linking

### Miscellaneous Tasks

- Remove bincode dependency
- Deal with new clippy lint
- Add more error context

### Bump

- Update rust-cache action

## [0.6.0] - 2024-11-05

### Features

- Add CronJob manifest for ldap-sync
- Add PlainLocalpart feature
- Read status attribute with TRUE or FALSE string value

### Testing

- Fix certificate being not trusted on MacOS

### Miscellaneous Tasks

- Add codecov configuration
- Rename to famedly-sync
- Update rust workflow
- Delete unused itertools dependency

## [0.5.0] - 2024-10-15


### Features

- Add CSV source

### Miscellaneous Tasks

- Remove inline `clippy::expect_used` allowances in tests
- Remove outdated lint `#[allow]`s
- Reduce scope of clippy lint
- Add release workflow with SBOM and attestation

## [0.4.0] - 2024-09-18

### Features

- Add UKT Source support
- Add CSV Source support
- [**breaking**] Adjust config struct for multi-source trait
  - The configuration format has changed to accommodate multiple sources. Please
    consult the sample configuration file.
- Retry creating, updating User with an invalid phone number

### Bug Fixes

- Enable the native-tls feature for tonic/reqwest

### Refactor

- Use Source Trait
- Fix typos

## [0.3.0] - 2024-09-03

### Bug Fixes

- Fix zitadel-tools build by bumping go version
- Actually make feature flags use snake_case
- Make zitadel auth error messages richer
- Reissue of test certificates

### Continuous Integration Pipeline

- Add code coverage to the test workflow
- Publish containers to the OSS registry

### Documentation

- Fix mistakes in the sample config
- Add documentation on behavior during deactivate only mode

### Features

- Add dry-run flag
- Add curl to the container for debug purposes
- Add deactivate only mode
- Add support for configuration through env var

### Miscellaneous Tasks

- Lock test tool versions

## [0.2.0] - 2024-08-13

### Bug Fixes

- Set required ldap attributes
- Don't exit when a single user fails to sync
- Don't sync disabled users
- Make the main function log issues in the config file
- Implement `PartialEq` to do deep byte comparison
- Correctly handle ldap_poller errors
- Print error context when errors make it to the main function
- Don't set passwordless registration for users
- [**breaking**] Correct env-var related path issues in the docker image

### Continuous Integration Pipeline

- Update docker workflow
- Fix missing entry to `PATH`
- Don't run everything in a container so we can use docker
- Print docker logs on failure
- Remove coverage-related actions

### Documentation

- Add basic doc comments across the project
- Document edge cases
- Add documentation for testing
- Document LDAPS testing architecture
- Document usage for end users

### Features

- Creation
- Implement Zitadel user creation
- Implement LDAP sync cache
- Add preferred username to user metadata
- Add user grants
- Add UUID to synced users
- Delete disabled users
- Implement propagating LDAP user deletion
- Implement user change sync
- Log successful outcomes better
- [**breaking**] Make LDAPS connections work correctly
- Make phone numbers optional
- Properly handle binary attributes
- Make tls config optional
- [**breaking**] Make using attribute filters optional
- [**breaking**] Implement bitflag for status attribute with multiple disable values
- [**breaking**] Make SSO setup mandatory and assert SSO works properly

### Miscellaneous Tasks

- Fix yaml editorconfig
- Update Dockerfile
- Update to new zitadel-rust-client Zitadel::new()
- Switch famedly dependency URLs from ssh to https
- Remove no longer relevant TODO comment

### Refactor

- Stop using the ldap cache for now
- Clean up user conversion to more easily persist metadata
- Properly represent user fields that aren't static values
- Implement display for our user struct
- Factor the user struct out into its own module

### Styling

- Remove unnecessary imports in the config module
- Give methods proper names

### Testing

- Implement infrastructure for e2e testing
- Switch to openldap for testing
- Clean up Zitadel org before running the tests
- Assert that the Zitadel user is actually created
- Clean up tests a bit by making a struct for ldap
- Implement further e2e test cases
- Improve test setup logging
- Assert that email changes are handled correctly
- Move test template config to allow splitting docs and tests
- Allow `change_user` to take binary values
- Fix missing `.success()` calls on ldap functions
- Add test for syncing binary values

<!-- generated by git-cliff -->
