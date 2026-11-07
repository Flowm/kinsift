#!/bin/sh
# Download guessit's test fixtures (LGPL-3.0) into target/ for the
# guessit_fixtures test. They are never committed to this repository.
set -eu
ref="${GUESSIT_REF:-develop}"
dest="$(dirname "$0")/../target/guessit-fixtures"
mkdir -p "$dest"
for f in episodes movies various; do
  curl -fsSL "https://raw.githubusercontent.com/guessit-io/guessit/${ref}/guessit/test/${f}.yml" -o "$dest/${f}.yml"
done
echo "fixtures in $dest (guessit ${ref})"
