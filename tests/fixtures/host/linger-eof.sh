#!/bin/sh
# Regression fixture: close stdout, then linger ~15 s without exiting.
# Used by pi_host integration tests to prove the Rust core reaps a
# child with a bounded wait when the reader sees EOF but the process
# still lives (a Node child cannot close its stdio fds, hence sh).
exec 1>&-
exec sleep 15
