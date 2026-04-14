#!/usr/bin/env bash
# Bring up the runtime integration test stack for one ads-runtime profile.
#
# Usage:
#   scripts/run-runtime-tests.sh [--profile <softbeckhoff|tc-runtime|mqtt>] [-- <docker-compose-args>...]
#
# Default profile is softbeckhoff (public CI). The tc-runtime and mqtt profiles
# require TC_RUNTIME_IMAGE to be set to a user-supplied TwinCAT runtime image.
#
# rt-1 is infrastructure only — this script just brings the stack up and waits
# for healthy. Test execution is wired up in rt-2+.

set -euo pipefail

PROFILE="softbeckhoff"
EXTRA_ARGS=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --profile)
      PROFILE="${2:-}"
      if [[ -z "$PROFILE" ]]; then
        echo "error: --profile requires a value (softbeckhoff|tc-runtime|mqtt)" >&2
        exit 2
      fi
      shift 2
      ;;
    --profile=*)
      PROFILE="${1#--profile=}"
      shift
      ;;
    --)
      shift
      EXTRA_ARGS=("$@")
      break
      ;;
    -h|--help)
      sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      EXTRA_ARGS+=("$1")
      shift
      ;;
  esac
done

case "$PROFILE" in
  softbeckhoff|tc-runtime|mqtt) ;;
  *)
    echo "error: unknown profile '$PROFILE' (expected: softbeckhoff|tc-runtime|mqtt)" >&2
    exit 2
    ;;
esac

if [[ "$PROFILE" =~ ^(tc-runtime|mqtt)$ && -z "${TC_RUNTIME_IMAGE:-}" ]]; then
  echo "error: --profile $PROFILE requires TC_RUNTIME_IMAGE to be set" >&2
  exit 2
fi

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
COMPOSE_FILE="$REPO_ROOT/tests/runtime/docker-compose.yml"

if ! command -v docker >/dev/null 2>&1; then
  echo "error: docker is not installed or not on PATH" >&2
  exit 1
fi

echo "==> Bringing up runtime stack (profile: $PROFILE)"
docker compose -f "$COMPOSE_FILE" --profile "$PROFILE" up -d --wait "${EXTRA_ARGS[@]}"

echo "==> Stack is healthy. Services:"
docker compose -f "$COMPOSE_FILE" --profile "$PROFILE" ps

cat <<EOF

Stack is ready. OTLP output will be written to /tmp/otlp.jsonl on the host.

To tear down:
  docker compose -f $COMPOSE_FILE --profile $PROFILE down -v
EOF
