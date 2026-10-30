"""启动 meme-generator HTTP 服务，地址由环境变量指定（不依赖 config.toml）。

为什么不直接 `meme run`：
- 它只从 config.toml 读端口，默认 2233 落在本机 Windows 的保留端口段（2180-2279），绑定直接报 Errno 13；
- config.toml 在 %APPDATA%\\meme_generator 下，从打包应用（如 Claude 桌面端）里启动时 %APPDATA% 会被重定向，
  改了配置也未必生效。

环境变量：MEME_HOST（默认 127.0.0.1）、MEME_PORT（默认 3233）。
"""
import os

import uvicorn
from meme_generator.app import app, register_routers
from meme_generator.log import LOGGING_CONFIG


def main() -> None:
    host = os.environ.get("MEME_HOST", "127.0.0.1")
    port = int(os.environ.get("MEME_PORT", "3233"))
    register_routers()
    uvicorn.run(app, host=host, port=port, log_config=LOGGING_CONFIG)


if __name__ == "__main__":
    main()
