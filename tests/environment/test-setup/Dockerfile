FROM registry.famedly.net/public-ecr-aws/bitnami/openldap:2.6

USER root

RUN echo 'deb http://deb.debian.org/debian bookworm-backports main' >> /etc/apt/sources.list

RUN apt-get update && apt-get upgrade -y && \
	apt-get install --yes curl jq && \
	apt-get install --yes -t bookworm-backports golang-go && \
	apt-get clean && rm -rf /var/lib/apt/lists /var/cache/apt/archives

RUN GOPATH=/ go install github.com/zitadel/zitadel-tools@v0.5.0
