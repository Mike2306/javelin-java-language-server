#!/bin/sh
# Rebuilds the committed class files and acme.jar from src/. Run by hand when a
# fixture changes; the tests read the committed output and never need javac.
set -eu
cd "$(dirname "$0")"
rm -rf classes acme.jar
javac --release 17 -d classes $(find src -name '*.java')
jar --create --file acme.jar -C classes .
