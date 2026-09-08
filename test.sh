#!/usr/bin/env bash

set -e
DIR=$(realpath $0) && DIR=${DIR%/*}
cd $DIR
if [ -f "sh/env.sh" ]; then
  . sh/env.sh
fi
set -x

NEXTEST="exec cargo nextest run --all-features --status-level fail "
if [[ "$*" =~ (^|[[:space:]])(-p|--package)([[:space:]]|$) ]]; then
  $NEXTEST "$@"
else
  $NEXTEST --workspace --exclude wedb_bench "$@"
fi
