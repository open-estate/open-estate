// Xuan Kong Flying Stars (玄空飞星), 24 Mountains (二十四山), Eight Mansions (八宅),
// Personal Kua (命卦), and NOAA WMM2025 geomagnetic declination via `world_magnetic_model`.

use chrono::{Datelike, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use world_magnetic_model::time::Date as WmmDate;
use world_magnetic_model::uom::si::angle::degree;
use world_magnetic_model::uom::si::f32::{Angle, Length};
use world_magnetic_model::uom::si::length::meter;
use world_magnetic_model::GeomagneticField;

use crate::shared::round_n;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct MountainDef {
    pub i: usize,
    pub zh: &'static str,
    pub py: &'static str,
    pub palace: &'static str,
    pub pos: u8, // 0 = 地元 earth, 1 = 天元 heaven, 2 = 人元 person
    pub yang: bool,
    pub center: f64,
}

pub const MOUNTAINS: [MountainDef; 24] = [
    MountainDef { i: 0, zh: "子", py: "Zi", palace: "N", pos: 1, yang: false, center: 0.0 },
    MountainDef { i: 1, zh: "癸", py: "Gui", palace: "N", pos: 2, yang: false, center: 15.0 },
    MountainDef { i: 2, zh: "丑", py: "Chou", palace: "NE", pos: 0, yang: false, center: 30.0 },
    MountainDef { i: 3, zh: "艮", py: "Gen", palace: "NE", pos: 1, yang: true, center: 45.0 },
    MountainDef { i: 4, zh: "寅", py: "Yin", palace: "NE", pos: 2, yang: true, center: 60.0 },
    MountainDef { i: 5, zh: "甲", py: "Jia", palace: "E", pos: 0, yang: true, center: 75.0 },
    MountainDef { i: 6, zh: "卯", py: "Mao", palace: "E", pos: 1, yang: false, center: 90.0 },
    MountainDef { i: 7, zh: "乙", py: "Yi", palace: "E", pos: 2, yang: false, center: 105.0 },
    MountainDef { i: 8, zh: "辰", py: "Chen", palace: "SE", pos: 0, yang: false, center: 120.0 },
    MountainDef { i: 9, zh: "巽", py: "Xun", palace: "SE", pos: 1, yang: true, center: 135.0 },
    MountainDef { i: 10, zh: "巳", py: "Si", palace: "SE", pos: 2, yang: true, center: 150.0 },
    MountainDef { i: 11, zh: "丙", py: "Bing", palace: "S", pos: 0, yang: true, center: 165.0 },
    MountainDef { i: 12, zh: "午", py: "Wu", palace: "S", pos: 1, yang: false, center: 180.0 },
    MountainDef { i: 13, zh: "丁", py: "Ding", palace: "S", pos: 2, yang: false, center: 195.0 },
    MountainDef { i: 14, zh: "未", py: "Wei", palace: "SW", pos: 0, yang: false, center: 210.0 },
    MountainDef { i: 15, zh: "坤", py: "Kun", palace: "SW", pos: 1, yang: true, center: 225.0 },
    MountainDef { i: 16, zh: "申", py: "Shen", palace: "SW", pos: 2, yang: true, center: 240.0 },
    MountainDef { i: 17, zh: "庚", py: "Geng", palace: "W", pos: 0, yang: true, center: 255.0 },
    MountainDef { i: 18, zh: "酉", py: "You", palace: "W", pos: 1, yang: false, center: 270.0 },
    MountainDef { i: 19, zh: "辛", py: "Xin", palace: "W", pos: 2, yang: false, center: 285.0 },
    MountainDef { i: 20, zh: "戌", py: "Xu", palace: "NW", pos: 0, yang: false, center: 300.0 },
    MountainDef { i: 21, zh: "乾", py: "Qian", palace: "NW", pos: 1, yang: true, center: 315.0 },
    MountainDef { i: 22, zh: "亥", py: "Hai", palace: "NW", pos: 2, yang: true, center: 330.0 },
    MountainDef { i: 23, zh: "壬", py: "Ren", palace: "N", pos: 0, yang: true, center: 345.0 },
];

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PalaceDef {
    pub key: &'static str,
    pub num: u8,
    pub zh: &'static str,
    pub dir: &'static str,
    pub en: &'static str,
    pub trigram: &'static str,
    pub element: &'static str,
}

pub const PALACES_LIST: [PalaceDef; 9] = [
    PalaceDef { key: "N", num: 1, zh: "坎", dir: "北", en: "North", trigram: "Kan ☵", element: "Water" },
    PalaceDef { key: "SW", num: 2, zh: "坤", dir: "西南", en: "Southwest", trigram: "Kun ☷", element: "Earth" },
    PalaceDef { key: "E", num: 3, zh: "震", dir: "东", en: "East", trigram: "Zhen ☳", element: "Wood" },
    PalaceDef { key: "SE", num: 4, zh: "巽", dir: "东南", en: "Southeast", trigram: "Xun ☴", element: "Wood" },
    PalaceDef { key: "C", num: 5, zh: "中", dir: "中宫", en: "Center", trigram: "—", element: "Earth" },
    PalaceDef { key: "NW", num: 6, zh: "乾", dir: "西北", en: "Northwest", trigram: "Qian ☰", element: "Metal" },
    PalaceDef { key: "W", num: 7, zh: "兑", dir: "西", en: "West", trigram: "Dui ☱", element: "Metal" },
    PalaceDef { key: "NE", num: 8, zh: "艮", dir: "东北", en: "Northeast", trigram: "Gen ☶", element: "Earth" },
    PalaceDef { key: "S", num: 9, zh: "离", dir: "南", en: "South", trigram: "Li ☲", element: "Fire" },
];

pub const FLIGHT_ORDER: [&str; 9] = ["C", "NW", "W", "NE", "S", "N", "SW", "E", "SE"];

pub const LO_SHU_SOUTH_UP: [[&str; 3]; 3] = [
    ["SE", "S", "SW"],
    ["E", "C", "W"],
    ["NE", "N", "NW"],
];

pub fn palace_info(palace: &str) -> PalaceDef {
    PALACES_LIST
        .iter()
        .copied()
        .find(|p| p.key == palace)
        .unwrap_or(PALACES_LIST[4])
}

pub fn palace_of_num(n: u8) -> &'static str {
    match n {
        1 => "N",
        2 => "SW",
        3 => "E",
        4 => "SE",
        6 => "NW",
        7 => "W",
        8 => "NE",
        9 => "S",
        _ => "C",
    }
}

pub fn opposite_palace(p: &str) -> &'static str {
    match p {
        "N" => "S",
        "S" => "N",
        "E" => "W",
        "W" => "E",
        "NE" => "SW",
        "SW" => "NE",
        "SE" => "NW",
        "NW" => "SE",
        _ => "C",
    }
}

#[inline]
pub fn norm360(deg: f64) -> f64 {
    ((deg % 360.0) + 360.0) % 360.0
}

#[inline]
pub fn wrap9(x: i32) -> u8 {
    ((((x - 1) % 9) + 9) % 9 + 1) as u8
}

/// Official NOAA WMM2025 magnetic declination in degrees (East positive, West negative).
pub fn wmm_declination(lat_deg: f64, lon_deg: f64, elev_m: f64, date: Option<NaiveDate>) -> f64 {
    let dt = date.unwrap_or_else(|| Utc::now().date_naive());
    let clamped_year = dt.year().clamp(2025, 2029);
    let max_ord = if (clamped_year % 4 == 0 && clamped_year % 100 != 0) || (clamped_year % 400 == 0) {
        366
    } else {
        365
    };
    let ordinal = (dt.ordinal() as u16).clamp(1, max_ord);
    let wmm_date = WmmDate::from_ordinal_date(clamped_year, ordinal)
        .unwrap_or_else(|_| WmmDate::from_ordinal_date(2026, 180).expect("valid WMM date"));
    let h_m = elev_m.clamp(-900.0, 800_000.0) as f32;
    let lat = lat_deg.clamp(-89.999, 89.999) as f32;
    let lon = lon_deg.clamp(-180.0, 180.0) as f32;

    match GeomagneticField::new(
        Length::new::<meter>(h_m),
        Angle::new::<degree>(lat),
        Angle::new::<degree>(lon),
        wmm_date,
    ) {
        Ok(field) => field.declination().get::<degree>() as f64,
        Err(_) => 0.0,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountainReading {
    pub i: usize,
    pub zh: &'static str,
    pub py: &'static str,
    pub palace: &'static str,
    pub pos: u8,
    pub yang: bool,
    pub center: f64,
    pub deg: f64,
    pub offset_from_center: f64,
    pub boundary_dist: f64,
    pub neighbor: MountainDef,
    pub void_line: Option<&'static str>,
    pub jian: Option<String>,
}

/// Mountain for a magnetic bearing + distance to nearest boundary & void-line (空亡) detection.
pub fn mountain_of(deg: f64) -> MountainReading {
    let d = norm360(deg);
    let shifted = norm360(d + 7.5);
    let idx = ((shifted / 15.0).floor() as usize) % 24;
    let m = MOUNTAINS[idx];
    let off = shifted % 15.0; // 0..15 within the mountain
    let to_low = off;
    let to_high = 15.0 - off;
    let near_idx = if to_low < to_high {
        (idx + 23) % 24
    } else {
        (idx + 1) % 24
    };
    let near_m = MOUNTAINS[near_idx];
    let boundary_dist = to_low.min(to_high);
    let cross_trigram = near_m.palace != m.palace;
    let void_line = if boundary_dist < 1.5 {
        Some(if cross_trigram {
            "大空亡 (major void line, 出卦)"
        } else {
            "小空亡 (minor void line)"
        })
    } else {
        None
    };
    let offset_from_center = off - 7.5;
    let jian = if offset_from_center.abs() > 3.0 {
        Some(format!(
            "兼{} (leaning {:.1}° toward {})",
            near_m.zh,
            offset_from_center.abs(),
            near_m.zh
        ))
    } else {
        None
    };

    MountainReading {
        i: m.i,
        zh: m.zh,
        py: m.py,
        palace: m.palace,
        pos: m.pos,
        yang: m.yang,
        center: m.center,
        deg: d,
        offset_from_center,
        boundary_dist,
        neighbor: near_m,
        void_line,
        jian,
    }
}

/// Construction period (元运) 1..=9 for a year (period boundary at 立春 ~Feb 4).
pub fn period_of_year(year: i32, month: u32) -> u8 {
    let y = if month < 2 { year - 1 } else { year };
    let p = (y - 1864).div_euclid(20).rem_euclid(9);
    wrap9(p + 1)
}

pub fn period_range(p: u8) -> [i32; 2] {
    let start = 1864 + (p as i32 - 1) * 20;
    [start, start + 19]
}

/// Fly a star 1..=9 through the Lo Shu 9 palaces.
pub fn fly(center: u8, forward: bool) -> BTreeMap<&'static str, u8> {
    let mut out = BTreeMap::new();
    for (k, &p) in FLIGHT_ORDER.iter().enumerate() {
        let v = if forward {
            wrap9(center as i32 + k as i32)
        } else {
            wrap9(center as i32 - k as i32)
        };
        out.insert(p, v);
    }
    out
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChartFeature {
    pub key: &'static str,
    pub zh: &'static str,
    pub en: &'static str,
    pub good: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlyingStarChart {
    pub period: u8,
    pub name: String,
    pub name_en: String,
    pub facing_mountain: MountainReading,
    pub sitting_mountain: MountainDef,
    pub base: BTreeMap<&'static str, u8>,
    pub mountain: BTreeMap<&'static str, u8>,
    pub facing: BTreeMap<&'static str, u8>,
    pub facing_flight: &'static str,
    pub mountain_flight: &'static str,
    pub features: Vec<ChartFeature>,
}

/// Natal Xuan Kong Flying Star chart (下卦) for a construction period and magnetic facing bearing.
pub fn flying_star_chart(period: u8, facing_deg_mag: f64) -> FlyingStarChart {
    let fm = mountain_of(facing_deg_mag);
    let sm = MOUNTAINS[(fm.i + 12) % 24];
    let base = fly(period, true);
    let f_star = base[fm.palace];
    let m_star = base[sm.palace];

    let is_forward = |star: u8, pos: u8, own_palace: &'static str| -> bool {
        let pal = if star == 5 {
            own_palace
        } else {
            palace_of_num(star)
        };
        MOUNTAINS
            .iter()
            .find(|x| x.palace == pal && x.pos == pos)
            .map(|x| x.yang)
            .unwrap_or(true)
    };

    let f_fwd = is_forward(f_star, fm.pos, fm.palace);
    let m_fwd = is_forward(m_star, sm.pos, sm.palace);
    let facing = fly(f_star, f_fwd);
    let mountain = fly(m_star, m_fwd);

    let mut chart = FlyingStarChart {
        period,
        name: format!("{}山{}向", sm.zh, fm.zh),
        name_en: format!("Sitting {} ({}), facing {} ({})", sm.py, sm.palace, fm.py, fm.palace),
        facing_mountain: fm,
        sitting_mountain: sm,
        base,
        mountain,
        facing,
        facing_flight: if f_fwd { "forward" } else { "reverse" },
        mountain_flight: if m_fwd { "forward" } else { "reverse" },
        features: Vec::new(),
    };
    chart.features = chart_features(&chart);
    chart
}

/// Classical Xuan Kong chart patterns (旺山旺向, 上山下水, 双星到向/坐, 伏吟/反吟, 合十, 三般卦).
pub fn chart_features(c: &FlyingStarChart) -> Vec<ChartFeature> {
    let f_pal = c.facing_mountain.palace;
    let s_pal = c.sitting_mountain.palace;
    let p = c.period;
    let mut out = Vec::new();

    if c.mountain[s_pal] == p && c.facing[f_pal] == p {
        out.push(ChartFeature {
            key: "wangshan-wangxiang",
            zh: "旺山旺向",
            en: "Prosperous Sitting & Facing (for its construction period)",
            good: Some(true),
        });
    } else if c.mountain[f_pal] == p && c.facing[s_pal] == p {
        out.push(ChartFeature {
            key: "shangshan-xiashui",
            zh: "上山下水",
            en: "Reversed stars (for its construction period)",
            good: Some(false),
        });
    } else if c.mountain[f_pal] == p && c.facing[f_pal] == p {
        out.push(ChartFeature {
            key: "shuangxing-daoxiang",
            zh: "双星到向",
            en: "Double stars at facing",
            good: None,
        });
    } else if c.mountain[s_pal] == p && c.facing[s_pal] == p {
        out.push(ChartFeature {
            key: "shuangxing-daozuo",
            zh: "双星到坐",
            en: "Double stars at sitting",
            good: None,
        });
    }

    let is_fuyin = |stars: &BTreeMap<&'static str, u8>| {
        FLIGHT_ORDER
            .iter()
            .all(|&pal| pal == "C" || stars[pal] == palace_info(pal).num)
    };
    let is_fanyin = |stars: &BTreeMap<&'static str, u8>| {
        FLIGHT_ORDER
            .iter()
            .all(|&pal| pal == "C" || stars[pal] == palace_info(opposite_palace(pal)).num)
    };

    if is_fuyin(&c.facing) || is_fuyin(&c.mountain) {
        out.push(ChartFeature {
            key: "fuyin",
            zh: "伏吟",
            en: "Fu Yin (stars duplicate the Lo Shu)",
            good: Some(false),
        });
    }
    if is_fanyin(&c.facing) || is_fanyin(&c.mountain) {
        out.push(ChartFeature {
            key: "fanyin",
            zh: "反吟",
            en: "Fan Yin (stars oppose the Lo Shu)",
            good: Some(false),
        });
    }
    if FLIGHT_ORDER.iter().all(|&pal| c.base[pal] + c.facing[pal] == 10) {
        out.push(ChartFeature {
            key: "heshi-xiang",
            zh: "合十 (向)",
            en: "Combination of Ten (facing stars)",
            good: Some(true),
        });
    }
    if FLIGHT_ORDER.iter().all(|&pal| c.base[pal] + c.mountain[pal] == 10) {
        out.push(ChartFeature {
            key: "heshi-shan",
            zh: "合十 (山)",
            en: "Combination of Ten (mountain stars)",
            good: Some(true),
        });
    }

    let pearl = FLIGHT_ORDER.iter().all(|&pal| {
        let mut a = [c.mountain[pal], c.base[pal], c.facing[pal]];
        a.sort_unstable();
        a[1] - a[0] == 1 && a[2] - a[1] == 1
    });
    if pearl {
        out.push(ChartFeature {
            key: "lianzhu",
            zh: "连珠三般卦",
            en: "Pearl String",
            good: Some(true),
        });
    }

    let parent = FLIGHT_ORDER.iter().all(|&pal| {
        let m3 = c.mountain[pal] % 3;
        let b3 = c.base[pal] % 3;
        let f3 = c.facing[pal] % 3;
        m3 == b3 && b3 == f3
    });
    if parent {
        out.push(ChartFeature {
            key: "fumu",
            zh: "父母三般卦",
            en: "Parent String",
            good: Some(true),
        });
    }

    out
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Timeliness {
    pub key: &'static str,
    pub zh: &'static str,
    pub en: &'static str,
    pub score: i32,
}

pub fn timeliness(star: u8, current_period: u8) -> Timeliness {
    if star == 5 && current_period != 5 {
        return Timeliness {
            key: "sha",
            zh: "五黄煞",
            en: "Hostile (5 Yellow)",
            score: -3,
        };
    }
    let d = wrap9(star as i32 - current_period as i32 + 9) % 9;
    match d {
        0 => Timeliness {
            key: "wang",
            zh: "当旺",
            en: "Prosperous (current)",
            score: 3,
        },
        1 => Timeliness {
            key: "sheng",
            zh: "生气",
            en: "Growing (next)",
            score: 2,
        },
        2 => Timeliness {
            key: "cisheng",
            zh: "次生气",
            en: "Future growing",
            score: 1,
        },
        8 => Timeliness {
            key: "tui",
            zh: "退气",
            en: "Retreating (previous)",
            score: 0,
        },
        _ => Timeliness {
            key: "si",
            zh: "死/煞气",
            en: "Dormant/hostile",
            score: -1,
        },
    }
}

pub fn annual_center(year: i32) -> u8 {
    wrap9(11 - year.rem_euclid(9))
}

pub fn annual_chart(year: i32) -> BTreeMap<&'static str, u8> {
    fly(annual_center(year), true)
}

pub fn pair_reading_zh(m: u8, f: u8) -> Option<String> {
    let pair = match (m, f) {
        (1, 4) | (4, 1) => Some("一四同宫，准发科名，利文昌学业"),
        (1, 6) | (6, 1) => Some("一六官星相会，利仕途晋升与掌权"),
        (6, 8) | (8, 6) => Some("六八武曲左辅，主富贵与稳健事业"),
        (8, 9) | (9, 8) => Some("八九喜庆相临，利置业添丁庆贺"),
        (2, 8) | (8, 2) => Some("二八合十土局，利田宅不动产积累"),
        (3, 9) | (9, 3) => Some("三九木火通明，主才智敏捷、声名显达"),
        (4, 9) | (9, 4) => Some("四九木火通明，利文思创意与名望"),
        (1, 9) | (9, 1) => Some("一九水火既济（逢煞则需防火水相冲）"),
        (2, 5) | (5, 2) => Some("二五交加必损主，主病符灾晦，宜用铜器金气泄化"),
        (2, 3) | (3, 2) => Some("二三斗牛煞，易生口舌是非与争执"),
        (6, 7) | (7, 6) => Some("六七交剑煞，慎防刀伤争讼或破耗"),
        (3, 7) | (7, 3) => Some("三七穿心煞，防盗贼破财与磕碰损伤"),
        (7, 9) | (9, 7) => Some("七九回禄之象，注意厨房电路与火险"),
        (6, 9) | (9, 6) => Some("六九火烧天门，留意家中男主人与头肺健康"),
        (2, 9) | (9, 2) => Some("二九火炎生土，恐助病符，宜清静泄土"),
        (5, 9) | (9, 5) => Some("五九紫黄毒药，切忌动土与大红强火布置"),
        (3, 4) | (4, 3) => Some("三四碧绿风魔，防情绪烦躁与是非风波"),
        (1, 8) | (8, 1) => Some("一八水土相会，需辛勤积累方成财"),
        (2, 6) | (6, 2) => Some("二六巨富之象，利置业积财"),
        _ => None,
    };
    if let Some(s) = pair {
        return Some(s.to_string());
    }
    if m == 5 || f == 5 {
        return Some("五黄廉贞飞临，宜静不宜动，忌装修动土".to_string());
    }
    if m == f {
        return Some(format!("伏吟双{m}星同宫，吉者更吉，凶者宜静化"));
    }
    None
}

pub fn pair_reading(m: u8, f: u8) -> Option<String> {
    let pair = match (m, f) {
        (1, 4) | (4, 1) => Some("文昌 Scholarly success, writing, romance"),
        (1, 6) | (6, 1) => Some("官星 Promotion, authority"),
        (6, 8) | (8, 6) => Some("武曲左辅 Wealth & steady career"),
        (8, 9) | (9, 8) => Some("喜庆 Celebrations, happy events"),
        (2, 8) | (8, 2) => Some("土局 Property/real-estate gains"),
        (3, 9) | (9, 3) => Some("木火通明 Brilliance, intelligence"),
        (4, 9) | (9, 4) => Some("木火通明 Brilliance, creativity"),
        (1, 9) | (9, 1) => Some("水火既济 Balance (or conflict if afflicted)"),
        (2, 5) | (5, 2) => Some("病符五黄 Illness & mishaps — remedy with metal"),
        (2, 3) | (3, 2) => Some("斗牛煞 Quarrels, disputes"),
        (6, 7) | (7, 6) => Some("交剑煞 Conflict, theft"),
        (3, 7) | (7, 3) => Some("穿心煞 Robbery, injury"),
        (7, 9) | (9, 7) => Some("回禄 Fire hazard (watch wiring/kitchen)"),
        (6, 9) | (9, 6) => Some("火烧天门 Elder-male health, head/lung strain"),
        (2, 9) | (9, 2) => Some("火生病符 Illness amplified by fire"),
        (5, 9) | (9, 5) => Some("火生五黄 Mishaps amplified by fire"),
        (3, 4) | (4, 3) => Some("碧绿风魔 Scandal, instability"),
        (1, 8) | (8, 1) => Some("水土 Wealth with effort"),
        (2, 6) | (6, 2) => Some("富 Wealth through property"),
        _ => None,
    };
    if let Some(s) = pair {
        return Some(s.to_string());
    }
    if m == 5 || f == 5 {
        return Some("五黄 Yellow-5 present — keep quiet, avoid renovation".to_string());
    }
    if m == f {
        return Some(format!("双{m} Doubled {m}"));
    }
    None
}

/// Convert a `FlyingStarChart` into the JSON structure rendered by `template.html`,
/// including both `palaces` array and 3x3 `grid` representation.
pub fn chart_to_json(
    c: &FlyingStarChart,
    current_period: u8,
    annual_stars: &BTreeMap<&'static str, u8>,
) -> Value {
    let palaces: Vec<Value> = FLIGHT_ORDER
        .iter()
        .map(|&p| {
            let info = palace_info(p);
            let m = c.mountain[p];
            let f = c.facing[p];
            let b = c.base[p];
            let mt = timeliness(m, current_period);
            let ft = timeliness(f, current_period);
            json!({
                "palace": p,
                "num": info.num,
                "zh": info.zh,
                "dir": info.dir,
                "en": info.en,
                "trigram": info.trigram,
                "element": info.element,
                "base": b,
                "mountain": m,
                "facing": f,
                "annual": annual_stars.get(p).copied(),
                "mountainTimeliness": mt,
                "facingTimeliness": ft,
                "reading": pair_reading(m, f),
                "readingZh": pair_reading_zh(m, f),
            })
        })
        .collect();

    let grid_3x3: Vec<Vec<Value>> = LO_SHU_SOUTH_UP
        .iter()
        .map(|row| {
            row.iter()
                .map(|&p| {
                    let info = palace_info(p);
                    json!({
                        "palace": p,
                        "zh": info.zh,
                        "dir": info.dir,
                        "base": c.base[p],
                        "mountain": c.mountain[p],
                        "facing": c.facing[p],
                        "annual": annual_stars.get(p).copied(),
                    })
                })
                .collect()
        })
        .collect();

    json!({
        "name": c.name,
        "nameEn": c.name_en,
        "period": c.period,
        "facingMountain": {
            "zh": c.facing_mountain.zh,
            "py": c.facing_mountain.py,
            "palace": c.facing_mountain.palace,
            "center": round_n(c.facing_mountain.center, 1),
        },
        "sittingMountain": {
            "zh": c.sitting_mountain.zh,
            "py": c.sitting_mountain.py,
            "palace": c.sitting_mountain.palace,
            "center": round_n(c.sitting_mountain.center, 1),
        },
        "facingFlight": c.facing_flight,
        "mountainFlight": c.mountain_flight,
        "features": c.features,
        "palaces": palaces,
        "grid": grid_3x3,
    })
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct BaZhaiStarInfo {
    pub star: &'static str,
    pub zh: &'static str,
    pub desc_zh: &'static str,
    pub en: &'static str,
    pub good: bool,
    pub rank: u8,
}

pub fn bazhai_star_info(star: &str) -> BaZhaiStarInfo {
    match star {
        "shengqi" => BaZhaiStarInfo { star: "shengqi", zh: "生气", desc_zh: "大吉 · 旺丁旺财、生机勃发", en: "Sheng Qi — vitality, wealth", good: true, rank: 1 },
        "tianyi" => BaZhaiStarInfo { star: "tianyi", zh: "天医", desc_zh: "次吉 · 贵人护佑、康泰稳健", en: "Tian Yi — health", good: true, rank: 2 },
        "yannian" => BaZhaiStarInfo { star: "yannian", zh: "延年", desc_zh: "中吉 · 家庭和睦、长久顺遂", en: "Yan Nian — relationships, longevity", good: true, rank: 3 },
        "fuwei" => BaZhaiStarInfo { star: "fuwei", zh: "伏位", desc_zh: "小吉 · 平稳守成、宜静居书房", en: "Fu Wei — stability, calm", good: true, rank: 4 },
        "huohai" => BaZhaiStarInfo { star: "huohai", zh: "祸害", desc_zh: "小凶 · 易生琐碎口舌是非", en: "Huo Hai — mishaps", good: false, rank: 5 },
        "liusha" => BaZhaiStarInfo { star: "liusha", zh: "六煞", desc_zh: "次凶 · 易招是非波折与耗泄", en: "Liu Sha — scandal, setbacks", good: false, rank: 6 },
        "wugui" => BaZhaiStarInfo { star: "wugui", zh: "五鬼", desc_zh: "大凶 · 易犯火盗争讼，宜作厨厕", en: "Wu Gui — fire, theft, disputes", good: false, rank: 7 },
        _ => BaZhaiStarInfo { star: "jueming", zh: "绝命", desc_zh: "至凶 · 气场极弱，宜作杂物或卫浴", en: "Jue Ming — severe misfortune", good: false, rank: 8 },
    }
}

pub fn bazhai_table_for_gua(gua: u8) -> [(&'static str, &'static str); 8] {
    match gua {
        1 => [("shengqi", "SE"), ("tianyi", "E"), ("yannian", "S"), ("fuwei", "N"), ("huohai", "W"), ("wugui", "NE"), ("liusha", "NW"), ("jueming", "SW")],
        2 => [("shengqi", "NE"), ("tianyi", "W"), ("yannian", "NW"), ("fuwei", "SW"), ("huohai", "E"), ("wugui", "SE"), ("liusha", "S"), ("jueming", "N")],
        3 => [("shengqi", "S"), ("tianyi", "N"), ("yannian", "SE"), ("fuwei", "E"), ("huohai", "SW"), ("wugui", "NW"), ("liusha", "NE"), ("jueming", "W")],
        4 => [("shengqi", "N"), ("tianyi", "S"), ("yannian", "E"), ("fuwei", "SE"), ("huohai", "NW"), ("wugui", "SW"), ("liusha", "W"), ("jueming", "NE")],
        6 => [("shengqi", "W"), ("tianyi", "NE"), ("yannian", "SW"), ("fuwei", "NW"), ("huohai", "SE"), ("wugui", "E"), ("liusha", "N"), ("jueming", "S")],
        7 => [("shengqi", "NW"), ("tianyi", "SW"), ("yannian", "NE"), ("fuwei", "W"), ("huohai", "N"), ("wugui", "S"), ("liusha", "SE"), ("jueming", "E")],
        8 => [("shengqi", "SW"), ("tianyi", "NW"), ("yannian", "W"), ("fuwei", "NE"), ("huohai", "S"), ("wugui", "N"), ("liusha", "E"), ("jueming", "SE")],
        _ => [("shengqi", "E"), ("tianyi", "SE"), ("yannian", "N"), ("fuwei", "S"), ("huohai", "NE"), ("wugui", "W"), ("liusha", "SW"), ("jueming", "NW")],
    }
}

/// Ba Zhai (八宅 Eight Mansions) house reading from sitting palace (`N`, `NE`, `E`, `SE`, `S`, `SW`, `W`, `NW`).
pub fn ba_zhai(sitting_palace: &str, door_palace: Option<&str>) -> Value {
    let pal = palace_info(sitting_palace);
    let gua = if pal.num == 5 { 2 } else { pal.num };
    let pairs = bazhai_table_for_gua(gua);

    let mut table = serde_json::Map::new();
    let mut by_dir = serde_json::Map::new();
    for (star, dir) in pairs {
        table.insert(star.to_string(), Value::String(dir.to_string()));
        let info = bazhai_star_info(star);
        by_dir.insert(
            dir.to_string(),
            json!({
                "star": info.star,
                "zh": info.zh,
                "descZh": info.desc_zh,
                "en": info.en,
                "good": info.good,
                "rank": info.rank,
            }),
        );
    }

    let is_east = matches!(gua, 1 | 3 | 4 | 9);
    let door_val = door_palace.and_then(|dp| {
        by_dir.get(dp).map(|st| {
            let mut m = st.as_object().cloned().unwrap_or_default();
            m.insert("palace".to_string(), Value::String(dp.to_string()));
            Value::Object(m)
        })
    });

    json!({
        "gua": gua,
        "houseTrigram": pal.trigram,
        "houseName": format!("{}宅", pal.zh),
        "group": if is_east { "东四宅 East group" } else { "西四宅 West group" },
        "groupZh": if is_east { "东四宅" } else { "西四宅" },
        "byDir": by_dir.clone(),
        "eightSectors": by_dir,
        "table": table,
        "door": door_val,
        "personalKuaTable": personal_kua_table(),
    })
}

/// Personal Kua (命卦) number 1..=9 (5 mapped to 2 for male, 8 for female) from solar birth year & gender.
pub fn kua_number(year: i32, gender: &str) -> u8 {
    let yy = year.rem_euclid(100);
    let mut k = if gender.eq_ignore_ascii_case("male") || gender.eq_ignore_ascii_case("m") {
        if year >= 2000 {
            (99 - yy).rem_euclid(9)
        } else {
            (100 - yy).rem_euclid(9)
        }
    } else if year >= 2000 {
        (yy + 6).rem_euclid(9)
    } else {
        (yy - 4 + 180).rem_euclid(9)
    };
    if k == 0 {
        k = 9;
    }
    if k == 5 {
        k = if gender.eq_ignore_ascii_case("male") || gender.eq_ignore_ascii_case("m") {
            2
        } else {
            8
        };
    }
    k as u8
}

/// Reference Personal Kua table across recent birth years for quick lookup.
pub fn personal_kua_table() -> Value {
    let mut rows = Vec::new();
    for y in (1955..=2005).step_by(5) {
        let m_kua = kua_number(y, "male");
        let f_kua = kua_number(y, "female");
        rows.push(json!({
            "solarYear": y,
            "maleKua": m_kua,
            "maleGroup": if matches!(m_kua, 1 | 3 | 4 | 9) { "East" } else { "West" },
            "maleGroupZh": if matches!(m_kua, 1 | 3 | 4 | 9) { "东四命" } else { "西四命" },
            "femaleKua": f_kua,
            "femaleGroup": if matches!(f_kua, 1 | 3 | 4 | 9) { "East" } else { "West" },
            "femaleGroupZh": if matches!(f_kua, 1 | 3 | 4 | 9) { "东四命" } else { "西四命" },
        }));
    }
    Value::Array(rows)
}
