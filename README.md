# ***palentirbot (no correlation)***
### vibecoded shishabot back into life using claude and modern deps

steps(maybe):

1. git clone

2. follow these first: https://github.com/mezodev0/shishabot --------->  REPO OF ORIGINAL

3. change fileserver python script to what port u want (default: 5555) and directory + upload_secret

4. then fill in .env.example and rename to .env

5. download and extract danser to ./data/danser
(i renamed danser-cli to danser and danser to danser-gui, because i have a headless server but dont know if it matters)
after first run, danser will generate a folder settings/ where a credentials and default json file will be stored
in those change the settings to what you need, and in credentials your client id and secret which you generate in your osu profile under oauth

7. In default.json danser config change settings to whatever desired

8. run "python3 upload.py" in fileserver/ , also included is a Caddyfile.example file which i user for serving
9. then "cargo run --release" (--release otherwise your bot wont work outside dev server)

10. pray 🙏

11. ???

12. PROFIT!!!! 💸
