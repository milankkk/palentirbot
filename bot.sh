#!/usr/bin/env bash
# Manage palentirbot & replay fileserver (again.py) in detached GNU screen sessions
# with built-in auto-restart-on-crash loops and HTTP/Discord-gateway health checks.
#
# Usage:
#   ./bot.sh              # Start any crashed/stopped screen session (safe if already running)
#   ./bot.sh start        # Same as default: ensure both screens are running & healthy
#   ./bot.sh restart      # Cleanly restart both serve & palentirbot on detached screens
#   ./bot.sh restart bot  # Restart only palentirbot
#   ./bot.sh restart serve# Restart only the replay fileserver (again.py)
#   ./bot.sh stop         # Cleanly stop both detached screens
#   ./bot.sh status       # Show screen sessions, PIDs, and live health/queue status
#   ./bot.sh watchdog     # Non-interactive health check (used by cron every 2 minutes)

set -uo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "$(readlink -f -- "${BASH_SOURCE[0]}")")" && pwd)"
BOT_DIR="${BOT_DIR:-${SCRIPT_DIR}}"
BOT_BIN="${BOT_BIN:-${BOT_DIR}/target/release/palentirbot}"
SERVE_DIR="${SERVE_DIR:-$(cd -- "${BOT_DIR}/.." && pwd)/fileserver}"
SERVE_SCRIPT="${SERVE_SCRIPT:-${SERVE_DIR}/again.py}"

BOT_SCREEN="palentirbot"
SERVE_SCREEN="serve"

BOT_STOP_FLAG="/tmp/palentirbot_stop_${UID}.flag"
SERVE_STOP_FLAG="/tmp/palentir_serve_stop_${UID}.flag"
LOCK_FILE="/tmp/palentirbot_ctl_${UID}.lock"

export PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/home/linuxbrew/.linuxbrew/bin:${PATH:-}"

has_screen() {
    local name="$1"
    screen -ls 2>/dev/null | grep -qE "[0-9]+\.${name}[[:space:]]"
}

serve_healthy() {
    curl -fsS --max-time 4 "http://127.0.0.1:5555/health" >/dev/null 2>&1
}

bot_healthy() {
    curl -fsS --max-time 4 "http://127.0.0.1:5556/api/web-render/health" >/dev/null 2>&1
}

bot_is_rendering() {
    curl -sS --max-time 3 "http://127.0.0.1:5556/api/web-render/queue" 2>/dev/null | grep -q '"status":"rendering"'
}

retire_legacy_systemd_units() {
    # Ensure user systemd units don't fight the screen sessions for ports 5555/5556
    if command -v systemctl >/dev/null 2>&1; then
        systemctl --user disable --now palentirbot-watchdog.timer palentirbot.service serve.service >/dev/null 2>&1 || true
    fi
}

stop_serve() {
    touch "$SERVE_STOP_FLAG"
    pkill -TERM -f "^(/usr/bin/)?python3 (${SERVE_SCRIPT}|again\.py)$" 2>/dev/null || true
    for _ in $(seq 1 15); do
        if ! pgrep -f "^(/usr/bin/)?python3 (${SERVE_SCRIPT}|again\.py)$" >/dev/null 2>&1; then
            break
        fi
        sleep 0.3
    done
    screen -S "$SERVE_SCREEN" -X quit >/dev/null 2>&1 || true
    pkill -KILL -f "^(/usr/bin/)?python3 (${SERVE_SCRIPT}|again\.py)$" 2>/dev/null || true
    rm -f "$SERVE_STOP_FLAG"
}

stop_bot() {
    touch "$BOT_STOP_FLAG"
    pkill -TERM -x "palentirbot" 2>/dev/null || true
    for _ in $(seq 1 25); do
        if ! pgrep -x "palentirbot" >/dev/null 2>&1; then
            break
        fi
        sleep 0.4
    done
    screen -S "$BOT_SCREEN" -X quit >/dev/null 2>&1 || true
    pkill -KILL -x "palentirbot" 2>/dev/null || true
    rm -f "$BOT_STOP_FLAG"
}

start_serve() {
    rm -f "$SERVE_STOP_FLAG"
    if has_screen "$SERVE_SCREEN" && serve_healthy; then
        echo "[serve] Already running on screen '${SERVE_SCREEN}' and healthy (127.0.0.1:5555)."
        return 0
    fi

    stop_serve
    echo "[serve] Starting detached screen '${SERVE_SCREEN}'..."
    screen -dmS "$SERVE_SCREEN" /bin/bash -c "
        cd '${SERVE_DIR}' || exit 1
        export PYTHONUNBUFFERED=1
        export PATH='${PATH}'
        while [ ! -f '${SERVE_STOP_FLAG}' ]; do
            echo \"[\$(date '+%F %T')] Starting again.py on 127.0.0.1:5555...\"
            /usr/bin/python3 '${SERVE_SCRIPT}'
            code=\$?
            if [ -f '${SERVE_STOP_FLAG}' ]; then
                break
            fi
            echo \"[\$(date '+%F %T')] again.py exited (code \${code}); restarting in 3s...\"
            sleep 3
        done
    "

    for _ in $(seq 1 20); do
        if serve_healthy; then
            echo "[serve] Ready -> $(curl -fsS --max-time 2 http://127.0.0.1:5555/health)"
            return 0
        fi
        sleep 0.5
    done
    echo "[serve] WARNING: Started screen '${SERVE_SCREEN}', but 127.0.0.1:5555/health did not respond yet. Check: screen -r ${SERVE_SCREEN}"
    return 1
}

start_bot() {
    rm -f "$BOT_STOP_FLAG"
    if [ ! -x "$BOT_BIN" ]; then
        echo "[palentirbot] ERROR: Binary not found at ${BOT_BIN}. Run 'cargo build --release' first."
        return 1
    fi

    if has_screen "$BOT_SCREEN" && bot_healthy; then
        echo "[palentirbot] Already running on screen '${BOT_SCREEN}' and healthy (127.0.0.1:5556)."
        return 0
    fi

    stop_bot
    echo "[palentirbot] Starting detached screen '${BOT_SCREEN}'..."
    screen -dmS "$BOT_SCREEN" /bin/bash -c "
        cd '${BOT_DIR}' || exit 1
        export PATH='${PATH}'
        while [ ! -f '${BOT_STOP_FLAG}' ]; do
            echo \"[\$(date '+%F %T')] Starting palentirbot...\"
            '${BOT_BIN}'
            code=\$?
            if [ -f '${BOT_STOP_FLAG}' ]; then
                break
            fi
            echo \"[\$(date '+%F %T')] palentirbot exited (code \${code}); restarting in 5s...\"
            sleep 5
        done
    "

    for _ in $(seq 1 30); do
        if bot_healthy; then
            echo "[palentirbot] Ready -> $(curl -fsS --max-time 2 http://127.0.0.1:5556/api/web-render/health)"
            return 0
        fi
        sleep 0.5
    done
    echo "[palentirbot] WARNING: Started screen '${BOT_SCREEN}', but 127.0.0.1:5556 did not respond yet. Check: screen -r ${BOT_SCREEN}"
    return 1
}

show_status() {
    echo "=== Detached Screen Sessions ==="
    screen -ls 2>/dev/null | grep -E "${BOT_SCREEN}|${SERVE_SCREEN}" || echo "  (no palentirbot/serve screens running)"
    echo ""
    echo "=== Live Health ==="
    echo -n "  serve       (127.0.0.1:5555): "
    curl -fsS --max-time 3 "http://127.0.0.1:5555/health" 2>/dev/null || echo "DOWN"
    echo ""
    echo -n "  palentirbot (127.0.0.1:5556): "
    curl -fsS --max-time 3 "http://127.0.0.1:5556/api/web-render/health" 2>/dev/null || echo "DOWN"
    echo ""
    echo -n "  render queue                : "
    curl -fsS --max-time 3 "http://127.0.0.1:5556/api/web-render/queue" 2>/dev/null || echo "UNAVAILABLE"
    echo ""
    echo ""
    echo "Attach logs:  screen -r ${BOT_SCREEN}   |   screen -r ${SERVE_SCREEN}   (detach: Ctrl+A then D)"
}

run_watchdog() {
    # Retry 3x before restarting in case of a momentary spike
    local ok_serve=0
    for _ in 1 2 3; do
        if has_screen "$SERVE_SCREEN" && serve_healthy; then ok_serve=1; break; fi
        sleep 2
    done
    if [ "$ok_serve" -eq 0 ]; then
        logger -t palentir-watchdog "serve screen missing or unhealthy; recovering via bot.sh"
        start_serve >/dev/null 2>&1
    fi

    local ok_bot=0
    for _ in 1 2 3; do
        if has_screen "$BOT_SCREEN" && bot_healthy; then ok_bot=1; break; fi
        sleep 3
    done
    if [ "$ok_bot" -eq 0 ]; then
        if has_screen "$BOT_SCREEN" && bot_is_rendering; then
            logger -t palentir-watchdog "palentirbot gateway stale during active GPU render; deferring restart"
            return 0
        fi
        logger -t palentir-watchdog "palentirbot screen missing or unhealthy; recovering via bot.sh"
        start_bot >/dev/null 2>&1
    fi
}

ACTION="${1:-start}"
TARGET="${2:-all}"

if [ "$ACTION" = "status" ]; then
    show_status
    exit 0
fi

exec 9>"$LOCK_FILE"
if [ "$ACTION" = "watchdog" ]; then
    flock -n 9 || exit 0
else
    if ! flock -w 30 9; then
        echo "Another bot.sh operation is currently in progress."
        exit 1
    fi
fi

case "$ACTION" in
    start|ensure)
        retire_legacy_systemd_units
        case "$TARGET" in
            serve) start_serve ;;
            bot|palentirbot) start_bot ;;
            *) start_serve; start_bot ;;
        esac
        ;;
    restart)
        retire_legacy_systemd_units
        case "$TARGET" in
            serve) stop_serve; start_serve ;;
            bot|palentirbot) stop_bot; start_bot ;;
            *) stop_bot; stop_serve; start_serve; start_bot ;;
        esac
        ;;
    stop)
        retire_legacy_systemd_units
        case "$TARGET" in
            serve) stop_serve; echo "[serve] Stopped." ;;
            bot|palentirbot) stop_bot; echo "[palentirbot] Stopped." ;;
            *) stop_bot; stop_serve; echo "Stopped both '${BOT_SCREEN}' and '${SERVE_SCREEN}' screens." ;;
        esac
        ;;
    watchdog)
        run_watchdog
        ;;
    *)
        echo "Usage: $0 {start|restart|stop|status|watchdog} [all|bot|serve]"
        exit 1
        ;;
esac
