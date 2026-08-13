#!/usr/bin/env bash
set -euo pipefail

target="${1:?usage: sign-notarize-macos.sh TARGET}"
case "$target" in
  aarch64-apple-darwin | x86_64-apple-darwin) ;;
  *)
    echo "unsupported macOS target: $target" >&2
    exit 1
    ;;
esac

: "${APPLE_SIGNING_IDENTITY:?APPLE_SIGNING_IDENTITY is required}"
: "${APPLE_ID:?APPLE_ID is required}"
: "${APPLE_PASSWORD:?APPLE_PASSWORD is required}"
: "${APPLE_TEAM_ID:?APPLE_TEAM_ID is required}"
[[ "$APPLE_TEAM_ID" =~ ^[A-Z0-9]{10}$ ]] || {
  echo "APPLE_TEAM_ID must be a 10-character Apple Team ID" >&2
  exit 1
}

archives=(target/distrib/mcplex-*"$target".tar.xz)
if [[ ${#archives[@]} -ne 1 || ! -f "${archives[0]}" ]]; then
  echo "expected one cargo-dist archive for $target" >&2
  printf 'candidate: %s\n' "${archives[@]}" >&2
  exit 1
fi
archive="${archives[0]}"
archive_name="$(basename "$archive")"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

tar -xJf "$archive" -C "$work"
roots=("$work"/mcplex-*)
if [[ ${#roots[@]} -ne 1 || ! -d "${roots[0]}" ]]; then
  echo "expected one root directory in $archive" >&2
  exit 1
fi
root="${roots[0]}"

shared_requirement="$work/mcplex-keychain.req"
cat > "$shared_requirement" <<EOF
designated => anchor apple generic
  and certificate 1[field.1.2.840.113635.100.6.2.6] exists
  and certificate leaf[field.1.2.840.113635.100.6.1.13] exists
  and certificate leaf[subject.OU] = "$APPLE_TEAM_ID"
  and (identifier "com.aprilnea.mcplex.cli"
       or identifier "com.aprilnea.mcplex.daemon")
EOF
shared_expression="$work/mcplex-keychain-expression.req"
sed '1s/^designated => //' "$shared_requirement" > "$shared_expression"

sign_and_verify() {
  local binary="$1"
  local identifier="$2"
  [[ -x "$binary" ]] || {
    echo "missing executable: $binary" >&2
    exit 1
  }

  codesign \
    --force \
    --options runtime \
    --timestamp \
    --identifier "$identifier" \
    --requirements "$shared_requirement" \
    --sign "$APPLE_SIGNING_IDENTITY" \
    "$binary"
  codesign --verify --strict --verbose=2 "$binary"
  codesign --verify --strict --verbose=2 \
    --test-requirement "$shared_expression" \
    "$binary"

  local details
  details="$(codesign --display --verbose=4 "$binary" 2>&1)"
  grep -Fxq "Identifier=$identifier" <<<"$details"
  grep -Fxq "TeamIdentifier=$APPLE_TEAM_ID" <<<"$details"
}

sign_and_verify "$root/mcplex" "com.aprilnea.mcplex.cli"
sign_and_verify "$root/mcplex-daemon" "com.aprilnea.mcplex.daemon"

cli_requirement="$(codesign --display -r- "$root/mcplex" 2>&1 | sed -n '/designated =>/p')"
daemon_requirement="$(codesign --display -r- "$root/mcplex-daemon" 2>&1 | sed -n '/designated =>/p')"
[[ -n "$cli_requirement" && "$cli_requirement" == "$daemon_requirement" ]] || {
  echo "mcplex and mcplex-daemon do not have identical designated requirements" >&2
  exit 1
}
grep -Fq 'identifier "com.aprilnea.mcplex.cli"' <<<"$cli_requirement"
grep -Fq 'identifier "com.aprilnea.mcplex.daemon"' <<<"$cli_requirement"

notarization_zip="$work/mcplex-$target-notarization.zip"
ditto -c -k --keepParent "$root" "$notarization_zip"
xcrun notarytool submit "$notarization_zip" \
  --apple-id "$APPLE_ID" \
  --password "$APPLE_PASSWORD" \
  --team-id "$APPLE_TEAM_ID" \
  --wait

rm "$archive"
tar -cJf "$archive" -C "$work" "$(basename "$root")"
(
  cd "$(dirname "$archive")"
  shasum -a 256 "$archive_name" > "$archive_name.sha256"
)
