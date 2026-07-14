#!/bin/sh
set -eu

printf '\033[1;36mBreaker: inspecting calculator.sh\033[0m\n'
sed -n '1,80p' calculator.sh
"${REWIND_BIN:?Rewind must expose REWIND_BIN}" mark "before-bad-change"
printf 'Breaker: changing the first arithmetic expression\n'
sed '/add)/s/left + right/left - right/' calculator.sh > calculator.sh.next
mv calculator.sh.next calculator.sh
chmod +x calculator.sh
printf '\033[1;33mBreaker: running tests\033[0m\n'
./test.sh
