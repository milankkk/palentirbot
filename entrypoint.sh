#!/usr/bin/env bash
set -e

DATA_DIR="${DANSER_PATH:-/app/data}"
mkdir -p "${DATA_DIR}/Downloads" "${DATA_DIR}/Replays" "${DATA_DIR}/Skins" "${DATA_DIR}/Songs" "${DATA_DIR}/settings" /app/logs

DANSER_EXEC="${DATA_DIR}/danser"

# 1. Download and extract Danser if not present
if [ ! -f "${DANSER_EXEC}" ]; then
    echo "[palentirbot] Danser binary not found in ${DATA_DIR}. Downloading latest release..."
    LATEST_URL=$(curl -sL https://api.github.com/repos/Wieku/danser-go/releases/latest 2>/dev/null | grep -o 'https://[^"]*linux\.zip' | head -n 1 || true)
    if [ -z "${LATEST_URL}" ]; then
        LATEST_URL="https://github.com/Wieku/danser-go/releases/download/0.12.1/danser-0.12.1-linux.zip"
    fi
    ZIP_FILE="/tmp/danser-latest.zip"
    echo "[palentirbot] Downloading from ${LATEST_URL}..."
    curl -fL -o "${ZIP_FILE}" "${LATEST_URL}"
    echo "[palentirbot] Extracting into ${DATA_DIR}..."
    unzip -qo "${ZIP_FILE}" -d "${DATA_DIR}"
    rm -f "${ZIP_FILE}"

    # Rename danser-cli to danser and danser to danser-gui
    if [ -f "${DATA_DIR}/danser-cli" ] && [ -f "${DATA_DIR}/danser" ]; then
        echo "[palentirbot] Setting up headless CLI binary (renaming danser-cli -> danser)..."
        mv "${DATA_DIR}/danser" "${DATA_DIR}/danser-gui"
        mv "${DATA_DIR}/danser-cli" "${DATA_DIR}/danser"
    fi

    chmod +x "${DATA_DIR}/danser" 2>/dev/null || true
    [ -f "${DATA_DIR}/danser-gui" ] && chmod +x "${DATA_DIR}/danser-gui" 2>/dev/null || true
    echo "[palentirbot] Danser download and extraction complete."
fi

# 2. Run first-time initialization to generate settings if missing
SETTINGS_FILE="${DATA_DIR}/settings/default.json"
if [ ! -f "${SETTINGS_FILE}" ]; then
    echo "[palentirbot] Generating default Danser settings..."
    (
        cd "${DATA_DIR}"
        export LD_LIBRARY_PATH="${DATA_DIR}:${LD_LIBRARY_PATH:-}"
        ./danser -noupdatecheck >/dev/null 2>&1 || true
    )
    if [ -f "${SETTINGS_FILE}" ]; then
        sed -i 's|"OsuSongsDir":.*|"OsuSongsDir": "'"${DATA_DIR}"'/Songs",|' "${SETTINGS_FILE}" 2>/dev/null || true
        sed -i 's|"OsuSkinsDir":.*|"OsuSkinsDir": "'"${DATA_DIR}"'/Skins",|' "${SETTINGS_FILE}" 2>/dev/null || true
        sed -i 's|"OsuReplaysDir":.*|"OsuReplaysDir": "'"${DATA_DIR}"'/Replays",|' "${SETTINGS_FILE}" 2>/dev/null || true
    fi
    echo "[palentirbot] Default settings generated."
fi

# 3. Pre-populate credentials.json if osu! credentials are provided
CREDENTIALS_FILE="${DATA_DIR}/settings/credentials.json"
if [ -n "${OSU_CLIENT_ID:-}" ] && [ -n "${OSU_CLIENT_SECRET:-}" ]; then
    if [ ! -f "${CREDENTIALS_FILE}" ] || [ ! -s "${CREDENTIALS_FILE}" ]; then
        echo "[palentirbot] Pre-configuring settings/credentials.json from environment variables..."
        cat <<EOF > "${CREDENTIALS_FILE}"
{
    "ClientId": "${OSU_CLIENT_ID}",
    "ClientSecret": "${OSU_CLIENT_SECRET}"
}
EOF
    fi
fi

# 4. Start the bot
echo "[palentirbot] Starting Discord Bot..."
exec xvfb-run -a -s "-screen 0 1920x1080x24" /app/palentirbot "$@"
