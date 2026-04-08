# upload.py
from flask import Flask, request, jsonify
import os, uuid

app = Flask(__name__)
UPLOAD_DIR = "" # directory to store rendered replays
SECRET = "" # secret for UPLOAD_SECRET
BASE_URL = ""  #url to domain/ static fileserver like caddy or nginx

@app.route("/upload", methods=["POST"])
def upload():
    if request.form.get("secret") != SECRET:
        return jsonify({"error": 401, "text": "Unauthorized"}), 401
    
    f = request.files["video"]
    filename = f"{uuid.uuid4()}_{f.filename}"
    f.save(os.path.join(UPLOAD_DIR, filename))
    
    url = f"{BASE_URL}/{filename}"
    return jsonify({"error": 0, "text": url})


app.run(port=5555) # port to run at
