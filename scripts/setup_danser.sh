#!/usr/bin/env bash
set -euo pipefail

TARGET_DIR="${1:-./data}"
mkdir -p "${TARGET_DIR}/Downloads" "${TARGET_DIR}/Replays" "${TARGET_DIR}/Skins" "${TARGET_DIR}/Songs" "${TARGET_DIR}/settings"

DANSER_EXEC="${TARGET_DIR}/danser"

echo "[danser-setup] Checking Danser installation in ${TARGET_DIR}..."

# 1. Download and extract Danser if not present
if [ ! -f "${DANSER_EXEC}" ]; then
    echo "[danser-setup] Danser binary not found in ${TARGET_DIR}. Fetching latest release..."
    
    LATEST_URL=""
    if command -v curl >/dev/null 2>&1; then
        LATEST_URL=$(curl -sL https://api.github.com/repos/Wieku/danser-go/releases/latest 2>/dev/null | grep -o 'https://[^"]*linux\.zip' | head -n 1 || true)
    fi

    if [ -z "${LATEST_URL}" ]; then
        echo "[danser-setup] Could not query GitHub API; falling back to release 0.12.1..."
        LATEST_URL="https://github.com/Wieku/danser-go/releases/download/0.12.1/danser-0.12.1-linux.zip"
    fi

    ZIP_FILE="${TARGET_DIR}/Downloads/danser-latest-linux.zip"
    echo "[danser-setup] Downloading from: ${LATEST_URL}"
    curl -fL -o "${ZIP_FILE}" "${LATEST_URL}"

    echo "[danser-setup] Extracting into ${TARGET_DIR}..."
    unzip -qo "${ZIP_FILE}" -d "${TARGET_DIR}"
    rm -f "${ZIP_FILE}"

    # Rename danser-cli to danser and danser to danser-gui
    if [ -f "${TARGET_DIR}/danser-cli" ] && [ -f "${TARGET_DIR}/danser" ]; then
        echo "[danser-setup] Setting up headless CLI binary (renaming danser-cli -> danser)..."
        mv "${TARGET_DIR}/danser" "${TARGET_DIR}/danser-gui"
        mv "${TARGET_DIR}/danser-cli" "${TARGET_DIR}/danser"
    fi

    chmod +x "${TARGET_DIR}/danser" 2>/dev/null || true
    [ -f "${TARGET_DIR}/danser-gui" ] && chmod +x "${TARGET_DIR}/danser-gui" 2>/dev/null || true
    echo "[danser-setup] Extraction complete."
else
    echo "[danser-setup] Danser binary already present."
fi

# 2. Run first-time initialization to generate settings if missing
SETTINGS_FILE="${TARGET_DIR}/settings/default.json"
CREDENTIALS_FILE="${TARGET_DIR}/settings/credentials.json"

if [ ! -f "${SETTINGS_FILE}" ]; then
    echo "[danser-setup] Generating default Danser settings..."
    (
        cd "${TARGET_DIR}"
        export LD_LIBRARY_PATH="${TARGET_DIR}${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
        if [ -z "${DISPLAY:-}" ] && command -v xvfb-run >/dev/null 2>&1; then
            xvfb-run -a -s "-screen 0 1920x1080x24" ./danser -noupdatecheck -settings=default >/dev/null 2>&1 || true
        else
            ./danser -noupdatecheck -settings=default >/dev/null 2>&1 || true
        fi
    )

    if [ -f "${SETTINGS_FILE}" ]; then
        echo "[danser-setup] Configuring paths in ${SETTINGS_FILE}..."
        # Set absolute paths for Songs, Skins, Replays, and OutputDir
        ABS_DIR="$(cd "${TARGET_DIR}" && pwd)"
        SAFE_ABS_DIR=${ABS_DIR//\\/\\\\}
        SAFE_ABS_DIR=${SAFE_ABS_DIR//&/\\&}
        SAFE_ABS_DIR=${SAFE_ABS_DIR//|/\\|}
        SAFE_ABS_DIR=${SAFE_ABS_DIR//\"/\\\"}
        sed -i 's|"OsuSongsDir":.*|"OsuSongsDir": "'"${SAFE_ABS_DIR}"'/Songs",|' "${SETTINGS_FILE}" 2>/dev/null || true
        sed -i 's|"OsuSkinsDir":.*|"OsuSkinsDir": "'"${SAFE_ABS_DIR}"'/Skins",|' "${SETTINGS_FILE}" 2>/dev/null || true
        sed -i 's|"OsuReplaysDir":.*|"OsuReplaysDir": "'"${SAFE_ABS_DIR}"'/Replays",|' "${SETTINGS_FILE}" 2>/dev/null || true
        sed -i 's|"OutputDir":.*|"OutputDir": "'"${SAFE_ABS_DIR}"'/Replays",|' "${SETTINGS_FILE}" 2>/dev/null || true
    fi
    echo "[danser-setup] Default settings generated."
fi

# 3. Pre-populate credentials.json if osu! credentials are in environment
if [ -n "${OSU_CLIENT_ID:-}" ] && [ -n "${OSU_CLIENT_SECRET:-}" ]; then
    if [ ! -f "${CREDENTIALS_FILE}" ] || [ ! -s "${CREDENTIALS_FILE}" ]; then
        echo "[danser-setup] Populating settings/credentials.json from environment variables..."
        esc_id=${OSU_CLIENT_ID//\\/\\\\}
        esc_id=${esc_id//\"/\\\"}
        esc_secret=${OSU_CLIENT_SECRET//\\/\\\\}
        esc_secret=${esc_secret//\"/\\\"}
        (
            umask 077
            printf '{\n    "ClientId": "%s",\n    "ClientSecret": "%s"\n}\n' "${esc_id}" "${esc_secret}" > "${CREDENTIALS_FILE}"
        )
        chmod 600 "${CREDENTIALS_FILE}"
    fi
fi

echo "[danser-setup] Danser is ready."
