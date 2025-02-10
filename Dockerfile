FROM python:3.12.9-slim-bookworm
RUN apt update && apt install -y g++ wget screen git libicu-dev && rm -rf /var/lib/apt/lists/*

# 安装dotnet
WORKDIR /tmp/dotnet
RUN  wget https://builds.dotnet.microsoft.com/dotnet/scripts/v1/dotnet-install.sh 
RUN bash dotnet-install.sh --channel 9.0

WORKDIR /app
COPY . .

ENV PATH=$PATH:/root/.dotnet
ENV DOTNET_ROOT=/root/.dotnet
ENV PATH=$PATH:$DOTNET_ROOT:$DOTNET_ROOT/tools


# 编译Lagrange
WORKDIR /usr/src/app
RUN git clone https://github.com/LagrangeDev/Lagrange.Core.git
WORKDIR /usr/src/app/Lagrange.Core
RUN dotnet publish Lagrange.OneBot/Lagrange.OneBot.csproj \ 
    --no-self-contained \
    -p:PublishSingleFile=true \
    -p:IncludeNativeLibrariesForSelfExtract=true -p:DebugType=none \
    --framework net9.0 \
    -o /app/Lagrange


WORKDIR /app

RUN chmod 777 /app/Lagrange/Lagrange.OneBot
RUN pip install -r requirements.txt

CMD ["bash", "./start.sh"]