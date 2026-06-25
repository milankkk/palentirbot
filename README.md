# ***palentirbot (no correlation)***
### vibecoded [shishabot](https://github.com/mezodev0/shishabot) back into life using claude and modern deps

![](https://cdn.insertdomainname.be/example.png)

[Invite link](https://discord.com/oauth2/authorize?client_id=1490728112173092875&permissions=4503599627388928&integration_type=0&scope=bot+applications.commands)

Default prefix: "<" , changeable using commands,
Supports reply to render, adding skins from raw osk links etc..

## Building steps(maybe):

1. git clone
---
2. follow these first: [shishabot github repo](https://github.com/mezodev0/shishabot) --------->  REPO OF ORIGINAL
---
3. change fileserver python script to what port u want (default: 5555) and directory + upload_secret
---
4. then fill in .env.example and rename to .env
---
5. download and extract [danser](https://github.com/Wieku/danser-go) to ./data/danser
(i renamed danser-cli to danser and danser to danser-gui, because i have a headless server but dont know if it matters)
---
7. Run danser once (chmod +x danser && ./danser)
8.After the first run, danser will generate a folder called settings/, where a credentials and default json file will be stored.
In default.json you need to change the settings to what you need (like encoder settings, skin settings, colors, etc..), and in credentials.json your client id and secret which you generate in your osu profile under oauth
---
10. run "python3 upload.py" in fileserver/ , also included is a Caddyfile.example file which i user for serving
---
11. then "cargo run --release" (--release otherwise your bot wont work outside dev server)
---
12. pray 🙏

13. ???

14. PROFIT!!!! 💸
