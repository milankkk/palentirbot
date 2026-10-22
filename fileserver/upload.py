# upload.py
from flask import Flask, request, jsonify
from werkzeug.utils import secure_filename
import hmac, os, uuid

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
                    v = v.strip().strip("'\"")
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
PORT = int(os.environ.get("UPLOAD_PORT") or os.environ.get("PORT") or 5555)

os.makedirs(UPLOAD_DIR, exist_ok=True)

@app.route("/upload", methods=["POST"])
def upload():
    secret_val = request.form.get("secret", "")
    if not SECRET or not hmac.compare_digest(secret_val, SECRET):
        return jsonify({"error": 401, "text": "Unauthorized"}), 401

    if "video" not in request.files:
        return jsonify({"error": 400, "text": "Missing video file"}), 400

    f = request.files["video"]
    if not f.filename.lower().endswith(".mp4"):
        return jsonify({"error": 400, "text": "Only .mp4 video files are accepted"}), 400

    safe_name = secure_filename(f.filename) or "replay.mp4"
    filename = f"{uuid.uuid4()}_{safe_name}"
    f.save(os.path.join(UPLOAD_DIR, filename))

    watch_base = BASE_URL if BASE_URL.endswith("/watch") else f"{BASE_URL}/watch"
    url = f"{watch_base}/{filename}"
    return jsonify({"error": 0, "text": url})

if __name__ == "__main__":
    app.run(host="0.0.0.0", port=PORT)
