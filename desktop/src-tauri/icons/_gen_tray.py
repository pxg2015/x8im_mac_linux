from PIL import Image, ImageDraw, ImageFont

# 2026-10-23 老8：托盘图标看不清 —— 保留品牌橙底，去掉小气泡，X8 放大占满，
# 白字压橙底（对比最强），16px 托盘也认得出。
S = 64
img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
d = ImageDraw.Draw(img)

# 橙底圆角方块（品牌橙 #FF7426，铺满少留白）
m = 2; r = 15
d.rounded_rectangle([m, m, S - m, S - m], radius=r, fill=(255, 116, 38, 255))

# 尽量大的白色 X8：从大字号往下探，找到仍能塞进内框(60px)的最大字号
font_path = "C:/Windows/Fonts/arialbd.ttf"
best = None
for size in range(52, 20, -1):
    try:
        font = ImageFont.truetype(font_path, size)
    except Exception:
        font = ImageFont.load_default()
        size = 20
    l, t, rr, b = d.textbbox((0, 0), "X8", font=font)
    w, h = rr - l, b - t
    if w <= 54 and h <= 46:   # 内框 60，留 3-6px 边距
        best = (size, font, w, h, l, t)
        break
if best is None:
    font = ImageFont.truetype(font_path, 34)
    l, t, rr, b = d.textbbox((0, 0), "X8", font=font)
    best = (34, font, rr - l, b - t, l, t)

size, font, w, h, l, t = best
# anchor="mm" 以包围盒中心对齐画布中心（textbbox 的 l/t 偏置一并抵消）
cx = S / 2 - (l + w / 2)
cy = S / 2 - (t + h / 2)
d.text((cx, cy), "X8", font=font, fill=(255, 255, 255, 255))

img.save("E:/AI/X8_Im/x8im/desktop/src-tauri/icons/tray.png")
print(f"wrote tray.png {img.size}  fontsize={size}  text={w}x{h}")
