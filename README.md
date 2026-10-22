# palentirbot (no correlation)

A modern revival of [shishabot](https://github.com/mezodev0/shishabot) using updated dependencies and robust asynchronous Rust.

![Preview](https://cdn.insertdomainname.be/example.png)

[**Invite the bot to your server**](https://discord.com/oauth2/authorize?client_id=1490728112173092875&permissions=4503599627388928&integration_type=0&scope=bot+applications.commands)

### Features
- Default prefix: `<` (changeable via commands)
- **Reply to Render**: Right-click or reply to any osu! score embed to instantly render the replay.
- **Custom Skins**: Add custom skins directly using raw `.osk` links.

---

## Setup & Installation

You can run the bot using **Docker / Podman** (recommended for full orchestration including PostgreSQL, Caddy, Fileserver, and Ollama) or **Bare Metal**.

---

### Method A: Docker & Podman Compose (Recommended)

The provided `docker-compose.yml` (and `podman-compose.yml`) orchestrates all required services:
- **`bot`**: The Rust Discord bot + danser-go + ffmpeg + xvfb (for headless rendering).
- **`postgres`**: PostgreSQL 16 with automatic schema initialization from migrations.
- **`fileserver`**: Lightweight upload receiver for rendered replay MP4s.
- **`caddy`**: Reverse proxy and static file server serving Discord-compatible video embeds and player.
- **`ollama`**: Local LLM backend for `/ask`, `/aicontext`, and `/aiclear`.

#### 1. Configure `.env`
Copy the template and fill in your Discord & osu! API keys and hosting URL:
```bash
cp .env.example .env
```
> **Note:** You do not need to configure PostgreSQL or the upload server—they are pre-configured with secure linked defaults out of the box.

#### 2. Danser Setup
**Automatic:** If `./data/` does not yet contain Danser, the container automatically downloads the latest release, configures the headless CLI binary, and generates default settings on first startup.

*(Optional)* You can also run the bootstrap script manually:
```bash
./scripts/setup_danser.sh ./data
```

#### 3. Start the Stack

**With Docker Compose:**
```bash
docker compose up -d --build
```

**With Podman:**
```bash
podman compose up -d --build
```

#### 4. (Optional) Pull an Ollama model
If you plan to use AI features with local Ollama:
```bash
docker exec -it palentirbot-ollama ollama pull huihui_ai/qwen3-abliterated:4b
# or with podman:
podman exec -it palentirbot-ollama ollama pull huihui_ai/qwen3-abliterated:4b
```

---

### Method B: Manual / Bare Metal Installation

Follow these instructions to host the bot directly on your host machine:

### 1. Clone the Repository
```bash
git clone https://github.com/milankkk/palentirbot.git
cd palentirbot
```

### 2. Environment Variables
Fill out the provided `.env.example` file with your credentials and rename it to `.env`:
```bash
cp .env.example .env
```

### 3. Configure the File Server
1. Navigate to the `fileserver/` directory.
2. Edit the python script to match your desired port (default is `5555`), directory, and `upload_secret`.
3. Start the upload server:
```bash
cd fileserver
python3 upload.py
```
*(Note: A `Caddyfile.example` is included if you wish to use Caddy for reverse-proxying.)*

### 4. Danser Setup
1. Download and extract [Danser](https://github.com/Wieku/danser-go) into the `./data/danser` directory.
   > **Note:** For headless servers, rename `danser-cli` to `danser` and `danser` to `danser-gui` or also set up a fake monitor for it to launch on in case it doesnt work (this is what did it for me)
2. Make it executable and run it once to generate the settings files:
```bash
cd data/danser
chmod +x danser
./danser
```
3. Danser will generate a `settings/` folder containing `default.json` and `credentials.json`.
4. In `default.json`, adjust your encoder settings, skin settings, colors, etc.
5. In `credentials.json`, input your osu! API Client ID and Secret (generate these in your osu! profile settings under OAuth).

### 5. Run the Bot
Compile and run the bot in release mode (required for global Discord command registration):
```bash
cargo run --release
```

### 6. Profit
Pray 🙏 and enjoy!
