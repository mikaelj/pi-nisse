#!/bin/sh
# Regression fixture: close stdin FIRST, then announce a valid
# host_ready on stdout, then linger ~15 s without exiting. Once the
# core has read the announcement its next stdin write hits a closed
# pipe deterministically while the child still lives.
exec 0<&-
printf '%s\n' '{"protocol_version":2,"id":"fault-ready","type":"host_ready","runtime":"fake"}'
exec sleep 15
