#!/bin/sh
# Regression fixture: stay alive ~15 s with stdout open, writing nothing
# and reading nothing. Used by the pi_host Drop-path test to prove that
# dropping a HostProcess whose child is still alive kills and reaps it
# within the bounded budget.
exec sleep 15
