#!/usr/bin/env bash
# Cargo runner used by `tauri dev` on macOS.
#
# cargo invokes runners with the path to the freshly-built binary as the first
# argument. We:
#   1. Wrap the binary in a minimal `.app` bundle (Muni-dev.app) — modern macOS
#      TCC (Input Monitoring, Accessibility) treats `.app` bundles as
#      first-class and is unreliable for raw mach-O executables. The user
#      grants permission against the bundle, and the grant survives across
#      rebuilds because the bundle's identifier + signing identity stay stable.
#   2. Sign the bundle with the local "Muni Dev" identity (matching production
#      `tauri build`'s identity) so the code signature is stable. If the cert
#      isn't installed, fall back to launching unsigned with a clear message.
#   3. Exec the bundled binary so cargo sees a normal child process exit.

set -euo pipefail

BINARY="${1:-}"
if [[ -z "${BINARY}" ]]; then
  echo "dev-runner: no binary path provided" >&2
  exit 64
fi
shift

# cargo applies this runner to EVERY binary it executes on macOS — not only the
# dev app via `cargo run`, but also every test / bench / example harness spawned
# by `cargo test`. Those harnesses must run in-process so libtest owns stdio and
# cargo can read their results directly; wrapping them in a detached `.app`
# launched via `open -n` makes `cargo test` hang (the bundle is App-Napped /
# LaunchServices-throttled in headless sessions, and the harness output is
# redirected into a log file cargo never reads). Only the real app binary needs
# the TCC-stable `.app` bundle, and it is the sole artifact named exactly "muni"
# — test/bench/example binaries live under target/*/deps with a hash suffix.
# Everything that is not the app execs directly, unchanged.
BIN_BASENAME="$(basename "${BINARY}")"
if [[ "${BIN_BASENAME}" != "muni" ]]; then
  exec "${BINARY}" "$@"
fi

IDENTITY="Muni Dev"
# Dev shell runs under its own bundle identifier so it does not collide with
# the installed prod app (`/Applications/Muni.app`, `com.muni.app`) on TCC
# grants, Application Support, Keychain, or Tauri-resolved data paths. The
# matching `MUNI_KEYCHAIN_SUFFIX=dev` env (passed to `open` below) makes
# secrets.rs resolve to the same `com.muni.app.dev` keychain service.
BUNDLE_ID="com.muni.app.dev"
BUNDLE_NAME="Muni-dev"
BUNDLE_VERSION="0.1.0-dev"
MIN_OS="14.0"

DEBUG_DIR="$(dirname "$BINARY")"
APP_DIR="$DEBUG_DIR/${BUNDLE_NAME}.app"
APP_CONTENTS="$APP_DIR/Contents"
APP_MACOS="$APP_CONTENTS/MacOS"
APP_RESOURCES="$APP_CONTENTS/Resources"
APP_BIN_NAME="$(basename "$BINARY")"
APP_BIN="$APP_MACOS/$APP_BIN_NAME"
APP_PLIST="$APP_CONTENTS/Info.plist"

# Source icon lives at src-tauri/icons/icon.icns, two levels up from this
# script (apps/desktop/src-tauri/scripts/dev-runner.sh → ../icons/icon.icns).
# Anchoring to the script's own directory keeps the runner robust against
# cargo's choice of CWD.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRC_ICON="$SCRIPT_DIR/../icons/icon.icns"

mkdir -p "$APP_MACOS" "$APP_RESOURCES"

# cargo only invokes the runner after a rebuild, so always overwrite.
cp -f "$BINARY" "$APP_BIN"

# Mirror the app icon into the dev bundle. Without CFBundleIconFile +
# Contents/Resources/icon.icns, LaunchServices falls back to the icon
# registered for `com.muni.app` (the installed /Applications/Muni.app, or
# the generic executable icon if none is installed) — and Cmd+Tab / the
# Dock show that fallback instead of the real brand mark.
if [[ -f "$SRC_ICON" ]]; then
  cp -f "$SRC_ICON" "$APP_RESOURCES/icon.icns"
else
  echo "dev-runner: WARN: $SRC_ICON missing; dev bundle will use the LS-fallback icon" >&2
fi

cat > "$APP_PLIST" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>${APP_BIN_NAME}</string>
    <key>CFBundleIconFile</key>
    <string>icon.icns</string>
    <key>CFBundleIdentifier</key>
    <string>${BUNDLE_ID}</string>
    <key>CFBundleName</key>
    <string>Muni-dev</string>
    <key>CFBundleDisplayName</key>
    <string>Muni-dev</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>${BUNDLE_VERSION}</string>
    <key>CFBundleVersion</key>
    <string>${BUNDLE_VERSION}</string>
    <key>LSMinimumSystemVersion</key>
    <string>${MIN_OS}</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSMicrophoneUsageDescription</key>
    <string>Muni records your microphone while you hold the dictation hotkey, so it can transcribe your speech.</string>
    <!--
        LSUIElement = true: run as a UIElement (accessory) app — no
        Dock icon, no Cmd+Tab entry, no menu bar. Match the production
        Info.plist. Without this, the dev bundle is a regular
        foreground app whose show() activates Muni and bounces the
        user out of any focused fullscreen Space (manual QA §16.1).
    -->
    <key>LSUIElement</key>
    <true/>
</dict>
</plist>
PLIST

if security find-identity -v -p codesigning 2>/dev/null | grep -q "${IDENTITY}"; then
  # --deep so the inner Mach-O is signed alongside the bundle.
  # Intentionally NOT passing --options runtime: Hardened Runtime makes macOS
  # demand a notarized signature for some TCC-gated APIs on Sonoma+, which a
  # self-signed local cert cannot satisfy. Production builds (`tauri build`)
  # still enable Hardened Runtime via tauri.conf.json.
  codesign \
    --force \
    --deep \
    --sign "${IDENTITY}" \
    --identifier "${BUNDLE_ID}" \
    "$APP_DIR" \
    >/dev/null 2>&1 || {
      echo "dev-runner: codesign failed for ${APP_DIR}; launching unsigned" >&2
    }
else
  echo "dev-runner: '${IDENTITY}' identity not found; launching unsigned" >&2
fi

# Launch via LaunchServices instead of direct exec.
#
# macOS Sequoia evaluates TCC grants against the "responsible process", which
# is the parent that spawned the binary. When `cargo run` execs the binary
# directly, the parent terminal (Ghostty / Terminal.app / iTerm) becomes
# responsible — so TCC checks the *terminal's* Input Monitoring grant and
# silently denies our bundle's grant. `open` routes through LaunchServices,
# which makes the launched bundle self-responsible. -W makes cargo wait on
# exit; -n forces a new instance.
#
# We tee logs to a file (instead of --stdout /dev/stdout, which fails with
# LSError -10810 when cargo's runner is invoked through pnpm pipes) and tail
# it in the background so the dev terminal still shows app logs.
APP_DIR_ABS="$(cd "$(dirname "$APP_DIR")" && pwd)/$(basename "$APP_DIR")"
LOG_FILE="${DEBUG_DIR}/muni-dev.log"
: > "${LOG_FILE}"

# Two distinct command-line shapes can survive a Ctrl+C'd dev session:
#   1. Bundled launches via `open -n -W ...Muni-dev.app/...`
#         → command line: <abs>/target/debug/Muni-dev.app/Contents/MacOS/muni
#   2. Direct `cargo run` invocations (no dev-runner involved)
#         → command line: <abs>/target/debug/muni
# A single pattern like "Muni-dev" only catches (1) — (2) survives every
# pkill, and accumulates across days into N concurrent muni processes all
# logging to the same file. Match both.
DEBUG_DIR_ABS="$(cd "${DEBUG_DIR}" && pwd)"
APP_BUNDLED_PATTERN="${APP_DIR_ABS}/Contents/MacOS/${APP_BIN_NAME}"
APP_RAW_PATTERN="${DEBUG_DIR_ABS}/${APP_BIN_NAME}"

# pgrep returns exit code 1 when there are no matches, which combined with
# `set -euo pipefail` would tear the script down inside a command
# substitution. `{ pgrep || true; }` absorbs that benign nonzero.
count_muni_processes() {
  {
    pgrep -f "${APP_BUNDLED_PATTERN}" 2>/dev/null || true
    pgrep -f "${APP_RAW_PATTERN}" 2>/dev/null || true
  } | sort -u | sed '/^$/d' | wc -l | tr -d ' '
}

kill_muni_processes() {
  local sig="$1"
  pkill -"${sig}" -f "${APP_BUNDLED_PATTERN}" 2>/dev/null || true
  pkill -"${sig}" -f "${APP_RAW_PATTERN}" 2>/dev/null || true
}

# Tear down any orphan from a prior dev session before we relaunch.
# Diagnostic prefix `[dev-runner pid=$$]` tags every line so we can tell when
# the runner is invoked more than once in parallel.
pre_count=$(count_muni_processes)
echo "[dev-runner pid=$$] startup; ${pre_count} existing muni process(es) (bundled+raw)" >&2
if [[ "${pre_count}" != "0" ]]; then
  kill_muni_processes TERM
  # SIGTERM is async — give processes a moment to actually exit before we
  # launch the replacement, otherwise `open -n` may race with the dying
  # processes' final logs landing in the shared log file.
  sleep 0.4 2>/dev/null || true
  remaining=$(count_muni_processes)
  if [[ "${remaining}" != "0" ]]; then
    echo "[dev-runner pid=$$] WARN: ${remaining} process(es) survived SIGTERM; sending SIGKILL" >&2
    kill_muni_processes KILL
    sleep 0.2 2>/dev/null || true
  fi
fi

# Kill orphan `tail -f muni-dev.log` processes from prior sessions.
#
# When a previous dev-runner.sh was killed without firing its EXIT trap
# (e.g. terminal force-quit, SIGKILL, parent crash), the backgrounded tail
# survived. Its stdout fd kept pointing at the user's terminal pty as long
# as that terminal stayed open. The next session's muni then writes one log
# line to muni-dev.log, every surviving tail re-prints it to the terminal,
# and the user sees N copies — even though only ONE muni process is alive.
# This is the actual root cause of the "duplicate logs" mystery.
#
# We use `tail -f.*muni-dev\.log` as the pattern (with the dot escaped so
# it doesn't match `muniXdevYlog` etc.). Our own tail hasn't been launched
# yet at this point, so we only hit orphans.
TAIL_ORPHAN_PATTERN='tail -f.*muni-dev\.log'
tail_orphans=$( { pgrep -f "${TAIL_ORPHAN_PATTERN}" 2>/dev/null || true; } | sort -u | sed '/^$/d' | wc -l | tr -d ' ')
if [[ "${tail_orphans}" != "0" ]]; then
  echo "[dev-runner pid=$$] killing ${tail_orphans} orphan tail process(es) of muni-dev.log" >&2
  pkill -KILL -f "${TAIL_ORPHAN_PATTERN}" 2>/dev/null || true
  sleep 0.1 2>/dev/null || true
fi

# Background tail follows the log into the dev terminal.
tail -f "${LOG_FILE}" &
TAIL_PID=$!

cleanup() {
  # SIGTERM both shapes (bundled + raw) so Drop impls flush remaining logs.
  kill_muni_processes TERM
  # Brief grace period for the .app to write its goodbye lines through tail
  # before we kill the tail itself.
  sleep 0.2 2>/dev/null || true
  kill ${TAIL_PID:-0} 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# `open` runs as a child (no `exec`) so the trap above can fire on Ctrl+C.
#
# We launch with `open -n` (NOT `-W`) and then poll for the launched
# PID ourselves. Why not `-W`? On Sequoia we observed
#   `Unable to block on application (GetProcessPID() returned <huge>)`
# every time the script ran, with `open` returning exit 0 *immediately*
# instead of blocking on the .app — the cleanup trap then fired and
# killed the just-launched bundle before any logs flushed. Root cause:
# LaunchServices has multiple registrations for `com.muni.app` (Tauri
# created entries for `~/Library/HTTPStorages/com.muni.app`,
# `~/Library/Logs/com.muni.app`, `~/Library/Application Support/com.muni.app`,
# `~/Library/WebKit/com.muni.app` — these are data dirs but `lsregister`
# indexed them as bundles). `open -W` resolves the launched PID by
# bundle id; with multiple registrations under the same id it returns
# `procNotFound` (-600 cast through uint64 is the giant number above).
#
# Polling `pgrep -f` against the absolute bundle path side-steps
# LaunchServices entirely. Same effect: we block until the launched
# process exits.
echo "[dev-runner pid=$$] launching ${APP_DIR_ABS}" >&2
# `--env MUNI_KEYCHAIN_SUFFIX=dev` makes `secrets.rs::keyring_service()`
# resolve to `com.muni.app.dev` so the dev shell and the installed prod
# app each have their own keychain items. Forced here (not via .env) so
# the dev identity stays guaranteed regardless of how env propagation
# behaves through pnpm/cargo/launchd.
if [[ "$#" -gt 0 ]]; then
  open -n \
    --env "MUNI_KEYCHAIN_SUFFIX=dev" \
    --stdout "${LOG_FILE}" \
    --stderr "${LOG_FILE}" \
    "${APP_DIR_ABS}" \
    --args "$@"
else
  open -n \
    --env "MUNI_KEYCHAIN_SUFFIX=dev" \
    --stdout "${LOG_FILE}" \
    --stderr "${LOG_FILE}" \
    "${APP_DIR_ABS}"
fi

# Find the PID `open` just launched. The bundle path is unique per
# build target (top-level vs deps/), so pgrep against
# `${APP_BUNDLED_PATTERN}` matches exactly one process — the one we
# care about waiting on. Allow up to ~3 s for LaunchServices to get
# through its launch dance before giving up.
LAUNCHED_PID=""
for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do
  LAUNCHED_PID=$(pgrep -f "${APP_BUNDLED_PATTERN}" | head -1)
  if [[ -n "${LAUNCHED_PID}" ]]; then
    break
  fi
  sleep 0.2 2>/dev/null || true
done

if [[ -z "${LAUNCHED_PID}" ]]; then
  echo "[dev-runner pid=$$] WARN: launched .app didn't appear in pgrep within 3s" >&2
  exit 1
fi

echo "[dev-runner pid=$$] watching pid ${LAUNCHED_PID}" >&2

# Block until the launched process exits. `kill -0` with a non-existent
# PID returns nonzero; that's our signal to stop waiting. We deliberately
# do NOT use `wait` — the launched process is detached (LaunchServices
# is its parent), not a shell child.
while kill -0 "${LAUNCHED_PID}" 2>/dev/null; do
  sleep 0.5 2>/dev/null || true
done

echo "[dev-runner pid=$$] pid ${LAUNCHED_PID} exited" >&2
exit 0
