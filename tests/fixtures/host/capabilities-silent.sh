#!/bin/sh
# Announce readiness, then go silent: never answer host_config with
# model_capabilities (capabilities-timeout fixture).
printf '%s\n' '{"protocol_version":2,"id":"fault-ready","type":"host_ready","runtime":"fake"}'
exec sleep 30
