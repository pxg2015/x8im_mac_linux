# -*- coding: utf-8 -*-
"""
邦信 图标统一（2026-10-25 品牌迁移；沿用 2026-10-23 老8 的托盘视觉规则）

品牌橙圆角块 + 白色「邦」单字（原为 X8）。2026-10-25 起对外品牌名 = 邦信，
图标内不再出现 X8 / IM 字样；单字比两字在 16~32px 小尺寸下更清楚。

本脚本以**同一套绘制逻辑**重新生成全部尺寸：先在 1024 画布上画（字号按内框自适应），
再用 LANCZOS 缩到各尺寸 —— 大图清晰、小图不糊，且与托盘 100% 同源。
托盘图本身（64）也用同一逻辑重画，保证绝对一致。

用法：python _gen_icons.py
输出：icon.ico(16..256) / icon.png(512) / icon.icns / 32x32 / 64x64 / 128x128 / 128x128@2x
      Square*Logo.png / StoreLogo.png / tray.png
"""
import os
from PIL import Image, ImageDraw, ImageFont

DIR = os.path.dirname(os.path.abspath(__file__))
ORANGE = (255, 116, 38, 255)
WHITE = (255, 255, 255, 255)
# 中文品牌字必须用中文字体，否则渲染成方块
FONT_PATH = "C:/Windows/Fonts/msyhbd.ttc"      # 微软雅黑 Bold
FONT_FALLBACK = "C:/Windows/Fonts/arialbd.ttf"
BRAND_TEXT = "邦"
MASTER = 1024          # 母版尺寸
RADIUS_RATIO = 15 / 64  # 与托盘 64px 时的 r=15 完全同比例
MARGIN_RATIO = 2 / 64   # 与托盘 64px 时的 m=2 完全同比例


def _font(size: int):
    for path in (FONT_PATH, FONT_FALLBACK):
        try:
            return ImageFont.truetype(path, size)
        except Exception:
            continue
    return ImageFont.load_default()


def draw_master(size: int) -> Image.Image:
    """按托盘的视觉规则绘制指定边长的图（圆角比例、边距比例、字号自适应都一样）。"""
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    m = max(1, round(size * MARGIN_RATIO))
    r = max(2, round(size * RADIUS_RATIO))
    d.rounded_rectangle([m, m, size - m, size - m], radius=r, fill=ORANGE)

    # 字号自适应：从大往小探，找到仍能塞进内框（留 8% 边）的最大字号
    inner_w = size - 2 * m
    max_w, max_h = inner_w * 0.90, inner_w * 0.78
    best = None
    start = int(size * 0.85)
    for fs in range(start, 4, -1):
        font = _font(fs)
        l, t, rr, b = d.textbbox((0, 0), BRAND_TEXT, font=font)
        w, h = rr - l, b - t
        if w <= max_w and h <= max_h:
            best = (font, w, h, l, t)
            break
    if best is None:
        font = _font(max(6, int(size * 0.4)))
        l, t, rr, b = d.textbbox((0, 0), BRAND_TEXT, font=font)
        best = (font, rr - l, b - t, l, t)
    font, w, h, l, t = best
    # 以包围盒中心对齐画布中心（抵消 textbbox 的 l/t 偏置）
    cx = size / 2 - (l + w / 2)
    cy = size / 2 - (t + h / 2)
    d.text((cx, cy), BRAND_TEXT, font=font, fill=WHITE)
    return img


def save_png(master: Image.Image, size: int, name: str):
    out = master.resize((size, size), Image.LANCZOS) if size != MASTER else master
    out.save(os.path.join(DIR, name))
    print(f"  {name:28s} {size}x{size}")


def main():
    master = draw_master(MASTER)
    print("母版 1024x1024 已绘制（品牌字：%s）" % BRAND_TEXT)

    # ── Windows / 通用 ──
    save_png(master, 512, "icon.png")
    save_png(master, 256, "128x128@2x.png")
    save_png(master, 128, "128x128.png")
    save_png(master, 64, "64x64.png")
    save_png(master, 32, "32x32.png")
    save_png(master, 64, "tray.png")          # 与托盘同一套画法，保证完全一致

    # ── icon.ico：exe / 快捷方式 / 任务栏 / 标题栏都取这里 ──
    ico_sizes = [16, 24, 32, 48, 64, 128, 256]
    frames = [master.resize((s, s), Image.LANCZOS) for s in ico_sizes]
    frames[-1].save(
        os.path.join(DIR, "icon.ico"),
        format="ICO",
        sizes=[(s, s) for s in ico_sizes],
        append_images=frames[:-1],
    )
    print(f"  icon.ico                     {ico_sizes}")

    # ── mac ──
    try:
        master.resize((1024, 1024), Image.LANCZOS).save(os.path.join(DIR, "icon.icns"), format="ICNS")
        print("  icon.icns                    1024 多尺寸")
    except Exception as e:
        print(f"  icon.icns 生成失败（跳过）：{e}")

    # ── Windows Store / MSIX 用的方形图 ──
    for n in (30, 44, 71, 89, 107, 142, 150, 284, 310):
        save_png(master, n, f"Square{n}x{n}Logo.png")
    save_png(master, 50, "StoreLogo.png")


if __name__ == "__main__":
    main()
