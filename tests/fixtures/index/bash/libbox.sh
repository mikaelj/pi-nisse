#!/usr/bin/env bash
# Representative Bash fixture: function forms, sourcing, calls.

source ./lib/common.sh
. ./lib/other.sh
source "$DIR/dynamic.sh"

function foo {
    validate_config "$file"
}

function bar() {
    :
}

baz() {
    if true; then echo hi; fi
}

main
