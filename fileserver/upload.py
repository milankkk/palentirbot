# upload.py
from flask import Flask, request, jsonify
from werkzeug.utils import secure_filename
import hmac, os, uuid

app = Flask(__name__)
UPLOAD_DIR = "" # directory to store rendered replays
SECRET = "" # secret for UPLOAD_SECRET
BASE_URL = ""  # url to domain/ static fileserver like caddy or nginx

@app.route("/upload", methods=["POST"])
def upload():
    secret_val = request.form.get("secret", "")
    if not hmac.compare_digest(secret_val, SECRET):
        return jsonify({"error": 401, "text": "Unauthorized"}), 401

    if "video" not in request.files:
        return jsonify({"error": 400, "text": "Missing video file"}), 400

    f = request.files["video"]
    safe_name = secure_filename(f.filename) or "video.mp4"
    filename = f"{uuid.uuid4()}_{safe_name}"
    f.save(os.path.join(UPLOAD_DIR, filename))

    url = f"{BASE_URL}/{filename}"
    return jsonify({"error": 0, "text": url})


app.run(port=5555) # port to run at
