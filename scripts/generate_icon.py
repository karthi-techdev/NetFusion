import os
import math
from PIL import Image, ImageDraw

def create_netfusion_icon():
    os.makedirs("assets", exist_ok=True)
    size = 512
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    draw = ImageDraw.Draw(img)

    center = size / 2
    radius = size * 0.44

    # Outer glow / background circle
    # Dark modern high-tech slate
    for r in range(int(radius + 8), int(radius - 2), -1):
        alpha = int(255 * (1.0 - (r - radius) / 10.0)) if r > radius else 255
        draw.ellipse(
            [center - r, center - r, center + r, center + r],
            fill=(15, 23, 42, min(255, max(0, alpha)))
        )

    # Vibrant glowing gradient border (Bonded Ring: Cyan to Purple-Blue)
    ring_width = 16
    for i in range(360):
        angle = math.radians(i)
        next_angle = math.radians(i + 1.5)
        # Gradient from Cyan (0, 240, 255) to Indigo/Violet (120, 50, 255)
        t = (math.sin(angle) + 1) / 2
        r_c = int(0 * (1 - t) + 120 * t)
        g_c = int(240 * (1 - t) + 60 * t)
        b_c = 255
        
        p1 = (center + (radius - ring_width) * math.cos(angle), center + (radius - ring_width) * math.sin(angle))
        p2 = (center + radius * math.cos(angle), center + radius * math.sin(angle))
        p3 = (center + radius * math.cos(next_angle), center + radius * math.sin(next_angle))
        p4 = (center + (radius - ring_width) * math.cos(next_angle), center + (radius - ring_width) * math.sin(next_angle))
        
        draw.polygon([p1, p2, p3, p4], fill=(r_c, g_c, b_c, 255))

    # Inner Core: Two streams converging into one (Fusion symbol)
    # Stream 1 (Left / WiFi - Cyan): Curved arrow entering from bottom-left converging upward
    # Stream 2 (Right / 5G-Ethernet - Magenta/Purple): Curved arrow from bottom-right converging upward
    # Converged central beam (Upward high speed arrow in Bright White/Cyan)

    # Central Fusion Arrow (Converged output beam)
    arrow_color = (255, 255, 255, 255)
    cyan_glow = (0, 245, 255, 255)
    purple_glow = (147, 51, 234, 255)

    # Top Arrow Head
    arrow_pts = [
        (center, center - 130),       # Tip
        (center + 60, center - 40),   # Right wing
        (center + 24, center - 40),   # Right inner
        (center + 24, center + 45),   # Right shaft
        (center - 24, center + 45),   # Left shaft
        (center - 24, center - 40),   # Left inner
        (center - 60, center - 40),   # Left wing
    ]
    draw.polygon(arrow_pts, fill=cyan_glow)

    # Inner bright core of arrow
    inner_arrow_pts = [
        (center, center - 110),
        (center + 40, center - 42),
        (center + 14, center - 42),
        (center + 14, center + 40),
        (center - 14, center + 40),
        (center - 14, center - 42),
        (center - 40, center - 42),
    ]
    draw.polygon(inner_arrow_pts, fill=arrow_color)

    # Left Convergence Stream (Curving into center shaft)
    left_stream = [
        (center - 115, center + 105),
        (center - 75, center + 125),
        (center - 20, center + 55),
        (center - 45, center + 40),
    ]
    draw.polygon(left_stream, fill=cyan_glow)

    # Right Convergence Stream (Curving into center shaft)
    right_stream = [
        (center + 115, center + 105),
        (center + 75, center + 125),
        (center + 20, center + 55),
        (center + 45, center + 40),
    ]
    draw.polygon(right_stream, fill=purple_glow)

    # Decorative connection nodes (circles at bottom inputs)
    node_r = 16
    draw.ellipse([center - 115 - node_r, center + 105 - node_r, center - 115 + node_r, center + 105 + node_r], fill=cyan_glow)
    draw.ellipse([center - 115 - 8, center + 105 - 8, center - 115 + 8, center + 105 + 8], fill=(255, 255, 255, 255))

    draw.ellipse([center + 115 - node_r, center + 105 - node_r, center + 115 + node_r, center + 105 + node_r], fill=purple_glow)
    draw.ellipse([center + 115 - 8, center + 105 - 8, center + 115 + 8, center + 105 + 8], fill=(255, 255, 255, 255))

    # Save PNG
    img.save("assets/icon.png", format="PNG")
    print("Saved assets/icon.png")

    # Generate multi-size ICO (256, 128, 64, 48, 32, 16)
    sizes = [(256, 256), (128, 128), (64, 64), (48, 48), (32, 32), (16, 16)]
    img.save("assets/icon.ico", format="ICO", sizes=sizes)
    print("Saved assets/icon.ico with sizes:", sizes)

if __name__ == "__main__":
    create_netfusion_icon()
