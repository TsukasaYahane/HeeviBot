"""把 .docx 转成带标题层级的 Markdown（只用标准库，不依赖 python-docx）。

用法：python tools/docx_to_md.py <输入.docx> <输出.md>

- 标题：按段落样式识别（Heading N / 标题 N，或样式的 outlineLvl），转成 `#` × N；
- 表格：每行转成 `| a | b |`；
- 图片：正文里留一个 `[图片]` 占位（知识库只收文字，图片里的内容拿不到）；
- 其余段落原样输出，连续空行折叠。
"""
import re
import sys
import zipfile
import xml.etree.ElementTree as ET

W = "{http://schemas.openxmlformats.org/wordprocessingml/2006/main}"


def heading_levels(z: zipfile.ZipFile) -> dict:
    """样式 id → 标题级别（1~9）"""
    levels = {}
    try:
        root = ET.fromstring(z.read("word/styles.xml"))
    except KeyError:
        return levels
    for st in root.iter(W + "style"):
        sid = st.get(W + "styleId", "")
        name_el = st.find(W + "name")
        name = (name_el.get(W + "val", "") if name_el is not None else "").lower()
        lvl = None
        m = re.match(r"(heading|标题)\s*(\d)", name)
        if m:
            lvl = int(m.group(2))
        ol = st.find(f"{W}pPr/{W}outlineLvl")
        if lvl is None and ol is not None:
            lvl = int(ol.get(W + "val", "9")) + 1
        if name == "title":
            lvl = 1
        if lvl and lvl <= 9:
            levels[sid] = lvl
    return levels


def para_text(p) -> tuple[str, int]:
    """段落文字 + 其中的图片数"""
    parts, images = [], 0
    for el in p.iter():
        if el.tag == W + "t":
            parts.append(el.text or "")
        elif el.tag == W + "tab":
            parts.append("\t")
        elif el.tag in (W + "br", W + "cr"):
            parts.append("\n")
        elif el.tag.endswith("}drawing") or el.tag.endswith("}pict"):
            images += 1
    return "".join(parts).strip(), images


def main() -> int:
    src, dst = sys.argv[1], sys.argv[2]
    z = zipfile.ZipFile(src)
    levels = heading_levels(z)
    body = ET.fromstring(z.read("word/document.xml")).find(W + "body")
    out, n_img, n_tbl, n_head = [], 0, 0, 0
    for el in body:
        if el.tag == W + "p":
            text, imgs = para_text(el)
            n_img += imgs
            style = el.find(f"{W}pPr/{W}pStyle")
            sid = style.get(W + "val", "") if style is not None else ""
            ol = el.find(f"{W}pPr/{W}outlineLvl")
            lvl = levels.get(sid) or (int(ol.get(W + "val", "9")) + 1 if ol is not None else None)
            if text and lvl:
                out.append(f"\n{'#' * min(lvl, 6)} {text}\n")
                n_head += 1
            elif text:
                out.append(text)
            if imgs and not text:
                out.append("[图片]")
        elif el.tag == W + "tbl":
            n_tbl += 1
            out.append("")
            for tr in el.iter(W + "tr"):
                cells = []
                for tc in tr.iter(W + "tc"):
                    cell = " ".join(t for t, _ in (para_text(p) for p in tc.iter(W + "p")) if t)
                    cells.append(cell.replace("|", "／").replace("\n", " "))
                if any(cells):
                    out.append("| " + " | ".join(cells) + " |")
            out.append("")
    md = "\n".join(out)
    md = re.sub(r"\n{3,}", "\n\n", md).strip() + "\n"
    open(dst, "w", encoding="utf-8").write(md)
    print(f"标题 {n_head} 个，表格 {n_tbl} 个，图片 {n_img} 张，正文 {len(md)} 字符 → {dst}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
