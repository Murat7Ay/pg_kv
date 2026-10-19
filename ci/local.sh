#!/usr/bin/env bash
# Local stand-in for .github/workflows/ci.yml: builds the shared toolchain stage,
# then builds and tests every PostgreSQL major with test/run.sh, and writes a
# report to ci/results/<time>-<commit>/report.md.
#
#   ./ci/local.sh                 # PostgreSQL 15 16 17 18
#   ./ci/local.sh 17 18           # selected majors
#   POST_STATUS=1 ./ci/local.sh   # also set a "local-ci" commit status on GitHub (gh CLI)
#
# CPU quotas match a GitHub runner (2 for the server, 2 for clients) unless
# SERVER_CPUS / CLIENT_CPUS are set.
set -uo pipefail
export MSYS_NO_PATHCONV=1   # keep container paths intact under Git Bash on Windows
cd "$(dirname "$0")/.."

MAJORS=${*:-15 16 17 18}
export SERVER_CPUS=${SERVER_CPUS:-2} CLIENT_CPUS=${CLIENT_CPUS:-2}
SHA=$(git rev-parse HEAD)
SHORT=${SHA:0:7}
DIRTY=$(git status --porcelain --untracked-files=no | wc -l)
OUT=ci/results/$(date +%Y%m%d-%H%M%S)-$SHORT
mkdir -p "$OUT"
log() { printf '%s  %s\n' "$(date +%H:%M:%S)" "$*"; }

log "commit $SHORT$( [ "$DIRTY" -gt 0 ] && echo " (with uncommitted changes)"), majors: $MAJORS"
log "toolchain stage"
if ! DOCKER_BUILDKIT=1 docker build --progress=plain --target toolchain -t pg_kv-toolchain:local . \
     >"$OUT/toolchain.log" 2>&1; then
  log "toolchain build FAILED, see $OUT/toolchain.log"
  exit 1
fi

declare -A RESULT TIME WARN SERVER
failed=0
for pg in $MAJORS; do
  start=$SECONDS
  log "PostgreSQL $pg: build"
  if ! PG_MAJOR=$pg DOCKER_BUILDKIT=1 docker compose build pg --progress=plain >"$OUT/build-pg$pg.log" 2>&1; then
    RESULT[$pg]="build failed"
  else
    log "PostgreSQL $pg: test"
    if PG_MAJOR=$pg ./test/run.sh >"$OUT/test-pg$pg.log" 2>&1; then
      RESULT[$pg]=passed
    else
      RESULT[$pg]="tests failed"
    fi
  fi
  [ "${RESULT[$pg]}" = passed ] || failed=1
  TIME[$pg]=$((SECONDS - start))
  WARN[$pg]=$(grep -cE '^#[0-9]+ [0-9.]+ warning' "$OUT/build-pg$pg.log" 2>/dev/null || true)
  SERVER[$pg]=$(sed -n 's/^== server: //p' "$OUT/test-pg$pg.log" 2>/dev/null | head -1)
  log "PostgreSQL $pg: ${RESULT[$pg]} (${TIME[$pg]} s)"
done
docker compose stop >/dev/null 2>&1

{
  echo "# Local CI: $SHORT"
  echo
  echo "$(date '+%Y-%m-%d %H:%M %Z'), \`./ci/local.sh $MAJORS\`, server/client CPU quotas $SERVER_CPUS/$CLIENT_CPUS."
  [ "$DIRTY" -gt 0 ] && echo "**The working tree had uncommitted changes.**"
  echo
  echo "| Major | Server | Result | Build warnings | Time |"
  echo "|---|---|---|---:|---:|"
  for pg in $MAJORS; do
    echo "| $pg | ${SERVER[$pg]:-n/a} | ${RESULT[$pg]} | ${WARN[$pg]:-n/a} | ${TIME[$pg]} s |"
  done
  echo
  echo "Logs: \`$OUT/\`"
} >"$OUT/report.md"
echo
cat "$OUT/report.md"

if [ "${POST_STATUS:-0}" = 1 ]; then
  state=$([ $failed = 0 ] && echo success || echo failure)
  gh api "repos/{owner}/{repo}/statuses/$SHA" -f state="$state" -f context=local-ci \
    -f description="PostgreSQL $MAJORS: $state (local run)" >/dev/null \
    && log "posted commit status '$state' for $SHORT"
fi
exit $failed
