#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""把 src/ui 下的硬编码颜色 / 字号收敛到 theme token。

原则：
1. 角色优先：同一色值在 bg / border / text 上语义不同，先做「方法+字面量」精确替换。
2. 色值守恒：token 的色值等于原字面量（或就近收敛），除有意修复的对比度问题外不改外观。
3. 只处理 src/ui，不动 theme.rs 自身。
"""
import re
import pathlib

UI = pathlib.Path("A:/Linux/Voice2Word/src/ui")

# ---- 1. 角色优先的精确替换（先做，避免被通用表覆盖）----
ROLE_FIRST = [
    (".text_color(rgb(0x09090b))", ".text_color(Theme::text_on_accent())"),
    (".bg(rgb(0x09090b))", ".bg(Theme::bg_media())"),
    (".border_color(rgb(0x282832))", ".border_color(Theme::border_mid())"),
    (".border_color(rgb(0x282836))", ".border_color(Theme::border_mid())"),
    (".border_color(rgb(0x282834))", ".border_color(Theme::border_mid())"),
    (".border_color(rgb(0x242430))", ".border_color(Theme::border_subtle())"),
    (".border_color(rgb(0x252530))", ".border_color(Theme::border_subtle())"),
    (".border_color(rgb(0x1e1e2a))", ".border_color(Theme::border_subtle())"),
    (".border_color(rgb(0x1e1e26))", ".border_color(Theme::border_subtle())"),
    (".border_color(rgb(0x22222c))", ".border_color(Theme::border_subtle())"),
]

# ---- 2. 通用字面量 -> token ----
GENERAL = {
    # 背景
    "rgb(0x0e0e11)": "Theme::bg_app()",
    "rgb(0x09090b)": "Theme::text_on_accent()",
    "rgb(0x050507)": "Theme::bg_media_deep()",
    "rgb(0x0a0a0f)": "Theme::bg_deep()",
    "rgb(0x131316)": "Theme::bg_sidebar()",
    "rgb(0x131318)": "Theme::bg_sidebar()",
    "rgb(0x12121a)": "Theme::bg_sidebar()",
    "rgb(0x161619)": "Theme::bg_input()",
    "rgb(0x16161c)": "Theme::bg_input()",
    "rgb(0x16161d)": "Theme::bg_input()",
    "rgb(0x18181c)": "Theme::bg_panel()",
    "rgb(0x18181e)": "Theme::bg_panel()",
    "rgb(0x18181f)": "Theme::bg_panel()",
    "rgb(0x181822)": "Theme::bg_panel()",
    "rgb(0x181820)": "Theme::bg_inset()",
    "rgb(0x1a1a24)": "Theme::bg_raised()",
    "rgb(0x1a1a22)": "Theme::bg_raised()",
    "rgb(0x1b1b24)": "Theme::bg_raised()",
    "rgb(0x1c1c26)": "Theme::bg_raised()",
    "rgb(0x1c1c28)": "Theme::bg_raised()",
    "rgb(0x1e1e26)": "Theme::bg_disabled()",
    "rgb(0x1e1e28)": "Theme::bg_disabled()",
    "rgb(0x1e1e2a)": "Theme::bg_disabled()",
    "rgb(0x23232f)": "Theme::bg_disabled()",
    "rgb(0x202024)": "Theme::bg_card()",
    "rgb(0x22222a)": "Theme::bg_track()",
    "rgb(0x22222c)": "Theme::bg_track()",
    "rgb(0x20202c)": "Theme::bg_track()",
    "rgb(0x1f1f2a)": "Theme::bg_card_hover()",
    "rgb(0x242428)": "Theme::bg_card_hover()",
    "rgb(0x24242e)": "Theme::bg_card_hover()",
    "rgb(0x242430)": "Theme::bg_card_hover()",
    "rgb(0x252532)": "Theme::bg_card_hover()",
    "rgb(0x27272a)": "Theme::bg_card_hover()",
    "rgb(0x27272e)": "Theme::bg_hover()",
    "rgb(0x282832)": "Theme::bg_hover_strong()",
    "rgb(0x282834)": "Theme::bg_hover_strong()",
    "rgb(0x282836)": "Theme::bg_hover_strong()",
    "rgb(0x2a2a36)": "Theme::bg_hover_strong()",
    "rgb(0x2c2c36)": "Theme::bg_hover_strong()",
    # 边框
    "rgb(0x252530)": "Theme::border_subtle()",
    "rgb(0x2a2a38)": "Theme::border_mid()",
    "rgb(0x2d2d3a)": "Theme::border_mid()",
    "rgb(0x2c2c3e)": "Theme::border_mid()",
    "rgb(0x2c2c34)": "Theme::border()",
    "rgb(0x323242)": "Theme::border_strong()",
    "rgb(0x323244)": "Theme::border_strong()",
    "rgb(0x363646)": "Theme::border_strong()",
    "rgb(0x383848)": "Theme::border_strong()",
    "rgb(0x3a3a48)": "Theme::border_strong()",
    "rgb(0x383842)": "Theme::border_strong()",
    # 中性灰
    "rgb(0x3a3a44)": "Theme::bg_dot_idle()",
    "rgb(0x374151)": "Theme::bg_dot_idle()",
    "rgb(0x4a4a58)": "Theme::bg_stat_track()",
    # 文字
    "rgb(0xf3f4f6)": "Theme::text_primary()",
    "rgb(0xffffff)": "Theme::text_white()",
    "rgb(0xe2e8f0)": "Theme::text_primary()",
    "rgb(0xd1d5db)": "Theme::text_primary()",
    "rgb(0xa1a1aa)": "Theme::text_secondary()",
    "rgb(0x6b7280)": "Theme::text_disabled()",
    "rgb(0x38bdf8)": "Theme::text_code()",
    # 强调色
    "rgb(0x6366f1)": "Theme::accent_primary()",
    "rgb(0x10b981)": "Theme::accent_mint()",
    "rgb(0x059669)": "Theme::accent_mint_deep()",
    "rgb(0xf43f5e)": "Theme::accent_red()",
    "rgb(0xe11d48)": "Theme::accent_red_strong()",
    "rgb(0xf59e0b)": "Theme::accent_orange()",
    "rgb(0x2563eb)": "Theme::accent_primary()",
    "rgb(0x1a2420)": "Theme::tint_mint_soft()",
    "rgb(0x22322a)": "Theme::tint_mint_badge()",
    # 透明 / 遮罩
    "rgba(0x00000000)": "Theme::transparent()",
    "rgba(0x000000cc)": "Theme::bg_scrim()",
    "rgba(0x000000dd)": "Theme::bg_scrim()",
    "rgba(0x0a0a0fb8)": "Theme::bg_scrim_soft()",
    # 中性叠加
    "rgba(0xffffff0d)": "Theme::tint_neutral()",
    "rgba(0xffffff20)": "Theme::tint_neutral_border()",
    "rgba(0xffffff28)": "Theme::tint_neutral_border()",
    # 关键对比度修复：原本 1.93:1 的日期文字
    "rgba(0xffffff33)": "Theme::text_muted()",
    # 薄荷 tint
    "rgba(0x10b98114)": "Theme::tint_mint_soft()",
    "rgba(0x10b98115)": "Theme::tint_mint_soft()",
    "rgba(0x10b98118)": "Theme::tint_mint_soft()",
    "rgba(0x10b9811a)": "Theme::tint_mint_soft()",
    "rgba(0x10b9811c)": "Theme::tint_mint_soft()",
    "rgba(0x10b9811f)": "Theme::tint_mint_soft()",
    "rgba(0x10b98120)": "Theme::tint_mint_badge()",
    "rgba(0x10b98125)": "Theme::tint_mint_badge()",
    "rgba(0x10b98126)": "Theme::tint_mint_badge()",
    "rgba(0x10b98128)": "Theme::tint_mint_badge()",
    "rgba(0x10b98130)": "Theme::tint_mint_border()",
    "rgba(0x10b98133)": "Theme::tint_mint_border()",
    "rgba(0x10b98138)": "Theme::tint_mint_border()",
    "rgba(0x10b9813f)": "Theme::tint_mint_border()",
    "rgba(0x10b98144)": "Theme::tint_mint_border()",
    "rgba(0x10b98150)": "Theme::tint_mint_border()",
    "rgba(0x10b98166)": "Theme::tint_mint_border()",
    "rgba(0x10b98188)": "Theme::tint_mint_border()",
    # 蓝 tint
    "rgba(0x38bdf80d)": "Theme::tint_blue_soft()",
    "rgba(0x38bdf818)": "Theme::tint_blue_soft()",
    "rgba(0x38bdf81a)": "Theme::tint_blue_soft()",
    "rgba(0x38bdf81c)": "Theme::tint_blue_soft()",
    "rgba(0x38bdf81f)": "Theme::tint_blue_soft()",
    "rgba(0x38bdf826)": "Theme::tint_blue_badge()",
    "rgba(0x38bdf82e)": "Theme::tint_blue_border()",
    "rgba(0x38bdf833)": "Theme::tint_blue_border()",
    "rgba(0x38bdf866)": "Theme::tint_blue_border()",
    "rgba(0x38bdf877)": "Theme::tint_blue_border()",
    "rgba(0x38bdf888)": "Theme::tint_blue_border()",
    "rgba(0x38bdf8aa)": "Theme::tint_blue_border()",
    # 红 tint
    "rgba(0xf43f5e0a)": "Theme::tint_red_soft()",
    "rgba(0xf43f5e14)": "Theme::tint_red_soft()",
    "rgba(0xf43f5e15)": "Theme::tint_red_soft()",
    "rgba(0xf43f5e18)": "Theme::tint_red_soft()",
    "rgba(0xf43f5e20)": "Theme::tint_red_border()",
    "rgba(0xf43f5e30)": "Theme::tint_red_border()",
    "rgba(0xf43f5e38)": "Theme::tint_red_border()",
    "rgba(0xf43f5e66)": "Theme::tint_red_border()",
    # 主重音 tint
    "rgba(0x6366f116)": "Theme::tint_primary_soft()",
    "rgba(0x6366f118)": "Theme::tint_primary_soft()",
    "rgba(0x6366f122)": "Theme::tint_primary_badge()",
    "rgba(0x6366f133)": "Theme::tint_primary_border()",
}

# ---- 3. 字号阶梯收敛（0.5px 碎片归并到 10 / 11 / 12 / 13 / 14）----
FONT = {
    "text_size(px(9.0))": "text_size(px(10.0))",
    "text_size(px(9.5))": "text_size(px(10.0))",
    "text_size(px(10.5))": "text_size(px(11.0))",
    "text_size(px(11.5))": "text_size(px(12.0))",
    "text_size(px(12.5))": "text_size(px(13.0))",
    "text_size(px(13.5))": "text_size(px(14.0))",
}


def main():
    files = sorted(p for p in UI.rglob("*.rs") if p.name != "theme.rs")
    total_c = total_f = 0
    for path in files:
        src = path.read_text(encoding="utf-8")
        orig = src
        c = 0
        for old, new in ROLE_FIRST:
            n = src.count(old)
            if n:
                src = src.replace(old, new)
                c += n
        for old, new in GENERAL.items():
            n = src.count(old)
            if n:
                src = src.replace(old, new)
                c += n
        f = 0
        for old, new in FONT.items():
            n = src.count(old)
            if n:
                src = src.replace(old, new)
                f += n
        if src != orig:
            path.write_text(src, encoding="utf-8")
        if c or f:
            print(f"  {path.relative_to(UI)}: 颜色 {c} 处, 字号 {f} 处")
        total_c += c
        total_f += f
    print(f"\n合计：颜色 {total_c} 处，字号 {total_f} 处")

    # 残留检查
    left = {}
    for path in files:
        for m in re.findall(r"rgba?\(0x[0-9a-fA-F]{6,8}\)", path.read_text(encoding="utf-8")):
            left[m] = left.get(m, 0) + 1
    if left:
        print("\n未映射的残留字面量：")
        for k, v in sorted(left.items(), key=lambda x: -x[1]):
            print(f"  {k} x{v}")
    else:
        print("\n没有残留的颜色字面量。")


if __name__ == "__main__":
    main()
