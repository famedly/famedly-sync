#!/bin/sh
set -eux

script_dir=$(dirname $0)

file_creation=$(date -r $script_dir/ca.crt +%s || echo 0)

if [ $(( $(date +%s) - $file_creation )) -gt 2160000 ]; # 25 days old?
then

	# We need to set EKUs (extendedKeyUsage) otherwise MacOS won't trust
	# the certificate
	openssl req -x509 -new -nodes -sha256 -newkey rsa:2048 \
			-keyout $script_dir/server.key \
			-out $script_dir/server.crt \
			-subj "/C=DE/CN=example.org" \
			-addext "subjectAltName = DNS:zitadel, DNS:localhost" \
			-addext "extendedKeyUsage = serverAuth, clientAuth"

	# These keys are not actually secret, and when passed into the docker
	# container the server key needs to be readable by the container user
	chmod go+r $script_dir/server.key

	openssl x509 -outform pem -in $script_dir/server.crt -out $script_dir/ca.crt
	openssl req -x509 -nodes -days 3650 -sha256 -newkey rsa:2048 \
			-CAkey $script_dir/server.key \
			-CA $script_dir/ca.crt \
			-keyout $script_dir/client.key \
			-out $script_dir/client.crt \
			-subj "/CN=admin.example.org"

	chmod a+r $script_dir/client.key
	chmod a+r $script_dir/client.crt
	chmod a+r $script_dir/server.crt

fi
