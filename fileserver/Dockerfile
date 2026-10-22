FROM python:3.12-slim

WORKDIR /app

RUN pip install --no-cache-dir flask gunicorn werkzeug

COPY upload.py /app/upload.py

EXPOSE 5555

CMD ["gunicorn", "-w", "2", "-b", "0.0.0.0:5555", "--timeout", "120", "upload:app"]
