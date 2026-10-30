"""下载 meme-generator 的字体到 meme_server/fonts（按大小校验，失败自动换镜像；可重复运行，已下好的跳过）。

用法：.venv\\Scripts\\python.exe fetch_fonts.py
"""
import json
import os
import sys
import urllib.parse
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "fonts")
LIST = os.path.join(HERE, "fonts.json")  # GitHub contents API 的结果：name + size

# 与 meme-generator 配置里的 resource_urls 同一批镜像
MIRRORS = [
    "https://cdn.jsdelivr.net/gh/MemeCrafters/meme-generator@main/resources/fonts/",
    "https://fastly.jsdelivr.net/gh/MemeCrafters/meme-generator@main/resources/fonts/",
    "https://raw.gitmirror.com/MemeCrafters/meme-generator/main/resources/fonts/",
    "https://raw.githubusercontent.com/MemeCrafters/meme-generator/main/resources/fonts/",
]


def fetch(name: str, size: int) -> str:
    dest = os.path.join(OUT, name)
    if os.path.exists(dest) and os.path.getsize(dest) == size:
        return "已存在"
    errors = []
    for base in MIRRORS:
        url = base + urllib.parse.quote(name)
        try:
            req = urllib.request.Request(url, headers={"User-Agent": "Mozilla/5.0"})
            with urllib.request.urlopen(req, timeout=120) as r:
                data = r.read()
            if len(data) != size:
                errors.append(f"{base.split('/')[2]} 大小不符 {len(data)}≠{size}")
                continue
            tmp = dest + ".part"
            with open(tmp, "wb") as fh:
                fh.write(data)
            os.replace(tmp, dest)
            return f"OK（{base.split('/')[2]}）"
        except Exception as e:  # noqa: BLE001 —— 换下一个镜像
            errors.append(f"{base.split('/')[2]}: {e}")
    return "失败：" + "；".join(errors)


def main() -> int:
    os.makedirs(OUT, exist_ok=True)
    items = json.load(open(LIST, encoding="utf-8"))
    failed = 0
    for it in items:
        status = fetch(it["name"], it["size"])
        failed += status.startswith("失败")
        print(f"{it['name']}: {status}")
    print(f"完成：{len(items) - failed}/{len(items)}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
