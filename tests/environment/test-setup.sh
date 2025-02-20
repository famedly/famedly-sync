#!/bin/sh

set -eu

# Script to wait for an ldap server to be up, clean up any existing
# data and then to do some basic initialization.
#
# This is intended for test suite setup, don't use this in production.

LDAP_HOST='ldap://ldap:1389'
LDAP_BASE='dc=example,dc=org'
LDAP_ADMIN='cn=admin,dc=example,dc=org'
LDAP_PASSWORD='adminpassword'

ZITADEL_HOST="http://zitadel:8080"

log() { echo "$@" 1>&2; }

log "Waiting for LDAP to be ready"

retries=5

while [ $retries -gt 0 ]; do
	sleep 5
	retries=$((retries - 1))

	if ldapsearch -D "${LDAP_ADMIN}" -w "${LDAP_PASSWORD}" -H "${LDAP_HOST}" -b "${LDAP_BASE}" 'objectclass=*' >/dev/null; then
		break
	fi
done

log "Authenticating to Zitadel"
zitadel-tools key2jwt --audience="http://localhost" --key=/environment/zitadel/service-user.json --output=/tmp/jwt.txt
zitadel_token="$(curl -sS \
	--fail-with-body \
	--request POST \
	--url "${ZITADEL_HOST}/oauth/v2/token" \
	--header 'Content-Type: application/x-www-form-urlencoded' \
	--header 'Host: localhost' \
	--data grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer \
	--data scope=openid \
	--data scope=urn:zitadel:iam:org:project:id:zitadel:aud \
	--data assertion="$(cat /tmp/jwt.txt)")"
zitadel_token="$(echo "${zitadel_token}" | jq --raw-output .access_token | tr -d '\n')"

zitadel_request() {
	_path="${1}"
	_request_type="${2:-GET}"

	shift 2

	curl -sS \
		--fail-with-body \
		--request "$_request_type" \
		--url "${ZITADEL_HOST}/${_path}" \
		--header 'Host: localhost' \
		--header "Authorization: Bearer ${zitadel_token}" \
		"$@" || exit 1
}

log "Deleting Zitadel users"
zitadel_users="$(zitadel_request management/v1/users/_search POST)"
# Filter out the admin users
zitadel_users="$(echo "$zitadel_users" | jq --raw-output '.result[]? | select(.userName | startswith("zitadel-admin") | not) | .id')"

for id in $zitadel_users; do
	log "Deleting user $id"
	zitadel_request "management/v1/users/$id" DELETE
done

log "Deleting Zitadel projects"
projects="$(zitadel_request 'management/v1/projects/_search' POST)"
# Filter out the ZITADEL project
projects="$(echo "$projects" | jq --raw-output '.result[]? | select(.name == "ZITADEL" | not) | .id')"

for id in $projects; do
	log "Deleting project $id"
	zitadel_request "management/v1/projects/$id" DELETE
done

log "Creating test project"
project_id="$(zitadel_request 'management/v1/projects' POST --data '{"name": "TestProject"}' | jq --raw-output '.id')"
zitadel_request "management/v1/projects/$project_id/roles" POST --data '{"roleKey": "User", "displayName": "User"}'

log "Setting up ldap IDP"
idp_id="$(zitadel_request 'management/v1/idps/ldap' POST --json @- <<EOF | jq --raw-output '.id'
{
    "name": "ldap",
    "servers": ["${LDAP_HOST}"],
    "startTls": false,
    "baseDn": "ou=testorg,${LDAP_BASE}",
    "bindDn": "${LDAP_ADMIN}",
    "bindPassword": "${LDAP_PASSWORD}",
    "userBase": "dn",
    "userObjectClasses": ["shadowAccount"],
    "userFilters": ["(objectClass=shadowAccount)"],
    "attributes": {
        "idAttribute": "uid"
    },
    "providerOptions": {
		"isCreationAllowed": true
    }
}
EOF
)"

log "Updating Zitadel IDs"
org_id="$(zitadel_request 'management/v1/orgs/me' GET | jq --raw-output '.org.id')"

sed -f - /config.template.yaml > /environment/config.yaml <<EOF
	s/<ORGANIZATION_ID>/$org_id/;
	s/<PROJECT_ID>/$project_id/;
	s/<IDP_ID>/$idp_id/;
EOF

log "Deleting LDAP test data"
ldapdelete -D "${LDAP_ADMIN}" -w "${LDAP_PASSWORD}" -H "${LDAP_HOST}" -r "ou=testorg,${LDAP_BASE}" || true

log "Add LDAP test organization"
ldapadd -D "${LDAP_ADMIN}" -w "${LDAP_PASSWORD}" -H "${LDAP_HOST}" -f /environment/ldap/testorg.ldif

log "Current LDAP test org data:"
ldapsearch -D "${LDAP_ADMIN}" -w "${LDAP_PASSWORD}" -H "${LDAP_HOST}" -b "ou=testorg,${LDAP_BASE}" "objectclass=*"

log "Current Zitadel org users:"
zitadel_request management/v1/users/_search POST | jq .result

# Signify that the script has completed
echo "ready" > /tmp/ready

sleep 5
