#!/usr/bin/env python3
"""生成 macOS 菜单栏托盘图标。

设计：圆角方形描边 + 中心对勾，语义为「每日签到」。
输出为**模板图**（template image）：只有 alpha 通道有意义，RGB 恒为黑，
由系统按菜单栏深浅色自动反色，因此在浅色/深色菜单栏下都清晰。

尺寸说明：tray-icon 的 macOS 实现会把图标强制缩放到 **18pt 高**
（见 tray-icon/src/platform_impl/macos/mod.rs），所以这里给 72px 见方的
位图，Retina 下由系统下采样，边缘平滑。

只用标准库，不依赖 Pillow。

用法：
    python3 scripts/make-tray-icon.py
产物：
    src-tauri/icons/tray.png
"""

import math
import struct
import zlib
from pathlib import Path

SIZE = 72   # 输出边长（像素）；展示时被缩放到 18pt
SS = 4      # 超采样倍数，用于抗锯齿

OUT = Path(__file__).resolve().parent.parent / "src-tauri" / "icons" / "tray.png"

# ---------------------------------------------------------------------------
# 有符号距离场（SDF）：负数表示在形状内部
# ---------------------------------------------------------------------------


def sd_round_rect(px, py, cx, cy, hw, hh, r):
    """点到圆角矩形边界的最短有符号距离。hw/hh 为半宽/半高。"""
    qx = abs(px - cx) - (hw - r)
    qy = abs(py - cy) - (hh - r)
    ax, ay = max(qx, 0.0), max(qy, 0.0)
    return math.hypot(ax, ay) + min(max(qx, qy), 0.0) - r


def sd_segment(px, py, ax, ay, bx, by):
    """点到线段的最短距离。"""
    vx, vy = bx - ax, by - ay
    wx, wy = px - ax, py - ay
    denom = vx * vx + vy * vy
    t = 0.0 if denom == 0 else max(0.0, min(1.0, (wx * vx + wy * vy) / denom))
    return math.hypot(wx - t * vx, wy - t * vy)


# ---------------------------------------------------------------------------
# 形状定义（归一化到 0..1 的设计坐标，再乘以 SIZE）
# ---------------------------------------------------------------------------

FRAME = dict(cx=0.500, cy=0.500, hw=0.400, hh=0.380, r=0.165, stroke=0.100)
# 对勾：两段折线，端点为设计坐标
CHECK = [(0.270, 0.510), (0.430, 0.670), (0.740, 0.350)]
CHECK_STROKE = 0.105


def coverage(x, y):
    """设计坐标下的点是否落在图形内（返回 0/1，由调用方做超采样平均）。"""
    frame = FRAME
    d = sd_round_rect(x, y, frame["cx"], frame["cy"], frame["hw"], frame["hh"], frame["r"])
    if abs(d) <= frame["stroke"] / 2:
        return True

    half = CHECK_STROKE / 2
    for (ax, ay), (bx, by) in zip(CHECK, CHECK[1:]):
        if sd_segment(x, y, ax, ay, bx, by) <= half:
            return True
    return False


def render():
    """超采样渲染出 RGBA 像素。"""
    rgba = bytearray(SIZE * SIZE * 4)

    for py in range(SIZE):
        for px in range(SIZE):
            hits = 0
            for sy in range(SS):
                for sx in range(SS):
                    x = (px + (sx + 0.5) / SS) / SIZE
                    y = (py + (sy + 0.5) / SS) / SIZE
                    if coverage(x, y):
                        hits += 1

            alpha = round(255 * hits / (SS * SS))
            offset = (py * SIZE + px) * 4
            # 模板图：RGB 固定为黑，颜色交由系统决定
            rgba[offset] = 0
            rgba[offset + 1] = 0
            rgba[offset + 2] = 0
            rgba[offset + 3] = alpha

    return bytes(rgba)


def write_png(path, width, height, rgba):
    """最小 PNG 编码器（8 位 RGBA，无滤波）。"""
    raw = bytearray()
    stride = width * 4
    for y in range(height):
        raw.append(0)  # filter type: None
        raw.extend(rgba[y * stride : (y + 1) * stride])

    def chunk(tag, data):
        body = tag + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    ihdr = struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)
    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
        + chunk(b"IEND", b"")
    )
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(png)
    return len(png)


def main():
    pixels = render()

    # 自检：图案必须真的画出来了，且不能糊成一整块
    opaque = sum(1 for i in range(3, len(pixels), 4) if pixels[i] > 200)
    ratio = opaque / (SIZE * SIZE)
    if not 0.05 < ratio < 0.55:
        raise SystemExit(f"图形覆盖率异常：{ratio:.1%}，请检查形状定义")

    size = write_png(OUT, SIZE, SIZE, pixels)
    print(f"已生成 {OUT}")
    print(f"  {SIZE}x{SIZE}px, {size} 字节, 实心覆盖率 {ratio:.1%}")
    print("  提示：展示尺寸由系统固定为 18pt，本文件仅作高清位图来源")


if __name__ == "__main__":
    main()
