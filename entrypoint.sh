#!/usr/bin/env bash
set -euo pipefail

DATA_DIR="${DANSER_PATH:-/app/data}"
SAFE_DATA_DIR=${DATA_DIR//\\/\\\\}
SAFE_DATA_DIR=${SAFE_DATA_DIR//&/\\&}
SAFE_DATA_DIR=${SAFE_DATA_DIR//|/\\|}
SAFE_DATA_DIR=${SAFE_DATA_DIR//\"/\\\"}
mkdir -p "${DATA_DIR}/Downloads" "${DATA_DIR}/Replays" "${DATA_DIR}/Skins" "${DATA_DIR}/Songs" "${DATA_DIR}/settings" /app/logs

DANSER_EXEC="${DATA_DIR}/danser"

# 1. Download and extract Danser if not present
if [ ! -f "${DANSER_EXEC}" ]; then
    echo "[palentirbot] Danser binary not found in ${DATA_DIR}. Downloading latest release..."
    LATEST_URL=$(curl -fsSL https://api.github.com/repos/Wieku/danser-go/releases/latest 2>/dev/null | grep -o 'https://[^"]*linux\.zip' | head -n 1 || true)
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

patch_danser_settings() {
    local file="$1"
    local dir="$2"
    if command -v jq >/dev/null 2>&1; then
        local tmp="${file}.tmp"
        if jq --arg songs "${dir}/Songs" \
              --arg skins "${dir}/Skins" \
              --arg replays "${dir}/Replays" \
              '.General.OsuSongsDir = $songs
               | .General.OsuSkinsDir = $skins
               | .General.OsuReplaysDir = $replays
               | .Recording.OutputDir = $replays' "$file" > "$tmp" 2>/dev/null; then
            mv "$tmp" "$file"
            return 0
        fi
        rm -f "$tmp"
    fi
    sed -i 's|"OsuSongsDir":.*|"OsuSongsDir": "'"${SAFE_DATA_DIR}"'/Songs",|' "$file" 2>/dev/null || true
    sed -i 's|"OsuSkinsDir":.*|"OsuSkinsDir": "'"${SAFE_DATA_DIR}"'/Skins",|' "$file" 2>/dev/null || true
    sed -i 's|"OsuReplaysDir":.*|"OsuReplaysDir": "'"${SAFE_DATA_DIR}"'/Replays",|' "$file" 2>/dev/null || true
    sed -i 's|"OutputDir":.*|"OutputDir": "'"${SAFE_DATA_DIR}"'/Replays",|' "$file" 2>/dev/null || true
}

# 2. Run first-time initialization to generate settings if missing
SETTINGS_FILE="${DATA_DIR}/settings/default.json"
if [ ! -f "${SETTINGS_FILE}" ]; then
    echo "[palentirbot] Generating default Danser settings..."
    (
        cd "${DATA_DIR}"
        export LD_LIBRARY_PATH="${DATA_DIR}${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
        xvfb-run -a -s "-screen 0 1920x1080x24" ./danser -noupdatecheck -settings=default >/dev/null 2>&1 || true
    )
    echo "[palentirbot] Default settings generated."
fi

# Always ensure Danser paths point inside DATA_DIR even if default.json was created on host
if [ -f "${SETTINGS_FILE}" ]; then
    patch_danser_settings "${SETTINGS_FILE}" "${DATA_DIR}"
fi

# 3. Create default prompt.txt inside data directory if missing
PROMPT_FILE="${DATA_DIR}/prompt.txt"
if [ ! -f "${PROMPT_FILE}" ]; then
    echo "[palentirbot] Creating default ${PROMPT_FILE}..."
    if [ -f "/app/prompt.example.txt" ]; then
        cp /app/prompt.example.txt "${PROMPT_FILE}"
    else
        printf "You are Render Slave, a helpful osu! Discord bot assistant.\nCurrent requester Discord username: {username}\n" > "${PROMPT_FILE}"
    fi
fi

# 4. Pre-populate credentials.json if osu! credentials are provided
CREDENTIALS_FILE="${DATA_DIR}/settings/credentials.json"
if [ -n "${OSU_CLIENT_ID:-}" ] && [ -n "${OSU_CLIENT_SECRET:-}" ]; then
    if [ ! -f "${CREDENTIALS_FILE}" ] || [ ! -s "${CREDENTIALS_FILE}" ]; then
        echo "[palentirbot] Pre-configuring settings/credentials.json from environment variables..."
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

# 5. Start headless Xvfb display and hand PID-child directly to palentirbot (under tini)
export DISPLAY=":99"
Xvfb "${DISPLAY}" -screen 0 1920x1080x24 -nolisten tcp >/dev/null 2>&1 &
XVFB_PID=$!
for _ in $(seq 1 30); do
    [ -S /tmp/.X11-unix/X99 ] && break
    kill -0 "${XVFB_PID}" 2>/dev/null || break
    sleep 0.1
done

echo "[palentirbot] Starting Discord Bot..."
exec /app/palentirbot "$@"
