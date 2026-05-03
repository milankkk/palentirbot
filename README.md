# ***palentirbot***

steps(maybe):

1. git clone

2. follow these first: https://github.com/mezodev0/shishabot --------->  REPO OF ORIGINAL

3. change fileserver python script to what port u want (default: 5555) and directory + upload_secret

4. then fill in .env.example and rename to .env

5. download and extract danser to ./data/danser
(i renamed danser-cli to danser and danser to danser-gui, because i have a headless server but dont know if it matters)

6. In default.json danser config change settings to whatever desired

7. run "python3 upload.py" in fileserver/
8. then "cargo run --release" (--release otherwise your bot wont work outside dev server)

9. pray 🙏

...

10. PROFIT!!!! 💸
