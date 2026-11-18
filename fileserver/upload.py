# upload.py
from flask import Flask, request, jsonify
from werkzeug.utils import secure_filename
import hmac, json, os, shutil, subprocess, time, uuid

# Automatically link with .env in repository root if present
def _load_env_file():
    env_path = os.path.join(os.path.dirname(__file__), "..", ".env")
    if os.path.isfile(env_path):
        try:
            with open(env_path, "r", encoding="utf-8") as f:
                for line in f:
                    line = line.strip()
                    if not line or line.startswith("#") or "=" not in line:
                        continue
                    k, v = line.split("=", 1)
                    k = k.strip()
                    v = v.strip()
                    if len(v) >= 2 and (
                        (v[0] == '"' and v[-1] == '"') or (v[0] == "'" and v[-1] == "'")
                    ):
                        v = v[1:-1]
                    elif "#" in v:
                        v = v.split("#", 1)[0].strip()
                    if k and k not in os.environ:
                        os.environ[k] = v
        except Exception:
            pass

_load_env_file()

app = Flask(__name__)
app.config["MAX_CONTENT_LENGTH"] = 500 * 1024 * 1024  # 500 MB upload limit

# Linked defaults matching bot configuration
UPLOAD_DIR = os.environ.get("UPLOAD_DIR", "replays")
SECRET = os.environ.get("UPLOAD_SECRET", "palentir_replay_secret_key")
BASE_URL = (os.environ.get("PUBLIC_REPLAY_BASE_URL") or os.environ.get("BASE_URL", "http://localhost:8085")).rstrip("/")
HOST = os.environ.get("UPLOAD_HOST", "0.0.0.0")
PORT = int(os.environ.get("UPLOAD_PORT") or os.environ.get("PORT") or 5555)

if SECRET == "palentir_replay_secret_key":
    import sys
    print("WARNING: UPLOAD_SECRET is using the default fallback value. Set a strong secret in .env for production!", file=sys.stderr)

os.makedirs(UPLOAD_DIR, exist_ok=True)


def _cleanup_file(path):
    if path and os.path.exists(path):
        try:
            os.remove(path)
        except OSError:
            pass


def _probe_video(video_path):
    ffprobe = shutil.which("ffprobe")
    if not ffprobe:
        return None, None, None
    try:
        proc = subprocess.run(
            [ffprobe, "-v", "error", "-select_streams", "v:0",
             "-show_entries", "stream=width,height:format=duration",
             "-of", "json", video_path],
            check=True, capture_output=True, timeout=15, text=True,
        )
        info = json.loads(proc.stdout or "{}")
        streams = info.get("streams") or []
        s0 = streams[0] if streams else {}
        fmt = info.get("format") or {}
        width = int(s0["width"]) if s0.get("width") else None
        height = int(s0["height"]) if s0.get("height") else None
        duration = float(fmt["duration"]) if fmt.get("duration") else None
        return width, height, duration
    except (subprocess.SubprocessError, ValueError, KeyError, OSError, json.JSONDecodeError):
        return None, None, None


def _generate_thumbnail(video_path, thumb_path, stem, duration_secs):
    ffmpeg = shutil.which("ffmpeg")
    if not ffmpeg:
        return False
    seek = min(5.0, max(0.2, duration_secs * 0.35)) if (duration_secs and duration_secs > 0) else 5.0
    if duration_secs and seek >= duration_secs:
        seek = max(0.0, duration_secs * 0.5)
    tmp_thumb = os.path.join(UPLOAD_DIR, f".{stem}.tmp.jpg")
    try:
        subprocess.run(
            [ffmpeg, "-y", "-ss", f"{seek:.2f}", "-i", video_path,
             "-frames:v", "1", "-vf", "scale=1280:-2", "-q:v", "3", tmp_thumb],
            check=True, timeout=60, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        os.chmod(tmp_thumb, 0o644)
        os.replace(tmp_thumb, thumb_path)
        return True
    except (subprocess.SubprocessError, OSError):
        _cleanup_file(tmp_thumb)
        return False


@app.route("/health", methods=["GET"])
def health():
    return jsonify({"ok": True}), 200


@app.route("/upload", methods=["POST"])
def upload():
    header_secret = request.headers.get("X-Upload-Secret")
    secret_val = header_secret if header_secret is not None else request.form.get("secret", "")
    if not SECRET or not hmac.compare_digest(
        secret_val.encode("utf-8"), SECRET.encode("utf-8")
    ):
        return jsonify({"error": 401, "text": "Unauthorized"}), 401

    if "video" not in request.files:
        return jsonify({"error": 400, "text": "Missing video file"}), 400

    f = request.files["video"]
    if not f.filename or not f.filename.lower().endswith(".mp4"):
        return jsonify({"error": 400, "text": "Only .mp4 video files are accepted"}), 400

    safe_name = secure_filename(f.filename)
    if not safe_name or not safe_name.lower().endswith(".mp4"):
        return jsonify({"error": 400, "text": "Invalid .mp4 filename"}), 400

    safe_stem, _ = os.path.splitext(safe_name)
    safe_stem = safe_stem[:120].strip("._-") or "replay"
    filename = f"{uuid.uuid4()}_{safe_stem}.mp4"
    final_path = os.path.join(UPLOAD_DIR, filename)
    tmp_path = os.path.join(UPLOAD_DIR, f".{filename}.tmp")
    try:
        f.save(tmp_path)
        os.chmod(tmp_path, 0o644)
        os.replace(tmp_path, final_path)
    except Exception:
        _cleanup_file(tmp_path)
        raise

    stem, _ = os.path.splitext(filename)
    width, height, duration = _probe_video(final_path)
    has_thumb = _generate_thumbnail(final_path, os.path.join(UPLOAD_DIR, f"{stem}.jpg"), stem, duration)

    watch_base = BASE_URL if BASE_URL.endswith("/watch") else f"{BASE_URL}/watch"
    url = f"{watch_base}/{stem}"
    video_url = f"{BASE_URL}/{stem}.mp4"

    meta_path = os.path.join(UPLOAD_DIR, f"{stem}.json")
    tmp_meta = os.path.join(UPLOAD_DIR, f".{stem}.tmp.json")
    try:
        meta = {
            "filename": filename,
            "title": (request.form.get("title") or "").strip()[:300] or stem,
            "author": (request.form.get("author") or "").strip()[:64],
            "beatmap": (request.form.get("beatmap") or "").strip()[:300],
            "hash": (request.form.get("hash") or "").strip()[:128],
            "width": width,
            "height": height,
            "duration": round(duration, 2) if duration is not None else None,
            "size_bytes": os.path.getsize(final_path),
            "uploaded_at": int(time.time()),
            "watch_url": url,
            "video_url": video_url,
        }
        with open(tmp_meta, "w", encoding="utf-8") as mf:
            json.dump(meta, mf, ensure_ascii=False, separators=(",", ":"))
        os.chmod(tmp_meta, 0o644)
        os.replace(tmp_meta, meta_path)
    except OSError:
        _cleanup_file(tmp_meta)

    return jsonify({
        "error": 0,
        "text": url,
        "video_url": video_url,
        "thumbnail_url": f"{BASE_URL}/{stem}.jpg" if has_thumb else None,
        "width": width,
        "height": height,
        "duration": round(duration, 2) if duration is not None else None,
    })

if __name__ == "__main__":
    app.run(host=HOST, port=PORT, threaded=True)
