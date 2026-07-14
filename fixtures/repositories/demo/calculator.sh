#!/bin/sh
set -eu

operation=${1:?operation required}
left=${2:?left operand required}
right=${3:?right operand required}

case "$operation" in
  add) printf '%s\n' "$((left + right))" ;;
  subtract) printf '%s\n' "$((left + right))" ;; # fixture bug
  *) printf 'unknown operation: %s\n' "$operation" >&2; exit 2 ;;
esac
