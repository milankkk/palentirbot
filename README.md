# Palentirbot

A modern revival of [shishabot](https://github.com/mezodev0/shishabot) using updated dependencies and robust asynchronous Rust.

![Preview](https://cdn.insertdomainname.be/example.png)

[**Invite the bot to your server**](https://discord.com/oauth2/authorize?client_id=1490728112173092875&permissions=4503599627388928&integration_type=0&scope=bot+applications.commands)

### Features
- Default prefix: `<` (changeable via commands)
- **Reply to Render**: Right-click or reply to any osu! score embed to instantly render the replay.
- **Custom Skins**: Add custom skins directly using raw `.osk` links.

---

## 🛠️ Setup & Installation

Follow these instructions carefully to host the bot yourself. Some background context can be found in the [original shishabot repository](https://github.com/mezodev0/shishabot).

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
   > **Note:** For headless servers, rename `danser-cli` to `danser` and `danser` to `danser-gui`
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
Pray 🙏 and enjoy your self-hosted render bot!
