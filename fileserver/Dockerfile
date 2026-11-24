FROM python:3.12-slim

WORKDIR /app

RUN apt-get update && \
    apt-get install -y --no-install-recommends ffmpeg && \
    rm -rf /var/lib/apt/lists/*

RUN pip install --no-cache-dir flask gunicorn werkzeug

COPY upload.py /app/upload.py

EXPOSE 5555

CMD ["sh", "-c", "exec gunicorn -w 2 -b 0.0.0.0:${UPLOAD_PORT:-5555} --timeout 600 upload:app"]
