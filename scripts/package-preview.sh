#!/bin/bash
# Secret-free, ad-hoc-signed Apple Silicon preview. Never publishes an updater feed.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo 'Requires an Apple Silicon macOS host' >&2; exit 1; }
sha=$(git rev-parse HEAD)
run="${GITHUB_RUN_ID:-local}"
attempt="${GITHUB_RUN_ATTEMPT:-1}"
[[ "$run" =~ ^[a-zA-Z0-9]+$ && "$attempt" =~ ^[0-9]+$ ]] || exit 2
name="Doki-macos-arm64-${sha:0:12}-${run}-${attempt}"
out="$PWD/dist/preview"
mkdir -p "$out"
# Locked Cargo graph; bundle.sh assembles all native helper/runtime resources.
cargo build --locked --release --package waku --bin waku --bin waku_js_repl --package waku-daemon --bin waku-daemon
WAKU_SKIP_CARGO_BUILD=1 WAKU_CODESIGN_IDENTITY=- ./scripts/bundle.sh release
app="${CARGO_TARGET_DIR:-target}/release/Doki.app"
version=$(python3 -c 'import tomllib; print(tomllib.load(open("Cargo.toml", "rb"))["package"]["version"])')
short_version="${version%%-*}"
plist="$app/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $short_version" "$plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion ${GITHUB_RUN_NUMBER:-1}.$attempt" "$plist"
/usr/libexec/PlistBuddy -c "Add :DokiPreviewCommit string $sha" "$plist"
# Fail closed if a future source change accidentally enables a production feed.
python3 scripts/verify-preview.py "$app"
codesign --force --sign - "$app"
codesign --verify --deep --strict --verbose=2 "$app"
# Preserve framework symlinks and executable bits, unlike uploading a bare .app.
ditto -c -k --sequesterRsrc --keepParent "$app" "$out/$name.zip"
check=$(mktemp -d)
trap 'rm -rf "$check"' EXIT
ditto -x -k "$out/$name.zip" "$check"
python3 scripts/verify-preview.py "$check/Doki.app" --smoke
codesign --verify --deep --strict --verbose=2 "$check/Doki.app"
"$check/Doki.app/Contents/MacOS/waku-daemon" --help
python3 - "$out/$name.json" "$sha" "$version" "$run" "$attempt" <<'PY'
import json, sys
path, sha, version, run, attempt = sys.argv[1:]
with open(path, 'w') as f:
    json.dump(dict(commit=sha, version=version, run_id=run, run_attempt=attempt,
        platform='macos-arm64', minimum_macos='13.0', signing='ad-hoc',
        notarized=False, bundle_id='sh.doki', auto_update=False,
        source_url=f'https://github.com/Plintash/doki/tree/{sha}'), f, indent=2)
    f.write('\n')
PY
cp docs/preview-install.txt "$out/INSTALL.txt"
cp LICENSE "$out/LICENSE"
# Preserve the exact source even when GitHub's temporary PR merge ref expires.
git archive --format=tar.gz --prefix="Doki-source-$sha/" "$sha" > "$out/$name-source.tar.gz"
(cd "$out" && shasum -a 256 "$name.zip" "$name.json" "$name-source.tar.gz" INSTALL.txt LICENSE > SHA256SUMS)
echo "Packaged $out/$name.zip"
