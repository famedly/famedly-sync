#!/bin/sh
#
# A small script to convert famedly-sync configuration into its
# equivalent `ldapsearch` command; this can help find out what
# `famedly-sync` sees in practice when performing a sync.
#
# Usage:
#
# ./extract-ldapsearch-command.sh CONFIG

config="$1"

if ! [ -e "$config" ]; then
	echo "Please give the famedly-sync configuration file as the first argument"
	exit 1
fi

grep_from_yaml() {
	grep "$1" "$config" | cut -f 2 -d ':' | tr -d '" '
}

base_dn="$(grep_from_yaml base_dn)"
bind_dn="$(grep_from_yaml bind_dn)"
user_filter="$(grep_from_yaml user_filter)"
start_tls=""
if [ "$(grep_from_yaml danger_use_start_tls)" = "true" ]; then
	start_tls="-Z "
fi
if [ "$(grep_from_yaml use_attribute_filter)" = "true" ]; then
	echo "This command will not include the attribute filter"
	echo "To reproduce the search exactly, add every attribute name space-separated to the end of the command"
	echo
fi

echo "Please read the password from the config file yourself :)"
echo "ldapsearch $start_tls-W -b '$base_dn' -D '$bind_dn' '$user_filter'"
