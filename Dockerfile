FROM python:3.12.9-slim-bookworm
RUN apt update && apt install -y g++ && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY . .
RUN pip install -r requirements.txt
CMD ["python3","bot.py"]
