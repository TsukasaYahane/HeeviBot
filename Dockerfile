FROM python:3.12.9-slim-bookworm
RUN apt-get update && apt-get install -y wget tar libicu-dev screen \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY . .
RUN pip install --no-cache-dir -r requirements.txt
RUN pip install --no-cache-dir nonebot2 nonebot-adapter-onebot nonebot2[fastapi]
RUN wget -O onebot.tar.gz https://github.com/LagrangeDev/Lagrange.Core/releases/download/nightly/Lagrange.OneBot_linux-x64_net9.0_SelfContained.tar.gz \
    && mkdir -p /app/onebot \
    && tar -xzf onebot.tar.gz -C /app/onebot \
    && rm onebot.tar.gz
RUN chmod +x /app/onebot/Lagrange.OneBot/bin/Release/net9.0/linux-x64/publish/Lagrange.OneBot
CMD ["/bin/sh", "-c", "screen -S bot -d -m python /app/bot.py && /app/onebot/Lagrange.OneBot/bin/Release/net9.0/linux-x64/publish/Lagrange.OneBot"]
