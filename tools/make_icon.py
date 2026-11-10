"""生成限速器图标：圆角方形底 + 速度表盘 + 指针 + 限速横杠。

产出：
  assets/icon.png   256x256，供窗口标题栏用
  assets/app.ico    多尺寸（16/24/32/48/64/128/256），供 exe 资源用
"""

from PIL import Image, ImageDraw
import math
import os

OUT = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "assets")
os.makedirs(OUT, exist_ok=True)

BG = (31, 111, 235)        # 底色
BG_DARK = (20, 74, 163)    # 底部渐变（扁平两色，不用连续渐变以免小尺寸糊）
WHITE = (255, 255, 255)
ARC = (120, 180, 255)      # 未点亮弧
ACCENT = (255, 209, 102)   # 指针/刻度高亮


def rounded_rect(img, box, radius, color):
    """带圆角的实心矩形（4 倍超采样后缩小，边缘平滑）"""
    s = 4
    w = int((box[2] - box[0]) * s)
    h = int((box[3] - box[1]) * s)
    layer = Image.new("RGBA", (w, h), (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)
    d.rounded_rectangle([0, 0, w - 1, h - 1], radius=radius * s, fill=color)
    layer = layer.resize((box[2] - box[0], box[3] - box[1]), Image.LANCZOS)
    img.alpha_composite(layer, (box[0], box[1]))


def draw(size: int) -> Image.Image:
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)

    u = size / 256.0  # 以 256 为设计基准的缩放系数

    # 底板：上半亮下半暗的两段式，小尺寸也能看出层次
    rounded_rect(img, (0, 0, size, size), int(56 * u), BG)
    rounded_rect(img, (0, int(size * 0.55), size, size), int(56 * u), BG_DARK)

    # 表盘：270° 弧（从左下到右下）
    pad = int(40 * u)
    box = [pad, pad, size - pad, size - pad]
    width = max(1, int(14 * u))
    d.arc(box, start=135, end=405, fill=ARC, width=width)

    # 已点亮的部分：从左下起约 55%
    cx = cy = size / 2
    r = (size - pad * 2) / 2
    for i in range(int(0.55 * 100)):
        t = i / 100.0
        a0 = math.radians(135 + 270 * t - 3)
        a1 = math.radians(135 + 270 * t + 3)
        d.pieslice(
            [cx - r, cy - r, cx + r, cy + r],
            start=math.degrees(a0),
            end=math.degrees(a1),
            fill=WHITE,
        )

    # 指针：指向约 55% 处
    ang = math.radians(135 + 270 * 0.55)
    px = cx + math.cos(ang) * r * 0.62
    py = cy + math.sin(ang) * r * 0.62
    d.line([cx, cy, px, py], fill=ACCENT, width=max(1, int(10 * u)))
    d.ellipse(
        [cx - 9 * u, cy - 9 * u, cx + 9 * u, cy + 9 * u],
        fill=WHITE,
        outline=BG_DARK,
        width=max(1, int(3 * u)),
    )

    # 底部限速横杠：表示"压到这条线"
    bar_y = int(size * 0.80)
    bar_h = max(1, int(8 * u))
    rounded_rect(
        img,
        (int(size * 0.28), bar_y - bar_h // 2, int(size * 0.72), bar_y + bar_h // 2 + bar_h),
        bar_h,
        WHITE + (255,),
    )
    return img


png = draw(256)
png.save(os.path.join(OUT, "icon.png"))

ico = draw(256)
ico.save(
    os.path.join(OUT, "app.ico"),
    sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
)
print("icon.png + app.ico written to", OUT)
