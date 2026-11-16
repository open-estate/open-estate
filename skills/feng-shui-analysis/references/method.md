# Feng shui method reference

Implemented in `src/modules/xuankong.rs` (24 mountains, Xuan Kong flying stars, Ba Zhai, Personal Kua, WMM2025
declination via `world_magnetic_model`) and `src/modules/fengshui.rs` (form-school checks, OSM geometry, terrain).

## 24 mountains (二十四山) — centers every 15°, read on a luopan (magnetic)
| Palace | Earth 地元 | Heaven 天元 | Person 人元 | Polarity (E/H/P) |
|---|---|---|---|---|
| N 坎 1 | 壬 345° | 子 0° | 癸 15° | + − − |
| NE 艮 8 | 丑 30° | 艮 45° | 寅 60° | − + + |
| E 震 3 | 甲 75° | 卯 90° | 乙 105° | + − − |
| SE 巽 4 | 辰 120° | 巽 135° | 巳 150° | − + + |
| S 离 9 | 丙 165° | 午 180° | 丁 195° | + − − |
| SW 坤 2 | 未 210° | 坤 225° | 申 240° | − + + |
| W 兑 7 | 庚 255° | 酉 270° | 辛 285° | + − − |
| NW 乾 6 | 戌 300° | 乾 315° | 亥 330° | − + + |

Boundary within ±1.5° → void line: 大空亡 between trigrams (出卦), 小空亡 within a trigram. Leaning > 3° from a
mountain's center → 兼向 (practitioners may use a 替卦 replacement chart; not computed here).

## Periods (元运), 20 years each, starting at 立春 (~Feb 4)
P1 1864–83 · P2 1884–1903 · P3 1904–23 · P4 1924–43 · P5 1944–63 · P6 1964–83 · P7 1984–2003 · P8 2004–23 ·
**P9 2024–43**. Timeliness in P9: 9 当旺 · 1 生气 · 2 次生气 · 8 退气 · 5 五黄煞 · 3/4/6/7 衰死.

## Natal chart (下卦) construction
1. Fly the period number from the center along 中→乾(NW)→兑(W)→艮(NE)→离(S)→坎(N)→坤(SW)→震(E)→巽(SE).
2. Facing star = base number in the facing palace; mountain star = base number in the sitting palace; both go to
   the center.
3. Direction: look up the trigram whose Lo Shu number equals the star; take its sub-mountain at the same position
   (地/天/人) as the facing (or sitting) mountain; yang → fly forward, yin → backward. Star 5 uses the facing (or
   sitting) palace's own trigram.
4. Patterns: 旺山旺向 (mountain-P at sitting & facing-P at facing), 上山下水 (reverse), 双星到向/到坐, 伏吟/反吟
   (a center 5 flying forward/backward), 合十 (base + star = 10 everywhere), 连珠/父母三般卦.
5. 入囚: when the current period equals the center facing star, the wealth star is "imprisoned" (relieved by open
   space/water in front, or by re-dating the house through major renovation).

## Annual star
Center = `wrap9(11 − (year mod 9))` for the solar year (2024 → 3, 2025 → 2, 2026 → 1, 2027 → 9), flown forward.

## Ba Zhai (八宅)
House trigram = sitting palace. Sector stars by trigram (生气, 天医, 延年, 伏位 / 祸害, 五鬼, 六煞, 绝命):
坎1 SE E S N / W NE NW SW · 坤2 NE W NW SW / E SE S N · 震3 S N SE E / SW NW NE W · 巽4 N S E SE / NW SW W NE ·
乾6 W NE SW NW / SE E N S · 兑7 NW SW NE W / N S SE E · 艮8 SW NW W NE / S N E SE · 离9 E SE N S / NE W SW NW.
East group 1·3·4·9; West group 2·6·7·8.

## Personal Kua (命卦)
Male: `(100 − yy) mod 9` (≥2000: `(99 − yy) mod 9`), 0→9, 5→2. Female: `(yy − 4) mod 9` (≥2000: `(yy + 6) mod 9`),
0→9, 5→8. Use the previous year for births before Feb 4.

## Form-school checks (峦头) implemented
Road arrow 路冲 (street end aligned with the house within 110 m), 反弓/玉带 (≥20° bend), busy road front/back/left
(青龙)/right (白虎), corner lot, cul-de-sac, power lines, substations, masts, cemeteries/funeral homes, hospitals,
places of worship (宫前庙后), fire/police, gas stations, freeways, water side, terrain (靠山 / 明堂 / 青龙白虎高低),
missing corners (缺角), footprint regularity, relative height, taller building behind/in front, commercial land
behind, lot shape (前窄后宽 / 前宽后窄).

## Sources
沈竹礽《沈氏玄空学》; 《紫白诀》; 《飞星赋》; 《八宅明镜》; NOAA/BGS WMM2025 for declination.
Readings are traditional associations, offered as cultural information.
