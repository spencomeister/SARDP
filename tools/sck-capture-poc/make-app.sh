#!/bin/zsh
# Wraps the built sck-capture-poc binary in a minimal .app bundle, signs it
# (ad-hoc unless SARDP_SIGN_IDENTITY is set) and launches it with `open`.
#
# Why: TCC attributes a permission prompt to the *responsible process*. A
# binary started from a shell inherits the terminal/IDE as responsible
# process (the Screen Recording dialog would say "Claude"/"Terminal"). An
# .app launched by `open` (LaunchServices) is its own responsible process,
# which is what sardp-server will be when launched as a LaunchAgent.
#
# usage: make-app.sh [--package P] [--bin NAME | --example NAME]
#                    [--app-name NAME] [--release] [--no-run] [-- <args...>]
#
# --app-name makes a *separate* bundle with the same identifier, so two
# signed binaries (say a server and a guard) can run side by side without
# overwriting each other. TCC matches on the designated requirement --
# identifier plus certificate -- not on the path, so one grant covers both.
#
# The bundle identifier stays the same whichever binary is packaged, so one
# Screen Recording grant covers all of them: the designated requirement is
# `identifier ... and certificate leaf = H"..."`, and neither half changes
# when the executable inside does.
set -euo pipefail
cd "$(dirname "$0")"
export PATH="$HOME/.local/share/mise/shims:$PATH"

profile=debug
run=1
pkg=sck-capture-poc
kind=bin
binname=sck-capture-poc
appname=SckCapturePoC
while [[ $# -gt 0 ]]; do
  case "$1" in
    --app-name) appname="$2"; shift 2 ;;
    --package) pkg="$2"; shift 2 ;;
    --bin) kind=bin; binname="$2"; shift 2 ;;
    --example) kind=example; binname="$2"; shift 2 ;;
    --release) profile=release; shift ;;
    --no-run) run=0; shift ;;
    --) shift; break ;;
    *) break ;;
  esac
done

build=(cargo build -p "$pkg" "--$kind" "$binname")
# An `if`, not `[[ ... ]] && ...`: under `set -e` a false AND-list as the
# last command would abort the script.
if [[ $profile == release ]]; then build+=(--release); fi
"${build[@]}"
target="$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
if [[ $kind == example ]]; then bin="$target/$profile/examples/$binname"; else bin="$target/$profile/$binname"; fi

bundle_id="${SARDP_BUNDLE_ID:-io.sardp.sck-capture-poc}"
app="$target/$profile/$appname.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS"
cp "$bin" "$app/Contents/MacOS/$binname"
cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>$bundle_id</string>
  <key>CFBundleName</key><string>$appname</string>
  <key>CFBundleExecutable</key><string>$binname</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>CFBundleShortVersionString</key><string>0.1</string>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF
# Signing identity decides how durable a TCC grant is, so this is not a
# detail to leave to a default. An ad-hoc signature ("-") gives a
# designated requirement of `identifier ... and cdhash H"..."`, which
# changes on *every* rebuild; worse, re-signing an already-granted bundle
# ad-hoc makes tccd rewrite the stored requirement to that cdhash
# (UpdateVerifierData), silently destroying a grant that was previously
# pinned to a stable certificate. Signing with a (self-signed) certificate
# gives `identifier ... and certificate leaf = H"..."`, which survives
# rebuilds. Default to the project's dev certificate and only fall back to
# ad-hoc when explicitly asked.
identity="${SARDP_SIGN_IDENTITY:-SARDP Dev Signing}"
if [[ "$identity" != "-" ]] && ! security find-certificate -c "$identity" >/dev/null 2>&1; then
  echo "error: signing identity '$identity' is not in the keychain." >&2
  echo "       create it, or pass SARDP_SIGN_IDENTITY=- to sign ad-hoc" >&2
  echo "       (ad-hoc grants break on every rebuild -- see README)." >&2
  exit 1
fi
codesign --force --sign "$identity" --identifier "$bundle_id" "$app"
echo "bundle: $app"
codesign -dv "$app" 2>&1 | grep -E "Identifier|CDHash|Signature|TeamIdentifier" || true

if [[ $run == 1 ]]; then
  logdir="${SARDP_POC_LOGDIR:-$PWD/captures}"
  mkdir -p "$logdir"
  log="$logdir/run_$(date +%s).log"
  echo "launching via open; log: $log"
  open -W -n --stdout "$log" --stderr "$log" "$app" --args "$@"
  echo "--- exit; log follows"
  cat "$log"
fi
