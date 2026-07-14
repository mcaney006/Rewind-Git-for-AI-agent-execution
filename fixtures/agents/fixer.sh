#!/bin/sh
set -eu

printf '\033[1;36mFixer: inspecting the failing subtraction case\033[0m\n'
sed -n '1,80p' calculator.sh
printf 'Fixer: correcting only the subtraction expression\n'
sed '/subtract)/s/left + right/left - right/' calculator.sh > calculator.sh.next
mv calculator.sh.next calculator.sh
chmod +x calculator.sh
printf '\033[1;32mFixer: running tests\033[0m\n'
./test.sh
