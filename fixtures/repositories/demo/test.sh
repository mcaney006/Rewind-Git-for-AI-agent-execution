#!/bin/sh
set -eu

assert_equal() {
  expected=$1
  actual=$2
  label=$3
  if [ "$actual" != "$expected" ]; then
    printf 'not ok - %s: expected %s, got %s\n' "$label" "$expected" "$actual" >&2
    return 1
  fi
  printf 'ok - %s\n' "$label"
}

failed=0
assert_equal 5 "$(./calculator.sh add 2 3)" addition || failed=1
assert_equal 1 "$(./calculator.sh subtract 3 2)" subtraction || failed=1
exit "$failed"
