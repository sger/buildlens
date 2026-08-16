#!/bin/sh
# BuildLens Xcode post-action.
#
# Install: Xcode > Product > Scheme > Edit Scheme > Build > Post-actions >
#   "+" > New Run Script Action, "Provide build settings from" = your app
#   target, then paste:
#
#       "$SRCROOT/path/to/buildlens/scripts/xcode-post-action.sh"
#
# To also record failed builds, add runPostActionsOnFailure="YES" to the
# scheme's <BuildAction> element (Xcode has no UI for it; edit the
# .xcscheme file directly).
#
# The log is read in place and only redacted derived metrics reach PostgreSQL.

set -eu

BUILDLENS="${BUILDLENS_BIN:-buildlens}"
DB="${BUILDLENS_DATABASE_URL:-postgres://localhost/buildlens}"
REPO="${BUILDLENS_REPO:-${SRCROOT:-$PWD}}"
LOG="${BUILDLENS_LOG:-$HOME/.buildlens/collect.log}"

mkdir -p "$(dirname "$LOG")"

# BUILD_DIR points at .../DerivedData/<Project>-<hash>/Build/Products;
# its great-grandparent is the project's DerivedData directory. If it is
# missing or unreadable, fall back to searching all of DerivedData.
BUILD_DIR_ARG=""
if [ -n "${BUILD_DIR:-}" ]; then
  if PROJECT_DERIVED_DATA=$(cd "$BUILD_DIR/../.." 2>/dev/null && pwd); then
    BUILD_DIR_ARG="--build-dir $PROJECT_DERIVED_DATA"
  fi
fi

# Xcode sets PROJECT_NAME for post-actions that provide build settings from a
# target. Passing it records the real project name instead of inferring one
# from the log's DerivedData directory, which is only usually the same thing.
# Set as a positional pair so a name containing spaces survives quoting.
if [ -n "${PROJECT_NAME:-}" ]; then
  set -- --project "$PROJECT_NAME"
else
  set --
fi

# Xcode kills post-actions that outlive the build, so detach and let the
# collector wait for the log to finish writing on its own.
{
  # shellcheck disable=SC2086
  "$BUILDLENS" collect $BUILD_DIR_ARG "$@" \
    --db "$DB" \
    --repo "$REPO" \
    --timeout 120 \
    --collect-all
} >>"$LOG" 2>&1 &

exit 0
