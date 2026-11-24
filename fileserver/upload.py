import argparse
import hashlib
import hmac
import json
import math
import os
import re
import shutil
import subprocess
import threading
import time
import urllib.parse
import uuid

from flask import Flask, jsonify, request
from werkzeug.utils import secure_filename


def _load_env_file():
    candidates = [
        os.path.join(os.path.dirname(os.path.abspath(__file__)), ".env"),
        os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".env"),
    ]
    for env_path in candidates:
        if not os.path.isfile(env_path):
            continue
        try:
            with open(env_path, "r", encoding="utf-8") as fh:
                for line in fh:
                    line = line.strip()
                    if not line or line.startswith("#") or "=" not in line:
                        continue
                    k, v = line.split("=", 1)
                    k = k.strip()
                    v = v.strip()
                    if v.startswith('"') and v.find('"', 1) != -1:
                        v = v[1 : v.find('"', 1)]
                    elif v.startswith("'") and v.find("'", 1) != -1:
                        v = v[1 : v.find("'", 1)]
                    elif "#" in v:
                        v = v.split("#", 1)[0].strip()
                    if k and k not in os.environ:
                        os.environ[k] = v
        except OSError:
            pass


_load_env_file()

app = Flask(__name__)
app.config["MAX_CONTENT_LENGTH"] = int(
    os.environ.get("MAX_UPLOAD_BYTES", str(2 * 1024 * 1024 * 1024))
)

UPLOAD_DIR = os.environ.get("UPLOAD_DIR", "replays")
STUBS_DIR = os.path.join(UPLOAD_DIR, ".stubs")
SPRITES_DIR = os.path.join(UPLOAD_DIR, ".sprites")
LINKS_DIR = os.path.join(UPLOAD_DIR, ".links")
INDEX_PATH = os.path.join(UPLOAD_DIR, "index.json")

SECRET = (os.environ.get("UPLOAD_SECRET") or "").strip()
if not SECRET or SECRET in ("somethingYouMakeUp1234!", "palentir_replay_secret_key"):
    raise RuntimeError(
        "UPLOAD_SECRET must be set to a strong non-default value in environment or .env"
    )

BASE_URL = (
    os.environ.get("PUBLIC_REPLAY_BASE_URL")
    or os.environ.get("BASE_URL")
    or "http://localhost:8085"
).rstrip("/")
WATCH_BASE_URL = f"{BASE_URL}/watch"
HOST = os.environ.get("UPLOAD_HOST", "0.0.0.0")
PORT = int(os.environ.get("UPLOAD_PORT") or os.environ.get("PORT") or "5555")

RETENTION_DAYS = max(1, int(os.environ.get("RETENTION_DAYS", "7")))
RETENTION_SECS = RETENTION_DAYS * 86400
MAINT_INTERVAL_SECS = max(300, int(os.environ.get("MAINT_INTERVAL_SECS", "3600")))
STALE_TMP_SECS = 3600

DEFAULT_THUMB_SECONDS = 5.0
THUMB_WIDTH = "1280"
WEBP_WIDTH = "720"
SPRITE_FRAMES = 6
SPRITE_W = 320
SPRITE_H = 180

MAINT_LOCK = threading.Lock()

KNOWN_MOD_TOKENS = {
    "NM", "NF", "EZ", "TD", "HD", "HR", "SD", "DT", "RX", "HT",
    "NC", "FL", "AT", "SO", "AP", "PF", "V2", "CL", "MR", "FI",
    "DC", "BL", "ST", "AC", "TP", "DA", "RD", "WU", "WD", "TC",
}

RE_TITLE_STARS = re.compile(r"^\[\s*(\d+(?:\.\d+)?)\s*(?:⭐|★|\*)\s*\]\s*")
RE_TITLE_PP = re.compile(r"(?:\s*[•|·-]\s*|\s+)(\d+(?:\.\d+)?)\s*pp\s*$", re.I)
RE_TITLE_ACC = re.compile(r"\s*\((\d+(?:\.\d+)?)%\)\s*$")
RE_TITLE_MODS = re.compile(r"\s+\+([A-Z0-9]{2,16})\s*$")
RE_BEATMAP_ID = re.compile(r"osu\.ppy\.sh/(?:b|beatmaps)/(\d+)")
RE_MAPSET_ID = re.compile(r"osu\.ppy\.sh/(?:s|beatmapsets)/(\d+)")
RE_SAFE_STEM = re.compile(r"^[A-Za-z0-9._-]+$")
RE_SHORT_ID = re.compile(r"^[a-z0-9]{6,16}$")

os.makedirs(UPLOAD_DIR, exist_ok=True)
os.makedirs(STUBS_DIR, mode=0o755, exist_ok=True)
os.makedirs(SPRITES_DIR, mode=0o755, exist_ok=True)
os.makedirs(LINKS_DIR, mode=0o755, exist_ok=True)


def _is_safe_stem(stem):
    return bool(stem) and stem not in (".", "..") and bool(RE_SAFE_STEM.fullmatch(stem))


def cleanup_file(path):
    if path and (os.path.exists(path) or os.path.islink(path)):
        try:
            os.remove(path)
        except OSError:
            pass


def _read_link_target(short_id):
    if not short_id or not RE_SHORT_ID.fullmatch(str(short_id)):
        return None
    txt_path = os.path.join(LINKS_DIR, f"{short_id}.txt")
    if not os.path.isfile(txt_path):
        return None
    try:
        with open(txt_path, "r", encoding="utf-8") as fh:
            val = fh.read().strip()
        return val if _is_safe_stem(val) else None
    except OSError:
        return None


def compute_short_id(stem, preferred_id=None):
    """Return a deterministic collision-checked 8+ char lowercase hex short ID for stem."""
    if preferred_id and RE_SHORT_ID.fullmatch(str(preferred_id)):
        existing = _read_link_target(preferred_id)
        if existing in (None, stem):
            return str(preferred_id)

    digest = hashlib.sha256(stem.encode("utf-8")).hexdigest()
    for length in (8, 10, 12, 16):
        cand = digest[:length]
        existing = _read_link_target(cand)
        if existing is None or existing == stem:
            return cand
        if not os.path.isfile(os.path.join(UPLOAD_DIR, f"{existing}.mp4")):
            return cand
    return digest[:16]


def ensure_short_link(stem, preferred_id=None):
    """Atomically create /.links/<id>.txt (slug) and /.links/<id>.mp4 (symlink to ../<stem>.mp4)."""
    if not _is_safe_stem(stem):
        raise ValueError("Refusing to create short link for unsafe replay stem")
    os.makedirs(LINKS_DIR, mode=0o755, exist_ok=True)
    short_id = compute_short_id(stem, preferred_id=preferred_id)

    txt_path = os.path.join(LINKS_DIR, f"{short_id}.txt")
    sym_path = os.path.join(LINKS_DIR, f"{short_id}.mp4")
    rel_target = f"../{stem}.mp4"

    if _read_link_target(short_id) != stem:
        tmp_txt = os.path.join(LINKS_DIR, f".{uuid.uuid4().hex}_{short_id}.tmp.txt")
        try:
            with open(tmp_txt, "w", encoding="utf-8") as fh:
                fh.write(stem)
            os.chmod(tmp_txt, 0o644)
            os.replace(tmp_txt, txt_path)
        except OSError:
            cleanup_file(tmp_txt)

    need_sym = True
    if os.path.islink(sym_path):
        try:
            if os.readlink(sym_path) == rel_target:
                need_sym = False
        except OSError:
            pass
    if need_sym:
        tmp_sym = os.path.join(LINKS_DIR, f".{uuid.uuid4().hex}_{short_id}.tmp.mp4")
        try:
            os.symlink(rel_target, tmp_sym)
            os.replace(tmp_sym, sym_path)
        except OSError:
            cleanup_file(tmp_sym)

    return short_id


def resolve_tool(name):
    found = shutil.which(name)
    if found:
        return found
    for candidate in (f"/usr/lib/jellyfin-ffmpeg/{name}", f"/usr/bin/{name}"):
        if os.path.isfile(candidate) and os.access(candidate, os.X_OK):
            return candidate
    return None


def _parse_fps(rate_str):
    if not rate_str or rate_str in ("0/0", "N/A"):
        return None
    try:
        if "/" in rate_str:
            num_s, den_s = rate_str.split("/", 1)
            num, den = float(num_s), float(den_s)
            if den <= 0:
                return None
            val = num / den
        else:
            val = float(rate_str)
        if not math.isfinite(val) or val <= 0 or val > 1000:
            return None
        nearest = round(val)
        if abs(val - nearest) < 0.02 and nearest > 0:
            return int(nearest)
        return round(val, 2)
    except (ValueError, ZeroDivisionError):
        return None


def probe_video(video_path):
    """Return video metadata dict using ffprobe."""
    result = {
        "width": None,
        "height": None,
        "duration": None,
        "fps": None,
        "video_codec": None,
        "audio_codec": None,
        "bitrate_kbps": None,
    }
    ffprobe = resolve_tool("ffprobe")
    if not ffprobe or not os.path.isfile(video_path):
        return result
    try:
        proc = subprocess.run(
            [
                ffprobe,
                "-v",
                "error",
                "-show_entries",
                "stream=codec_type,codec_name,width,height,avg_frame_rate,r_frame_rate:format=duration,bit_rate",
                "-of",
                "json",
                video_path,
            ],
            check=True,
            capture_output=True,
            timeout=15,
            text=True,
        )
        info = json.loads(proc.stdout or "{}")
        streams = info.get("streams") or []
        v_stream = next((s for s in streams if s.get("codec_type") == "video"), None)
        a_stream = next((s for s in streams if s.get("codec_type") == "audio"), None)
        fmt = info.get("format") or {}

        if v_stream:
            if v_stream.get("width"):
                result["width"] = int(v_stream["width"])
            if v_stream.get("height"):
                result["height"] = int(v_stream["height"])
            result["fps"] = _parse_fps(v_stream.get("avg_frame_rate")) or _parse_fps(
                v_stream.get("r_frame_rate")
            )
            if v_stream.get("codec_name"):
                result["video_codec"] = str(v_stream["codec_name"])[:32]
        if a_stream and a_stream.get("codec_name"):
            result["audio_codec"] = str(a_stream["codec_name"])[:32]

        if fmt.get("duration"):
            dur = float(fmt["duration"])
            if math.isfinite(dur) and dur > 0:
                result["duration"] = round(dur, 2)
        if fmt.get("bit_rate"):
            br = int(fmt["bit_rate"])
            if br > 0:
                result["bitrate_kbps"] = int(round(br / 1000.0))
        elif result["duration"] and result["duration"] > 0:
            try:
                sz = os.path.getsize(video_path)
                result["bitrate_kbps"] = int(round((sz * 8.0) / result["duration"] / 1000.0))
            except OSError:
                pass
        return result
    except (subprocess.SubprocessError, ValueError, KeyError, OSError, json.JSONDecodeError):
        return result


def pick_thumbnail_timestamp(duration_secs):
    if duration_secs is None or duration_secs <= 0:
        return f"{DEFAULT_THUMB_SECONDS:.2f}"
    # Avoid black intro fade or overshooting short -start/-end clips.
    target = min(DEFAULT_THUMB_SECONDS, max(0.2, duration_secs * 0.35))
    if target >= duration_secs:
        target = max(0.0, duration_secs * 0.5)
    return f"{target:.2f}"


def generate_thumbnail(video_path, thumb_path, stem, duration_secs):
    ffmpeg = resolve_tool("ffmpeg")
    if not ffmpeg:
        return False

    tmp_thumb_path = os.path.join(UPLOAD_DIR, f".{uuid.uuid4().hex}_{stem}.tmp.jpg")
    seek_at = pick_thumbnail_timestamp(duration_secs)
    try:
        subprocess.run(
            [
                ffmpeg,
                "-y",
                "-ss",
                seek_at,
                "-i",
                video_path,
                "-frames:v",
                "1",
                "-vf",
                f"scale={THUMB_WIDTH}:-2",
                "-q:v",
                "3",
                tmp_thumb_path,
            ],
            check=True,
            timeout=60,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        os.chmod(tmp_thumb_path, 0o644)
        os.replace(tmp_thumb_path, thumb_path)
        return True
    except (subprocess.SubprocessError, OSError):
        cleanup_file(tmp_thumb_path)
        return False


def generate_webp_thumbnail(thumb_path, webp_path, stem):
    ffmpeg = resolve_tool("ffmpeg")
    if not ffmpeg or not os.path.isfile(thumb_path):
        return False

    tmp_webp_path = os.path.join(UPLOAD_DIR, f".{uuid.uuid4().hex}_{stem}.tmp.webp")
    try:
        subprocess.run(
            [
                ffmpeg,
                "-y",
                "-i",
                thumb_path,
                "-vf",
                f"scale={WEBP_WIDTH}:-2",
                "-c:v",
                "libwebp",
                "-quality",
                "80",
                tmp_webp_path,
            ],
            check=True,
            timeout=30,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        os.chmod(tmp_webp_path, 0o644)
        os.replace(tmp_webp_path, webp_path)
        return True
    except (subprocess.SubprocessError, OSError):
        cleanup_file(tmp_webp_path)
        return False


def generate_storyboard_sprite(video_path, sprite_path, stem, duration_secs):
    """Build a 6x1 hover-scrub storyboard WebP sprite (1920x180, ~15-30 KB)."""
    ffmpeg = resolve_tool("ffmpeg")
    if not ffmpeg or not os.path.isfile(video_path):
        return False

    os.makedirs(SPRITES_DIR, mode=0o755, exist_ok=True)
    tmp_sprite_path = os.path.join(SPRITES_DIR, f".{uuid.uuid4().hex}_{stem}.tmp.webp")

    dur = float(duration_secs) if duration_secs and duration_secs > 0 else 10.0
    if dur >= 1.5:
        timestamps = [
            dur * (0.10 + 0.80 * i / max(1, SPRITE_FRAMES - 1))
            for i in range(SPRITE_FRAMES)
        ]
    else:
        timestamps = [
            max(0.0, min(dur * 0.9, dur * (i / max(1, SPRITE_FRAMES))))
            for i in range(SPRITE_FRAMES)
        ]

    cmd = [ffmpeg, "-y"]
    for t in timestamps:
        cmd.extend(["-ss", f"{t:.2f}", "-i", video_path])

    scale_filters = [
        f"[{i}:v]scale={SPRITE_W}:{SPRITE_H}:force_original_aspect_ratio=increase,"
        f"crop={SPRITE_W}:{SPRITE_H}[v{i}]"
        for i in range(SPRITE_FRAMES)
    ]
    inputs_concat = "".join(f"[v{i}]" for i in range(SPRITE_FRAMES))
    filter_complex = ";".join(scale_filters) + f";{inputs_concat}hstack=inputs={SPRITE_FRAMES}[out]"

    cmd.extend(
        [
            "-filter_complex",
            filter_complex,
            "-map",
            "[out]",
            "-frames:v",
            "1",
            "-c:v",
            "libwebp",
            "-quality",
            "75",
            tmp_sprite_path,
        ]
    )

    try:
        subprocess.run(
            cmd,
            check=True,
            timeout=60,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        os.chmod(tmp_sprite_path, 0o644)
        os.replace(tmp_sprite_path, sprite_path)
        return True
    except (subprocess.SubprocessError, OSError):
        cleanup_file(tmp_sprite_path)
        return False


def probe_image_dims(image_path):
    """Return (width, height) of an image via ffprobe, or (None, None)."""
    ffprobe = resolve_tool("ffprobe")
    if not ffprobe or not os.path.isfile(image_path):
        return None, None
    try:
        proc = subprocess.run(
            [
                ffprobe,
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=width,height",
                "-of",
                "json",
                image_path,
            ],
            check=True,
            capture_output=True,
            timeout=10,
            text=True,
        )
        info = json.loads(proc.stdout or "{}")
        streams = info.get("streams") or []
        stream0 = streams[0] if streams else {}
        w = int(stream0["width"]) if stream0.get("width") else None
        h = int(stream0["height"]) if stream0.get("height") else None
        return w, h
    except (subprocess.SubprocessError, ValueError, KeyError, OSError, json.JSONDecodeError):
        return None, None


def compute_thumb_dims(thumb_path, video_width, video_height):
    tw, th = probe_image_dims(thumb_path)
    if tw and th:
        return tw, th
    if video_width and video_height and video_width > 0 and video_height > 0:
        w = int(THUMB_WIDTH)
        h = max(2, int(round(video_height * (w / video_width))) & ~1)
        return w, h
    return int(THUMB_WIDTH), 720


def generate_discord_stub(thumb_path, stub_path, stem):
    ffmpeg = resolve_tool("ffmpeg")
    if not ffmpeg or not os.path.isfile(thumb_path):
        return False

    stub_dir = os.path.dirname(stub_path) or STUBS_DIR
    os.makedirs(stub_dir, mode=0o755, exist_ok=True)
    tmp_stub_path = os.path.join(stub_dir, f".{uuid.uuid4().hex}_{stem}.tmp.mp4")
    try:
        subprocess.run(
            [
                ffmpeg,
                "-y",
                "-loop",
                "1",
                "-framerate",
                "2",
                "-i",
                thumb_path,
                "-f",
                "lavfi",
                "-i",
                "anullsrc=channel_layout=stereo:sample_rate=44100",
                "-t",
                "0.5",
                "-vf",
                "scale=trunc(iw/2)*2:trunc(ih/2)*2:out_range=limited,format=yuv420p",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-tune",
                "stillimage",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-b:a",
                "32k",
                "-movflags",
                "+faststart",
                "-shortest",
                tmp_stub_path,
            ],
            check=True,
            timeout=30,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        os.chmod(tmp_stub_path, 0o644)
        os.replace(tmp_stub_path, stub_path)
        return True
    except (subprocess.SubprocessError, OSError):
        cleanup_file(tmp_stub_path)
        return False


def _is_valid_mod_string(mod_str):
    if not mod_str or len(mod_str) % 2 != 0:
        return False
    tokens = [mod_str[i : i + 2] for i in range(0, len(mod_str), 2)]
    return all(tok in KNOWN_MOD_TOKENS for tok in tokens)


def parse_title_fallback(title, stem, beatmap_url=""):
    """Extract structured display fields from a rendered title or filename stem."""
    raw = (title or "").strip()
    stars = None
    player = None
    map_title = None
    mods = "NM"
    accuracy = None
    pp_val = None

    m_stars = RE_TITLE_STARS.match(raw)
    if m_stars:
        try:
            stars = round(float(m_stars.group(1)), 2)
        except ValueError:
            pass
        raw = raw[m_stars.end() :]

    if " | " in raw:
        p_part, rest = raw.split(" | ", 1)
        player = p_part.strip()[:64] or None
    else:
        rest = raw

    m_pp = RE_TITLE_PP.search(rest)
    if m_pp:
        try:
            pp_val = round(float(m_pp.group(1)), 2)
        except ValueError:
            pass
        rest = rest[: m_pp.start()].strip()

    m_acc = RE_TITLE_ACC.search(rest)
    if m_acc:
        try:
            accuracy = round(float(m_acc.group(1)), 2)
        except ValueError:
            pass
        rest = rest[: m_acc.start()].strip()

    m_mods = RE_TITLE_MODS.search(rest)
    if m_mods and _is_valid_mod_string(m_mods.group(1)):
        mods = m_mods.group(1)
        rest = rest[: m_mods.start()].strip()

    if rest:
        map_title = rest[:240]

    # Fallback to stem format: {stars}-{player}-{map}-{mods}-{acc}_{hash}_{ts}
    if stars is None or player is None or accuracy is None:
        m_stem = re.match(
            r"^(\d+_\d+)-([^-]+)-(.+)-([A-Z0-9]{2,16})-(\d+_\d+)_[0-9a-f]{4,}_(\d+)$",
            stem or "",
        )
        if m_stem:
            if stars is None:
                try:
                    stars = round(float(m_stem.group(1).replace("_", ".")), 2)
                except ValueError:
                    pass
            if player is None:
                player = m_stem.group(2).replace("_", " ").strip()[:64]
            if not map_title:
                map_title = m_stem.group(3).replace("_", " ").strip()[:240]
            if mods == "NM" and _is_valid_mod_string(m_stem.group(4)):
                mods = m_stem.group(4)
            if accuracy is None:
                try:
                    accuracy = round(float(m_stem.group(5).replace("_", ".")), 2)
                except ValueError:
                    pass

    beatmap_id = None
    mapset_id = None
    if beatmap_url:
        mb = RE_BEATMAP_ID.search(beatmap_url)
        if mb:
            beatmap_id = int(mb.group(1))
        ms = RE_MAPSET_ID.search(beatmap_url)
        if ms:
            mapset_id = int(ms.group(1))

    out = {
        "player": player or "Unknown",
        "map_title": map_title or title or stem,
        "stars": stars,
        "mods": mods or "NM",
        "accuracy": accuracy,
        "beatmap_id": beatmap_id,
        "mapset_id": mapset_id,
    }
    if pp_val is not None:
        out["pp"] = pp_val
    return out


def _safe_float(val, lo=0.0, hi=1e7, digits=2):
    if val is None or isinstance(val, bool):
        return None
    try:
        f = float(val)
        if not math.isfinite(f):
            return None
        f = max(lo, min(hi, f))
        return round(f, digits)
    except (TypeError, ValueError):
        return None


def _safe_int(val, lo=0, hi=10**12):
    if val is None or isinstance(val, bool):
        return None
    try:
        i = int(val)
        if i < lo or i > hi:
            return None
        return i
    except (TypeError, ValueError):
        return None


def parse_extra_meta(raw_meta_str):
    """Sanitize optional structured JSON metadata supplied by palentirbot."""
    if not raw_meta_str:
        return {}
    try:
        data = json.loads(raw_meta_str)
    except (ValueError, TypeError):
        return {}
    if not isinstance(data, dict):
        return {}

    out = {}
    for key, max_len in (
        ("player", 64),
        ("map_title", 240),
        ("mods", 32),
        ("grade", 8),
        ("skin", 128),
    ):
        v = data.get(key)
        if isinstance(v, str) and v.strip():
            out[key] = v.strip()[:max_len]

    if "grade" in out and out["grade"].upper() not in (
        "SSH", "SS", "SH", "S", "A", "B", "C", "D", "F", "XH", "X"
    ):
        del out["grade"]
    elif "grade" in out:
        g = out["grade"].upper()
        out["grade"] = {"XH": "SSH", "X": "SS"}.get(g, g)

    for key, lo, hi in (
        ("stars", 0.0, 100.0),
        ("accuracy", 0.0, 100.0),
        ("pp", 0.0, 100000.0),
        ("nochoke_pp", 0.0, 100000.0),
        ("max_pp", 0.0, 100000.0),
        ("aim_pp", 0.0, 100000.0),
        ("speed_pp", 0.0, 100000.0),
        ("acc_pp", 0.0, 100000.0),
    ):
        v = _safe_float(data.get(key), lo, hi, 2)
        if v is not None:
            out[key] = v

    for key in (
        "mods_bits",
        "score",
        "max_combo",
        "map_max_combo",
        "count_300",
        "count_100",
        "count_50",
        "count_miss",
        "beatmap_id",
        "mapset_id",
        "play_timestamp",
    ):
        v = _safe_int(data.get(key))
        if v is not None:
            out[key] = v

    if isinstance(data.get("is_lazer"), bool):
        out["is_lazer"] = data["is_lazer"]

    return out


def write_json_atomic(dest_path, stem, payload):
    parent = os.path.dirname(dest_path) or UPLOAD_DIR
    tmp_path = os.path.join(parent, f".{uuid.uuid4().hex}_{stem}.tmp.json")
    try:
        with open(tmp_path, "w", encoding="utf-8") as fh:
            json.dump(payload, fh, ensure_ascii=False, separators=(",", ":"))
        os.chmod(tmp_path, 0o644)
        os.replace(tmp_path, dest_path)
        return True
    except OSError:
        cleanup_file(tmp_path)
        return False


def _remove_bundle(stem):
    """Remove an MP4 and every companion artifact (including its short link) atomically together."""
    meta = _load_sidecar_safe(os.path.join(UPLOAD_DIR, f"{stem}.json"))
    short_id = meta.get("short_id")
    targets = [
        os.path.join(UPLOAD_DIR, f"{stem}.mp4"),
        os.path.join(UPLOAD_DIR, f"{stem}.jpg"),
        os.path.join(UPLOAD_DIR, f"{stem}.webp"),
        os.path.join(UPLOAD_DIR, f"{stem}.json"),
        os.path.join(STUBS_DIR, f"{stem}.stub.mp4"),
        os.path.join(SPRITES_DIR, f"{stem}.webp"),
    ]
    if isinstance(short_id, str) and re.fullmatch(r"[a-z0-9]{6,16}", short_id):
        targets.append(os.path.join(LINKS_DIR, f"{short_id}.mp4"))
        targets.append(os.path.join(LINKS_DIR, f"{short_id}.txt"))
    for p in targets:
        cleanup_file(p)


def prune_expired_and_orphans():
    """Bundle-aware 7-day retention (based strictly on .mp4 mtime) + orphan sweep."""
    now = time.time()
    stats = {"expired_bundles": 0, "orphan_files": 0, "stale_tmp": 0}

    with MAINT_LOCK:
        try:
            entries = os.listdir(UPLOAD_DIR)
        except OSError:
            return stats

        active_stems = set()
        for name in entries:
            if name.startswith("."):
                full = os.path.join(UPLOAD_DIR, name)
                if ".tmp" in name and os.path.isfile(full):
                    try:
                        if now - os.path.getmtime(full) > STALE_TMP_SECS:
                            cleanup_file(full)
                            stats["stale_tmp"] += 1
                    except OSError:
                        pass
                continue
            if not name.endswith(".mp4"):
                continue
            full = os.path.join(UPLOAD_DIR, name)
            stem = name[:-4]
            try:
                mtime = os.path.getmtime(full)
            except OSError:
                continue
            if now - mtime > RETENTION_SECS:
                _remove_bundle(stem)
                stats["expired_bundles"] += 1
            else:
                active_stems.add(stem)

        # Sweep orphan sidecars/thumbnails in UPLOAD_DIR
        for name in entries:
            if name.startswith(".") or name == "index.json":
                continue
            for ext in (".json", ".jpg", ".webp"):
                if name.endswith(ext):
                    stem = name[: -len(ext)]
                    if stem not in active_stems:
                        cleanup_file(os.path.join(UPLOAD_DIR, name))
                        stats["orphan_files"] += 1
                    break

        # Sweep orphan stubs & sprites
        for directory, suffix in (
            (STUBS_DIR, ".stub.mp4"),
            (SPRITES_DIR, ".webp"),
        ):
            try:
                sub_entries = os.listdir(directory)
            except OSError:
                continue
            for name in sub_entries:
                full = os.path.join(directory, name)
                if name.startswith(".") and ".tmp" in name:
                    try:
                        if now - os.path.getmtime(full) > STALE_TMP_SECS:
                            cleanup_file(full)
                            stats["stale_tmp"] += 1
                    except OSError:
                        pass
                    continue
                if not name.endswith(suffix):
                    continue
                stem = name[: -len(suffix)]
                if stem not in active_stems:
                    cleanup_file(full)
                    stats["orphan_files"] += 1

        # Sweep expired/dangling short links in LINKS_DIR
        try:
            link_entries = os.listdir(LINKS_DIR)
        except OSError:
            link_entries = []
        for name in link_entries:
            full = os.path.join(LINKS_DIR, name)
            if name.startswith(".") and ".tmp" in name:
                try:
                    st_t = os.lstat(full)
                    if now - st_t.st_mtime > STALE_TMP_SECS:
                        cleanup_file(full)
                        stats["stale_tmp"] += 1
                except OSError:
                    pass
                continue
            if name.endswith(".txt"):
                short_id = name[:-4]
                target_stem = _read_link_target(short_id)
                if not target_stem or target_stem not in active_stems:
                    cleanup_file(full)
                    cleanup_file(os.path.join(LINKS_DIR, f"{short_id}.mp4"))
                    stats["orphan_files"] += 1
            elif name.endswith(".mp4"):
                short_id = name[:-4]
                target_stem = _read_link_target(short_id)
                if not os.path.isfile(full) or not target_stem or target_stem not in active_stems:
                    cleanup_file(full)
                    cleanup_file(os.path.join(LINKS_DIR, f"{short_id}.txt"))
                    stats["orphan_files"] += 1

    return stats


def _load_sidecar_safe(meta_path):
    if not os.path.isfile(meta_path):
        return {}
    try:
        with open(meta_path, "r", encoding="utf-8") as fh:
            data = json.load(fh)
        return data if isinstance(data, dict) else {}
    except (OSError, ValueError):
        return {}


def backfill_missing_assets():
    """Repair missing .jpg/.webp/.stub.mp4/.sprites/.json fields for all active replays."""
    stats = {
        "replays_checked": 0,
        "thumbs_generated": 0,
        "webps_generated": 0,
        "stubs_generated": 0,
        "sprites_generated": 0,
        "sidecars_updated": 0,
    }
    try:
        entries = sorted(os.listdir(UPLOAD_DIR))
    except OSError:
        return stats

    for name in entries:
        if name.startswith(".") or not name.endswith(".mp4"):
            continue
        stem = name[:-4]
        video_path = os.path.join(UPLOAD_DIR, name)
        if not os.path.isfile(video_path):
            continue

        stats["replays_checked"] += 1
        meta_path = os.path.join(UPLOAD_DIR, f"{stem}.json")
        thumb_path = os.path.join(UPLOAD_DIR, f"{stem}.jpg")
        webp_path = os.path.join(UPLOAD_DIR, f"{stem}.webp")
        stub_path = os.path.join(STUBS_DIR, f"{stem}.stub.mp4")
        sprite_path = os.path.join(SPRITES_DIR, f"{stem}.webp")

        meta = _load_sidecar_safe(meta_path)
        dirty = not bool(meta)

        needs_probe = any(
            meta.get(k) is None
            for k in ("width", "height", "duration", "fps", "video_codec", "bitrate_kbps")
        )
        if needs_probe:
            probed = probe_video(video_path)
            for k, v in probed.items():
                if v is not None and meta.get(k) is None:
                    meta[k] = v
                    dirty = True

        duration = meta.get("duration")

        if not os.path.isfile(thumb_path):
            if generate_thumbnail(video_path, thumb_path, stem, duration):
                stats["thumbs_generated"] += 1
                dirty = True

        if os.path.isfile(thumb_path) and not os.path.isfile(webp_path):
            if generate_webp_thumbnail(thumb_path, webp_path, stem):
                stats["webps_generated"] += 1
                dirty = True

        if os.path.isfile(thumb_path) and not os.path.isfile(stub_path):
            if generate_discord_stub(thumb_path, stub_path, stem):
                stats["stubs_generated"] += 1

        if not os.path.isfile(sprite_path):
            if generate_storyboard_sprite(video_path, sprite_path, stem, duration):
                stats["sprites_generated"] += 1
                dirty = True

        # Ensure standard sidecar metadata fields are populated
        try:
            st = os.stat(video_path)
            size_bytes = st.st_size
            mtime = int(st.st_mtime)
        except OSError:
            size_bytes = meta.get("size_bytes")
            mtime = int(time.time())

        safe_video = urllib.parse.quote(name)
        safe_stem = urllib.parse.quote(stem)
        short_id = ensure_short_link(stem, preferred_id=meta.get("short_id"))
        defaults = {
            "filename": name,
            "short_id": short_id,
            "title": meta.get("title") or stem,
            "author": meta.get("author", ""),
            "beatmap": meta.get("beatmap", ""),
            "hash": meta.get("hash", ""),
            "size_bytes": size_bytes,
            "uploaded_at": meta.get("uploaded_at") or mtime,
            "video_url": f"{BASE_URL}/{safe_video}",
            "watch_url": f"{WATCH_BASE_URL}/{short_id}",
            "long_watch_url": f"{WATCH_BASE_URL}/{safe_stem}",
            "retention_days": RETENTION_DAYS,
        }
        if os.path.isfile(thumb_path):
            defaults["thumbnail_url"] = f"{BASE_URL}/{urllib.parse.quote(stem + '.jpg')}"
            if not meta.get("thumb_width") or not meta.get("thumb_height"):
                tw, th = compute_thumb_dims(thumb_path, meta.get("width"), meta.get("height"))
                defaults["thumb_width"] = tw
                defaults["thumb_height"] = th
        if os.path.isfile(webp_path):
            defaults["webp_url"] = f"{BASE_URL}/{urllib.parse.quote(stem + '.webp')}"
        if os.path.isfile(sprite_path):
            defaults["sprite_url"] = f"{BASE_URL}/sprites/{safe_stem}.webp"
            defaults["sprite_frames"] = SPRITE_FRAMES

        parsed = parse_title_fallback(defaults["title"], stem, defaults["beatmap"])
        for k, v in parsed.items():
            if v is not None and not meta.get(k):
                defaults[k] = v

        for k, v in defaults.items():
            if meta.get(k) != v and (meta.get(k) is None or k in (
                "short_id", "watch_url", "long_watch_url",
                "retention_days", "webp_url", "sprite_url", "sprite_frames",
                "thumbnail_url", "thumb_width", "thumb_height", "size_bytes",
            )):
                meta[k] = v
                dirty = True

        if dirty:
            if write_json_atomic(meta_path, stem, meta):
                stats["sidecars_updated"] += 1

    return stats


def rebuild_index_json():
    """Atomically write /media/storage/replays/index.json for fast gallery & stats pages."""
    with MAINT_LOCK:
        try:
            entries = os.listdir(UPLOAD_DIR)
        except OSError:
            return 0

        items = []
        total_bytes = 0
        total_duration = 0.0

        for name in entries:
            if name.startswith(".") or not name.endswith(".mp4"):
                continue
            stem = name[:-4]
            video_path = os.path.join(UPLOAD_DIR, name)
            try:
                st = os.stat(video_path)
            except OSError:
                continue

            meta = _load_sidecar_safe(os.path.join(UPLOAD_DIR, f"{stem}.json"))
            title = meta.get("title") or stem
            beatmap = meta.get("beatmap") or ""
            parsed = parse_title_fallback(title, stem, beatmap)

            uploaded_at = _safe_int(meta.get("uploaded_at")) or int(st.st_mtime)
            size_bytes = st.st_size
            duration = _safe_float(meta.get("duration"), 0.0, 86400.0, 2)
            total_bytes += size_bytes
            if duration:
                total_duration += duration

            has_thumb = os.path.isfile(os.path.join(UPLOAD_DIR, f"{stem}.jpg"))
            has_webp = os.path.isfile(os.path.join(UPLOAD_DIR, f"{stem}.webp"))
            has_sprite = os.path.isfile(os.path.join(SPRITES_DIR, f"{stem}.webp"))

            entry = {
                "slug": stem,
                "short_id": meta.get("short_id") or ensure_short_link(stem),
                "title": title,
                "player": meta.get("player") or parsed["player"],
                "map_title": meta.get("map_title") or parsed["map_title"],
                "stars": meta.get("stars") if meta.get("stars") is not None else parsed["stars"],
                "mods": meta.get("mods") or parsed["mods"],
                "accuracy": meta.get("accuracy") if meta.get("accuracy") is not None else parsed["accuracy"],
                "grade": meta.get("grade"),
                "pp": meta.get("pp"),
                "nochoke_pp": meta.get("nochoke_pp"),
                "max_pp": meta.get("max_pp"),
                "score": meta.get("score"),
                "max_combo": meta.get("max_combo"),
                "map_max_combo": meta.get("map_max_combo"),
                "count_300": meta.get("count_300"),
                "count_100": meta.get("count_100"),
                "count_50": meta.get("count_50"),
                "count_miss": meta.get("count_miss"),
                "beatmap": beatmap,
                "beatmap_id": meta.get("beatmap_id") or parsed["beatmap_id"],
                "mapset_id": meta.get("mapset_id") or parsed["mapset_id"],
                "skin": meta.get("skin"),
                "is_lazer": meta.get("is_lazer"),
                "width": meta.get("width"),
                "height": meta.get("height"),
                "fps": meta.get("fps"),
                "bitrate_kbps": meta.get("bitrate_kbps"),
                "video_codec": meta.get("video_codec"),
                "duration": duration,
                "size_bytes": size_bytes,
                "uploaded_at": uploaded_at,
                "expires_at": int(st.st_mtime) + RETENTION_SECS,
                "play_timestamp": meta.get("play_timestamp"),
                "has_thumb": has_thumb,
                "has_webp": has_webp,
                "has_sprite": has_sprite,
                "sprite_frames": SPRITE_FRAMES if has_sprite else 0,
            }
            # Drop None values to keep index.json compact and clean
            items.append({k: v for k, v in entry.items() if v is not None})

        items.sort(key=lambda r: (r.get("uploaded_at", 0), r.get("slug", "")), reverse=True)

        payload = {
            "generated_at": int(time.time()),
            "retention_days": RETENTION_DAYS,
            "count": len(items),
            "total_bytes": total_bytes,
            "total_duration": round(total_duration, 2),
            "replays": items,
        }
        write_json_atomic(INDEX_PATH, "index", payload)
        return len(items)


def run_maintenance(backfill=False):
    pruned = prune_expired_and_orphans()
    backfilled = backfill_missing_assets() if backfill else {}
    indexed = rebuild_index_json()
    return {"pruned": pruned, "backfilled": backfilled, "indexed": indexed}


def _start_maintenance_thread():
    def _loop():
        try:
            run_maintenance(backfill=True)
        except Exception as exc:
            app.logger.warning("Startup maintenance error: %s", exc)
        while True:
            time.sleep(MAINT_INTERVAL_SECS)
            try:
                run_maintenance(backfill=False)
            except Exception as exc:
                app.logger.warning("Periodic maintenance error: %s", exc)

    t = threading.Thread(target=_loop, name="replay-maintenance", daemon=True)
    t.start()


@app.route("/health", methods=["GET"])
def health():
    return jsonify(ok=True, retention_days=RETENTION_DAYS), 200


@app.route("/upload", methods=["POST"])
def upload():
    header_secret = request.headers.get("X-Upload-Secret")
    secret_val = header_secret if header_secret is not None else request.form.get("secret", "")
    if not SECRET or not hmac.compare_digest(
        secret_val.encode("utf-8"), SECRET.encode("utf-8")
    ):
        return jsonify(error=401, text="Unauthorized"), 401

    if "video" not in request.files:
        return jsonify(error=400, text="Missing video file"), 400

    f = request.files["video"]
    if not f.filename:
        return jsonify(error=400, text="Empty filename"), 400

    filename = secure_filename(f.filename)
    if not filename or not filename.lower().endswith(".mp4"):
        return jsonify(error=400, text="Only valid .mp4 filenames are accepted"), 400

    video_path = os.path.join(UPLOAD_DIR, filename)
    tmp_video_path = os.path.join(UPLOAD_DIR, f".{uuid.uuid4().hex}_{filename}.tmp")
    try:
        f.save(tmp_video_path)
        os.chmod(tmp_video_path, 0o644)
        os.replace(tmp_video_path, video_path)
    except Exception:
        cleanup_file(tmp_video_path)
        raise

    stem, _ = os.path.splitext(filename)
    thumb_filename = f"{stem}.jpg"
    thumb_path = os.path.join(UPLOAD_DIR, thumb_filename)
    webp_filename = f"{stem}.webp"
    webp_path = os.path.join(UPLOAD_DIR, webp_filename)
    sprite_path = os.path.join(SPRITES_DIR, f"{stem}.webp")

    probed = probe_video(video_path)
    width = probed["width"]
    height = probed["height"]
    duration = probed["duration"]

    if not generate_thumbnail(video_path, thumb_path, stem, duration):
        thumb_filename = None

    thumb_width, thumb_height = (
        compute_thumb_dims(thumb_path, width, height)
        if thumb_filename is not None
        else (int(THUMB_WIDTH), 720)
    )

    has_webp = False
    if thumb_filename is not None:
        has_webp = generate_webp_thumbnail(thumb_path, webp_path, stem)
        stub_path = os.path.join(STUBS_DIR, f"{stem}.stub.mp4")
        generate_discord_stub(thumb_path, stub_path, stem)

    has_sprite = generate_storyboard_sprite(video_path, sprite_path, stem, duration)

    safe_video = urllib.parse.quote(filename)
    safe_stem = urllib.parse.quote(stem)
    short_id = ensure_short_link(stem)
    video_url = f"{BASE_URL}/{safe_video}"
    watch_url = f"{WATCH_BASE_URL}/{short_id}"
    long_watch_url = f"{WATCH_BASE_URL}/{safe_stem}"

    thumbnail_url = (
        f"{BASE_URL}/{urllib.parse.quote(thumb_filename)}"
        if thumb_filename is not None
        else None
    )
    webp_url = f"{BASE_URL}/{urllib.parse.quote(webp_filename)}" if has_webp else None
    sprite_url = f"{BASE_URL}/sprites/{safe_stem}.webp" if has_sprite else None

    title_val = (request.form.get("title") or "").strip()[:300]
    author_val = (request.form.get("author") or "").strip()[:64]
    raw_beatmap = (request.form.get("beatmap") or "").strip()[:300]
    beatmap_val = (
        raw_beatmap
        if raw_beatmap.startswith(("https://osu.ppy.sh/", "http://osu.ppy.sh/"))
        else ""
    )
    hash_val = (request.form.get("hash") or "").strip()[:128]
    extra_meta = parse_extra_meta(request.form.get("meta"))

    try:
        size_bytes = os.path.getsize(video_path)
    except OSError:
        size_bytes = None

    parsed = parse_title_fallback(title_val or stem, stem, beatmap_val)

    meta_filename = f"{stem}.json"
    meta_path = os.path.join(UPLOAD_DIR, meta_filename)
    meta_payload = {
        "filename": filename,
        "short_id": short_id,
        "title": title_val or stem,
        "author": author_val,
        "beatmap": beatmap_val,
        "hash": hash_val,
        "width": width,
        "height": height,
        "fps": probed["fps"],
        "video_codec": probed["video_codec"],
        "audio_codec": probed["audio_codec"],
        "bitrate_kbps": probed["bitrate_kbps"],
        "thumb_width": thumb_width,
        "thumb_height": thumb_height,
        "duration": duration,
        "size_bytes": size_bytes,
        "uploaded_at": int(time.time()),
        "retention_days": RETENTION_DAYS,
        "video_url": video_url,
        "thumbnail_url": thumbnail_url,
        "webp_url": webp_url,
        "sprite_url": sprite_url,
        "sprite_frames": SPRITE_FRAMES if has_sprite else 0,
        "watch_url": watch_url,
        "long_watch_url": long_watch_url,
        **parsed,
        **extra_meta,
    }
    metadata_url = None
    if write_json_atomic(meta_path, stem, meta_payload):
        metadata_url = f"{BASE_URL}/{urllib.parse.quote(meta_filename)}"

    prune_expired_and_orphans()
    rebuild_index_json()

    return jsonify(
        error=0,
        text=watch_url,
        short_id=short_id,
        watch_url=watch_url,
        long_watch_url=long_watch_url,
        video_url=video_url,
        thumbnail_url=thumbnail_url,
        webp_url=webp_url,
        sprite_url=sprite_url,
        metadata_url=metadata_url,
        width=width,
        height=height,
        duration=duration,
        size_bytes=size_bytes,
    )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="osu! replay upload & storage service")
    parser.add_argument(
        "--maintain",
        action="store_true",
        help="Prune expired/orphan files, backfill missing assets, rebuild index.json, and exit",
    )
    args = parser.parse_args()

    if args.maintain:
        summary = run_maintenance(backfill=True)
        print(json.dumps(summary, indent=2))
    else:
        _start_maintenance_thread()
        app.run(host=HOST, port=PORT, threaded=True)
