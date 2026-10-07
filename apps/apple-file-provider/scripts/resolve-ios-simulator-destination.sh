#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
    echo "usage: $0 <project-path> <scheme>" >&2
    exit 64
fi

PROJECT_PATH="$1"
SCHEME="$2"

extract_destination() {
    PATTERN="$1"
    # Restrict matching to destinations that Xcode declared available. It can
    # otherwise list unavailable simulators in a similarly shaped section.
    printf '%s\n' "$AVAILABLE_DESTINATIONS" \
        | sed -nE "s/^[[:space:]]*\\{ platform:iOS Simulator,.* id:([^,]+),.* name:(${PATTERN})[[:space:]]*\\}\$/platform=iOS Simulator,id=\\1/p" \
        | grep -v 'id=dvtdevice-' \
        | head -n 1
}

available_destinations() {
    printf '%s\n' "$DESTINATIONS" \
        | awk '
            /^[[:space:]]*Available destinations for the / { available = 1; next }
            /^[[:space:]]*Ineligible destinations for the / { available = 0 }
            available && /^[[:space:]]*$/ { exit }
            available && /^[[:space:]]*\{ platform:iOS Simulator,/ { print }
        '
}

ATTEMPT=1
MAX_ATTEMPTS=5
while [ "$ATTEMPT" -le "$MAX_ATTEMPTS" ]; do
    if DESTINATIONS="$(xcodebuild -project "$PROJECT_PATH" -scheme "$SCHEME" -showdestinations 2>&1)"; then
        AVAILABLE_DESTINATIONS="$(available_destinations)"
        DESTINATION="$(extract_destination "iPhone[^,}]*")"
        if [ -z "$DESTINATION" ]; then
            DESTINATION="$(extract_destination "[^,}]*")"
        fi
    else
        DESTINATION=""
    fi

    if [ -n "$DESTINATION" ]; then
        printf '%s\n' "$DESTINATION"
        exit 0
    fi

    if [ "$ATTEMPT" -lt "$MAX_ATTEMPTS" ]; then
        sleep 1
    fi
    ATTEMPT=$((ATTEMPT + 1))
done

echo "failed to resolve an available iOS Simulator destination for scheme $SCHEME" >&2
printf '%s\n' "$DESTINATIONS" >&2
exit 69
