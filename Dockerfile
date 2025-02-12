FROM python:3.12.9-slim-bookworm
WORKDIR /app
COPY . .
RUN pip install --no-cache-dir -r requirements.txt
RUN apt-get update && apt-get install -y wget tar libicu-dev \
    && rm -rf /var/lib/apt/lists/*
RUN wget -O onebot.tar.gz https://github.com/LagrangeDev/Lagrange.Core/releases/download/nightly/Lagrange.OneBot_linux-x64_net9.0_SelfContained.tar.gz \
    && mkdir -p /app/onebot \
    && tar -xzf onebot.tar.gz -C /app/onebot \
    && rm onebot.tar.gz
RUN chmod +x /app/onebot/Lagrange.OneBot/bin/Release/net9.0/linux-x64/publish/Lagrange.OneBot
CMD ["/bin/sh", "-c", "/app/onebot/Lagrange.OneBot/bin/Release/net9.0/linux-x64/publish/Lagrange.OneBot & python bot.py"]