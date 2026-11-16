//! Static vocabulary of "Your Match" (plan v3, sections 5.2-5.5): the eleven dimensions, the sixteen
//! house-style families with the mapping from the 44 standard RESO `ArchitecturalStyle` values, the
//! deal-breaker screens, wished features, and every user-facing label in English, Simplified Chinese
//! and Spanish.
//!
//! Nothing here depends on the buyer or on the language except the label text (plan section 6,
//! rule 1): keys, weights and mappings are identical in every language.

use super::presets::PRESETS;
use super::profile::{
    DEFAULT_DOWN_PAYMENT_PCT, DEFAULT_TRAFFIC_BUFFER, DEFAULT_TRIPS_PER_WEEK, MAX_PLACES,
    MAX_TRAFFIC_BUFFER, MIN_TRAFFIC_BUFFER, PROFILE_VERSION,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Version of the [`catalog_json`] document.
pub const CATALOG_VERSION: u32 = 1;

// ---------------------------------------------------------------------------------------------
// Languages
// ---------------------------------------------------------------------------------------------

/// UI language. Rankings never depend on it; it only selects label text.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
pub enum Lang {
    #[default]
    #[serde(rename = "en")]
    En,
    #[serde(rename = "zh-CN")]
    ZhCn,
    #[serde(rename = "es")]
    Es,
}

impl Lang {
    pub const ALL: [Lang; 3] = [Lang::En, Lang::ZhCn, Lang::Es];

    /// Lenient parser for `?lang=` values and language tags (`zh`, `zh-CN`, `zh_Hans`, `es-MX`, ...).
    /// Anything unrecognised falls back to English.
    pub fn parse(tag: &str) -> Lang {
        let t = tag.trim().to_ascii_lowercase();
        if t.starts_with("zh") {
            Lang::ZhCn
        } else if t.starts_with("es") {
            Lang::Es
        } else {
            Lang::En
        }
    }

    /// BCP 47 code: `en`, `zh-CN` or `es`.
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::ZhCn => "zh-CN",
            Lang::Es => "es",
        }
    }
}

/// A user-facing string in the three supported languages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Text {
    pub en: &'static str,
    pub zh: &'static str,
    pub es: &'static str,
}

impl Text {
    pub const fn new(en: &'static str, zh: &'static str, es: &'static str) -> Self {
        Self { en, zh, es }
    }

    pub fn get(&self, lang: Lang) -> &'static str {
        match lang {
            Lang::En => self.en,
            Lang::ZhCn => self.zh,
            Lang::Es => self.es,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Dimensions
// ---------------------------------------------------------------------------------------------

/// The eleven "Your Match" dimensions (plan 5.2), in catalog order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dimension {
    Cost,
    Schools,
    Commute,
    Convenience,
    Homefit,
    Style,
    Hazards,
    Crime,
    Environment,
    Investment,
    Fengshui,
}

impl Dimension {
    pub const COUNT: usize = 11;
    pub const ALL: [Dimension; Dimension::COUNT] = [
        Dimension::Cost,
        Dimension::Schools,
        Dimension::Commute,
        Dimension::Convenience,
        Dimension::Homefit,
        Dimension::Style,
        Dimension::Hazards,
        Dimension::Crime,
        Dimension::Environment,
        Dimension::Investment,
        Dimension::Fengshui,
    ];

    /// Position in [`Dimension::ALL`]; also the index into per-dimension arrays.
    pub const fn index(self) -> usize {
        self as usize
    }

    pub fn key(self) -> &'static str {
        match self {
            Dimension::Cost => "cost",
            Dimension::Schools => "schools",
            Dimension::Commute => "commute",
            Dimension::Convenience => "convenience",
            Dimension::Homefit => "homefit",
            Dimension::Style => "style",
            Dimension::Hazards => "hazards",
            Dimension::Crime => "crime",
            Dimension::Environment => "environment",
            Dimension::Investment => "investment",
            Dimension::Fengshui => "fengshui",
        }
    }

    pub fn from_key(key: &str) -> Option<Dimension> {
        Dimension::ALL.into_iter().find(|d| d.key() == key)
    }

    pub fn label(self) -> Text {
        match self {
            Dimension::Cost => Text::new("Cost", "成本", "Costo"),
            Dimension::Schools => Text::new("Schools", "学校", "Escuelas"),
            Dimension::Commute => Text::new("Commute and my places", "通勤与常去地点", "Trayectos y mis lugares"),
            Dimension::Convenience => Text::new("Daily convenience", "生活便利", "Comodidad diaria"),
            Dimension::Homefit => Text::new("Home fit", "房屋契合度", "Ajuste de la vivienda"),
            Dimension::Style => Text::new("Style and character", "风格与特色", "Estilo y carácter"),
            Dimension::Hazards => Text::new("Natural hazards", "自然灾害", "Riesgos naturales"),
            Dimension::Crime => Text::new("Crime", "犯罪率", "Delincuencia"),
            Dimension::Environment => Text::new("Environment and quiet", "环境与安静", "Medio ambiente y tranquilidad"),
            Dimension::Investment => Text::new("Investment and resale", "投资与转售", "Inversión y reventa"),
            Dimension::Fengshui => Text::new("Feng shui and orientation", "风水与朝向", "Feng shui y orientación"),
        }
    }

    pub fn description(self) -> Text {
        match self {
            Dimension::Cost => Text::new(
                "All-in monthly cost compared with your budget.",
                "每月总持有成本与您预算的对比。",
                "Costo mensual total comparado con su presupuesto.",
            ),
            Dimension::Schools => Text::new(
                "Assigned public schools, scored from official state data.",
                "学区公立学校，依据州政府官方数据评分。",
                "Escuelas públicas asignadas, calificadas con datos oficiales del estado.",
            ),
            Dimension::Commute => Text::new(
                "Drive time to the places you go, with a traffic buffer.",
                "到您常去地点的驾车时间（含交通缓冲）。",
                "Tiempo en auto a los lugares que frecuenta, con margen por tráfico.",
            ),
            Dimension::Convenience => Text::new(
                "Nearby shops, parks and services, and how walkable the area is.",
                "附近商店、公园和服务设施，以及步行便利程度。",
                "Tiendas, parques y servicios cercanos, y qué tan caminable es la zona.",
            ),
            Dimension::Homefit => Text::new(
                "Beds, baths, size, lot, condition and layout compared with your needs.",
                "卧室、卫浴、面积、地块、房况和格局与您需求的匹配程度。",
                "Recámaras, baños, tamaño, terreno, estado y distribución frente a sus necesidades.",
            ),
            Dimension::Style => Text::new(
                "How closely the house matches the styles and features you like.",
                "房屋与您喜欢的风格和特色的契合程度。",
                "Qué tanto la casa coincide con los estilos y características que le gustan.",
            ),
            Dimension::Hazards => Text::new(
                "Flood, wildfire, earthquake and other natural-hazard exposure.",
                "洪水、山火、地震及其他自然灾害风险。",
                "Exposición a inundaciones, incendios forestales, sismos y otros riesgos naturales.",
            ),
            Dimension::Crime => Text::new(
                "Reported city-level violent and property crime rates compared with the national rate.",
                "城市层面报告的暴力与财产犯罪率，与全国水平对比。",
                "Tasas de delitos violentos y contra la propiedad reportadas en la ciudad, frente a la tasa nacional.",
            ),
            Dimension::Environment => Text::new(
                "Air, noise, drinking water and nearby contamination.",
                "空气、噪音、饮用水及附近污染情况。",
                "Aire, ruido, agua potable y contaminación cercana.",
            ),
            Dimension::Investment => Text::new(
                "Local appreciation, rental yield and affordability trends.",
                "当地房价增值、租金回报率与可负担性趋势。",
                "Plusvalía local, rendimiento de renta y tendencias de asequibilidad.",
            ),
            Dimension::Fengshui => Text::new(
                "Form-school and flying-star reading of the site (cultural guidance).",
                "基于峦头与玄空飞星的场地解读（文化参考）。",
                "Lectura del sitio según la escuela de la forma y las estrellas voladoras (orientación cultural).",
            ),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// House-style families and the RESO ArchitecturalStyle mapping
// ---------------------------------------------------------------------------------------------

/// The sixteen house-style families (plan 5.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StyleFamily {
    Modern,
    MidCentury,
    Ranch,
    SplitLevel,
    Craftsman,
    Farmhouse,
    Barn,
    Colonial,
    CapeCod,
    Victorian,
    Tudor,
    Mediterranean,
    Southwestern,
    Cabin,
    Traditional,
    OtherDistinctive,
}

impl StyleFamily {
    pub const COUNT: usize = 16;
    pub const ALL: [StyleFamily; StyleFamily::COUNT] = [
        StyleFamily::Modern,
        StyleFamily::MidCentury,
        StyleFamily::Ranch,
        StyleFamily::SplitLevel,
        StyleFamily::Craftsman,
        StyleFamily::Farmhouse,
        StyleFamily::Barn,
        StyleFamily::Colonial,
        StyleFamily::CapeCod,
        StyleFamily::Victorian,
        StyleFamily::Tudor,
        StyleFamily::Mediterranean,
        StyleFamily::Southwestern,
        StyleFamily::Cabin,
        StyleFamily::Traditional,
        StyleFamily::OtherDistinctive,
    ];

    pub fn key(self) -> &'static str {
        match self {
            StyleFamily::Modern => "modern",
            StyleFamily::MidCentury => "mid_century",
            StyleFamily::Ranch => "ranch",
            StyleFamily::SplitLevel => "split_level",
            StyleFamily::Craftsman => "craftsman",
            StyleFamily::Farmhouse => "farmhouse",
            StyleFamily::Barn => "barn",
            StyleFamily::Colonial => "colonial",
            StyleFamily::CapeCod => "cape_cod",
            StyleFamily::Victorian => "victorian",
            StyleFamily::Tudor => "tudor",
            StyleFamily::Mediterranean => "mediterranean",
            StyleFamily::Southwestern => "southwestern",
            StyleFamily::Cabin => "cabin",
            StyleFamily::Traditional => "traditional",
            StyleFamily::OtherDistinctive => "other_distinctive",
        }
    }

    pub fn from_key(key: &str) -> Option<StyleFamily> {
        StyleFamily::ALL.into_iter().find(|f| f.key() == key)
    }

    pub fn label(self) -> Text {
        match self {
            StyleFamily::Modern => Text::new("Modern / Contemporary", "现代 / 当代", "Moderno / Contemporáneo"),
            StyleFamily::MidCentury => Text::new("Mid-century modern", "中世纪现代", "Moderno de mediados de siglo"),
            StyleFamily::Ranch => Text::new("Ranch", "牧场式平房", "Rancho"),
            StyleFamily::SplitLevel => Text::new("Split-level", "错层式", "Niveles divididos"),
            StyleFamily::Craftsman => Text::new("Craftsman / Bungalow", "工匠风格 / 平房小屋", "Craftsman / Bungaló"),
            StyleFamily::Farmhouse => Text::new("Farmhouse", "农舍风格", "Casa de campo"),
            StyleFamily::Barn => Text::new("Barn-style", "谷仓风格", "Estilo granero"),
            StyleFamily::Colonial => Text::new("Colonial", "殖民地风格", "Estilo colonial"),
            StyleFamily::CapeCod => Text::new("Cape Cod", "科德角风格", "Estilo Cape Cod"),
            StyleFamily::Victorian => Text::new("Victorian", "维多利亚风格", "Victoriano"),
            StyleFamily::Tudor => Text::new("Tudor / European", "都铎 / 欧式", "Tudor / Europeo"),
            StyleFamily::Mediterranean => Text::new("Mediterranean / Spanish", "地中海 / 西班牙风格", "Mediterráneo / Español"),
            StyleFamily::Southwestern => Text::new("Southwestern", "美国西南风格", "Suroeste"),
            StyleFamily::Cabin => Text::new("Cabin / Rustic", "木屋 / 乡村风格", "Cabaña / Rústico"),
            StyleFamily::Traditional => Text::new("Traditional", "传统风格", "Tradicional"),
            StyleFamily::OtherDistinctive => Text::new("Other distinctive", "其他特色风格", "Otro estilo distintivo"),
        }
    }

    /// The standard RESO `ArchitecturalStyle` values that map to this family.
    pub fn reso_values(self) -> Vec<&'static str> {
        RESO_ARCHITECTURAL_STYLES
            .iter()
            .filter(|(_, fam)| *fam == Some(self))
            .map(|(v, _)| *v)
            .collect()
    }
}

/// The 44 standard values of the RESO Data Dictionary 2.0 `ArchitecturalStyle` lookup and the family
/// each maps to (plan 5.5). `None` means unknown: "Other" and "See Remarks".
pub const RESO_ARCHITECTURAL_STYLES: [(&str, Option<StyleFamily>); 44] = [
    ("A-Frame", Some(StyleFamily::Cabin)),
    ("Art Deco", Some(StyleFamily::OtherDistinctive)),
    ("Barndominium", Some(StyleFamily::Barn)),
    ("Berm", Some(StyleFamily::OtherDistinctive)),
    ("Bungalow", Some(StyleFamily::Craftsman)),
    ("Cabin", Some(StyleFamily::Cabin)),
    ("Cape Cod", Some(StyleFamily::CapeCod)),
    ("Colonial", Some(StyleFamily::Colonial)),
    ("Contemporary", Some(StyleFamily::Modern)),
    ("Craftsman", Some(StyleFamily::Craftsman)),
    ("Creole", Some(StyleFamily::Traditional)),
    ("Dome", Some(StyleFamily::OtherDistinctive)),
    ("Dutch Colonial", Some(StyleFamily::Colonial)),
    ("Farmhouse", Some(StyleFamily::Farmhouse)),
    ("Federal", Some(StyleFamily::Colonial)),
    ("French Provincial", Some(StyleFamily::Tudor)),
    ("Georgian", Some(StyleFamily::Colonial)),
    ("Gothic Revival", Some(StyleFamily::Victorian)),
    ("Greek Revival", Some(StyleFamily::Colonial)),
    ("Italianate", Some(StyleFamily::Victorian)),
    ("Mediterranean", Some(StyleFamily::Mediterranean)),
    ("Mid-Century Modern", Some(StyleFamily::MidCentury)),
    ("Modern", Some(StyleFamily::Modern)),
    ("Monterey", Some(StyleFamily::Mediterranean)),
    ("National", Some(StyleFamily::Traditional)),
    ("Neoclassical", Some(StyleFamily::Colonial)),
    ("Other", None),
    ("Prairie", Some(StyleFamily::Craftsman)),
    ("Pueblo", Some(StyleFamily::Southwestern)),
    ("Queen Anne", Some(StyleFamily::Victorian)),
    ("Raised Ranch", Some(StyleFamily::SplitLevel)),
    ("Ranch", Some(StyleFamily::Ranch)),
    ("Regency", Some(StyleFamily::Colonial)),
    ("Rustic", Some(StyleFamily::Cabin)),
    ("Saltbox", Some(StyleFamily::Colonial)),
    ("See Remarks", None),
    ("Shingle", Some(StyleFamily::Victorian)),
    ("Shotgun", Some(StyleFamily::Traditional)),
    ("Spanish", Some(StyleFamily::Mediterranean)),
    ("Split Level", Some(StyleFamily::SplitLevel)),
    ("Stick", Some(StyleFamily::Victorian)),
    ("Traditional", Some(StyleFamily::Traditional)),
    ("Tudor", Some(StyleFamily::Tudor)),
    ("Victorian", Some(StyleFamily::Victorian)),
];

/// Result of looking up a standard RESO `ArchitecturalStyle` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResoStyle {
    Family(StyleFamily),
    /// "Other" or "See Remarks".
    Unknown,
}

/// Lower-case alphanumerics only, so "Mid-Century Modern", "MidCenturyModern" and "mid century
/// modern" compare equal.
fn norm_key(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Looks up one of the 44 standard RESO values, in display ("Split Level") or enumeration
/// ("SplitLevel") form. `None` when the value is not a standard one.
pub fn map_reso_style(value: &str) -> Option<ResoStyle> {
    let k = norm_key(value);
    if k.is_empty() {
        return None;
    }
    RESO_ARCHITECTURAL_STYLES
        .iter()
        .find(|(v, _)| norm_key(v) == k)
        .map(|(_, fam)| match fam {
            Some(f) => ResoStyle::Family(*f),
            None => ResoStyle::Unknown,
        })
}

/// Non-standard values seen in local MLS feeds and county records (RentCast `architectureType`,
/// ATTOM), compared with [`norm_key`].
const STYLE_ALIASES: &[(&str, StyleFamily)] = &[
    ("midcentury", StyleFamily::MidCentury),
    ("eichler", StyleFamily::MidCentury),
    ("postandbeam", StyleFamily::MidCentury),
    ("rambler", StyleFamily::Ranch),
    ("ranchrambler", StyleFamily::Ranch),
    ("ranchstyle", StyleFamily::Ranch),
    ("californiaranch", StyleFamily::Ranch),
    ("bilevel", StyleFamily::SplitLevel),
    ("trilevel", StyleFamily::SplitLevel),
    ("splitentry", StyleFamily::SplitLevel),
    ("splitfoyer", StyleFamily::SplitLevel),
    ("craftsmanbungalow", StyleFamily::Craftsman),
    ("artsandcrafts", StyleFamily::Craftsman),
    ("foursquare", StyleFamily::Craftsman),
    ("americanfoursquare", StyleFamily::Craftsman),
    ("modernfarmhouse", StyleFamily::Farmhouse),
    ("gambrel", StyleFamily::Barn),
    ("barn", StyleFamily::Barn),
    ("barnstyle", StyleFamily::Barn),
    ("colonialrevival", StyleFamily::Colonial),
    ("georgiancolonial", StyleFamily::Colonial),
    ("folkvictorian", StyleFamily::Victorian),
    ("secondempire", StyleFamily::Victorian),
    ("tudorrevival", StyleFamily::Tudor),
    ("englishtudor", StyleFamily::Tudor),
    ("frenchcountry", StyleFamily::Tudor),
    ("frenchnormandy", StyleFamily::Tudor),
    ("european", StyleFamily::Tudor),
    ("chateau", StyleFamily::Tudor),
    ("storybook", StyleFamily::Tudor),
    ("spanishcolonial", StyleFamily::Mediterranean),
    ("spanishrevival", StyleFamily::Mediterranean),
    ("spanishmission", StyleFamily::Mediterranean),
    ("mission", StyleFamily::Mediterranean),
    ("missionrevival", StyleFamily::Mediterranean),
    ("tuscan", StyleFamily::Mediterranean),
    ("montereycolonial", StyleFamily::Mediterranean),
    ("santabarbara", StyleFamily::Mediterranean),
    ("pueblorevival", StyleFamily::Southwestern),
    ("adobe", StyleFamily::Southwestern),
    ("southwestern", StyleFamily::Southwestern),
    ("southwest", StyleFamily::Southwestern),
    ("territorial", StyleFamily::Southwestern),
    ("santafe", StyleFamily::Southwestern),
    ("log", StyleFamily::Cabin),
    ("logcabin", StyleFamily::Cabin),
    ("loghome", StyleFamily::Cabin),
    ("chalet", StyleFamily::Cabin),
    ("lodge", StyleFamily::Cabin),
    ("conventional", StyleFamily::Traditional),
    ("minimaltraditional", StyleFamily::Traditional),
    ("artmoderne", StyleFamily::OtherDistinctive),
    ("streamlinemoderne", StyleFamily::OtherDistinctive),
    ("geodesic", StyleFamily::OtherDistinctive),
    ("geodesicdome", StyleFamily::OtherDistinctive),
    ("earthberm", StyleFamily::OtherDistinctive),
    ("earthsheltered", StyleFamily::OtherDistinctive),
    ("ultramodern", StyleFamily::Modern),
    ("postmodern", StyleFamily::Modern),
    ("international", StyleFamily::Modern),
    ("minimalist", StyleFamily::Modern),
];

/// Families named by one structured style value (an MLS `ArchitecturalStyle` entry or a county-record
/// style). Standard RESO values map exactly; known local variants through an alias table; composite
/// values such as "Spanish/Mediterranean" or "Traditional, Ranch" part by part. Unknown values and
/// "Other" / "See Remarks" give an empty list.
pub fn style_families_for_value(value: &str) -> Vec<StyleFamily> {
    if let Some(r) = map_reso_style(value) {
        return match r {
            ResoStyle::Family(f) => vec![f],
            ResoStyle::Unknown => Vec::new(),
        };
    }
    let k = norm_key(value);
    if let Some((_, f)) = STYLE_ALIASES.iter().find(|(a, _)| *a == k) {
        return vec![*f];
    }
    let parts: Vec<&str> = value
        .split(['/', ',', ';', '&', '+', '|'])
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    let mut out = Vec::new();
    if parts.len() > 1 {
        for p in parts {
            for f in style_families_for_value(p) {
                if !out.contains(&f) {
                    out.push(f);
                }
            }
        }
    }
    out
}

/// How the buyer feels about a style family (plan 5.4: love = 1.0, fine = 0.6, avoid = 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StylePreference {
    Love,
    Fine,
    Avoid,
}

impl StylePreference {
    pub const ALL: [StylePreference; 3] =
        [StylePreference::Love, StylePreference::Fine, StylePreference::Avoid];

    /// Preference value used by the style utility.
    pub fn value(self) -> f64 {
        match self {
            StylePreference::Love => 1.0,
            StylePreference::Fine => 0.6,
            StylePreference::Avoid => 0.0,
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            StylePreference::Love => "love",
            StylePreference::Fine => "fine",
            StylePreference::Avoid => "avoid",
        }
    }

    pub fn label(self) -> Text {
        match self {
            StylePreference::Love => Text::new("Love it", "很喜欢", "Me encanta"),
            StylePreference::Fine => Text::new("Fine with it", "可以接受", "Me parece bien"),
            StylePreference::Avoid => Text::new("Avoid", "避免", "Evitar"),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Screens (must-haves and deal-breakers)
// ---------------------------------------------------------------------------------------------

/// Quick screens a buyer can use as must-haves or deal-breakers (plan 5.3, step 7). Each names a
/// condition the buyer does not want: as a deal-breaker a home with the condition is flagged and
/// hidden by default; as a must-have it moves to the near-miss list with the reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Screen {
    /// In a FEMA Special Flood Hazard Area.
    SfhaFlood,
    /// In a very high fire hazard severity zone (CAL FIRE), or USFS wildfire hazard potential "Very High".
    VeryHighFire,
    /// A freeway within 500 ft.
    #[serde(rename = "freeway_within_500ft")]
    FreewayWithin500ft,
    /// A street points straight at the house (路冲).
    TJunction,
    /// HOA dues above `Profile::hoa_max_monthly`.
    HoaOver,
    /// Most likely a style family the buyer marked "avoid".
    StyleAvoid,
    /// The house number contains a digit from `Profile::avoid_digits`.
    AddressDigits,
}

impl Screen {
    pub const ALL: [Screen; 7] = [
        Screen::SfhaFlood,
        Screen::VeryHighFire,
        Screen::FreewayWithin500ft,
        Screen::TJunction,
        Screen::HoaOver,
        Screen::StyleAvoid,
        Screen::AddressDigits,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Screen::SfhaFlood => "sfha_flood",
            Screen::VeryHighFire => "very_high_fire",
            Screen::FreewayWithin500ft => "freeway_within_500ft",
            Screen::TJunction => "t_junction",
            Screen::HoaOver => "hoa_over",
            Screen::StyleAvoid => "style_avoid",
            Screen::AddressDigits => "address_digits",
        }
    }

    pub fn label(self) -> Text {
        match self {
            Screen::SfhaFlood => Text::new(
                "High-risk flood zone (FEMA SFHA)",
                "高风险洪水区（FEMA SFHA）",
                "Zona de alto riesgo de inundación (FEMA SFHA)",
            ),
            Screen::VeryHighFire => Text::new(
                "Very high fire hazard zone",
                "极高火灾危险区",
                "Zona de riesgo de incendio muy alto",
            ),
            Screen::FreewayWithin500ft => Text::new(
                "Freeway within 500 ft",
                "500 英尺内有高速公路",
                "Autopista a menos de 500 pies",
            ),
            Screen::TJunction => Text::new(
                "T-junction (a street points at the house)",
                "路冲（道路直冲房屋）",
                "Cruce en T (una calle apunta a la casa)",
            ),
            Screen::HoaOver => Text::new(
                "HOA dues above my limit",
                "HOA 费用超过我的上限",
                "Cuota de HOA por encima de mi límite",
            ),
            Screen::StyleAvoid => Text::new(
                "A style I marked “avoid”",
                "我标记为“避免”的风格",
                "Un estilo que marqué como “evitar”",
            ),
            Screen::AddressDigits => Text::new(
                "House number with digits I avoid",
                "门牌号含有我想避开的数字",
                "Número de casa con dígitos que evito",
            ),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Wished features (plan 5.5, character features)
// ---------------------------------------------------------------------------------------------

/// Character features a buyer can wish for; matched against MLS fields and listing text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Feature {
    OpenPlan,
    HighCeilings,
    NaturalLight,
    Hardwood,
    Fireplace,
    Pool,
    View,
    MainLevelPrimary,
    Office,
    WorkshopGarage,
    EvReady,
}

impl Feature {
    pub const ALL: [Feature; 11] = [
        Feature::OpenPlan,
        Feature::HighCeilings,
        Feature::NaturalLight,
        Feature::Hardwood,
        Feature::Fireplace,
        Feature::Pool,
        Feature::View,
        Feature::MainLevelPrimary,
        Feature::Office,
        Feature::WorkshopGarage,
        Feature::EvReady,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Feature::OpenPlan => "open_plan",
            Feature::HighCeilings => "high_ceilings",
            Feature::NaturalLight => "natural_light",
            Feature::Hardwood => "hardwood",
            Feature::Fireplace => "fireplace",
            Feature::Pool => "pool",
            Feature::View => "view",
            Feature::MainLevelPrimary => "main_level_primary",
            Feature::Office => "office",
            Feature::WorkshopGarage => "workshop_garage",
            Feature::EvReady => "ev_ready",
        }
    }

    pub fn label(self) -> Text {
        match self {
            Feature::OpenPlan => Text::new("Open floor plan", "开放式格局", "Planta abierta"),
            Feature::HighCeilings => Text::new("High or vaulted ceilings", "挑高或拱形天花板", "Techos altos o abovedados"),
            Feature::NaturalLight => Text::new("Natural light", "采光充足", "Luz natural"),
            Feature::Hardwood => Text::new("Hardwood floors", "实木地板", "Pisos de madera"),
            Feature::Fireplace => Text::new("Fireplace", "壁炉", "Chimenea"),
            Feature::Pool => Text::new("Pool", "泳池", "Alberca"),
            Feature::View => Text::new("View", "景观视野", "Vista"),
            Feature::MainLevelPrimary => Text::new("Main-level primary suite", "一楼主卧套房", "Recámara principal en planta baja"),
            Feature::Office => Text::new("Home office", "家庭办公室", "Oficina en casa"),
            Feature::WorkshopGarage => Text::new("Workshop garage", "带工作间的车库", "Cochera con taller"),
            Feature::EvReady => Text::new("EV charging", "电动车充电", "Carga para vehículo eléctrico"),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Smaller vocabularies
// ---------------------------------------------------------------------------------------------

/// Loan type used to adjust the monthly cost (plan 5.3, step 1).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum LoanType {
    #[default]
    Conventional,
    Fha,
    Va,
    Cash,
}

impl LoanType {
    pub const ALL: [LoanType; 4] = [LoanType::Conventional, LoanType::Fha, LoanType::Va, LoanType::Cash];

    pub fn key(self) -> &'static str {
        match self {
            LoanType::Conventional => "conventional",
            LoanType::Fha => "fha",
            LoanType::Va => "va",
            LoanType::Cash => "cash",
        }
    }

    pub fn label(self) -> Text {
        match self {
            LoanType::Conventional => Text::new("Conventional", "常规贷款", "Convencional"),
            LoanType::Fha => Text::new("FHA", "FHA 贷款", "Préstamo FHA"),
            LoanType::Va => Text::new("VA", "VA 贷款", "Préstamo VA"),
            LoanType::Cash => Text::new("All cash", "全款", "De contado"),
        }
    }
}

/// School levels a buyer can weight (plan 5.3, step 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchoolLevel {
    Elementary,
    Middle,
    High,
}

impl SchoolLevel {
    pub const ALL: [SchoolLevel; 3] = [SchoolLevel::Elementary, SchoolLevel::Middle, SchoolLevel::High];

    pub fn key(self) -> &'static str {
        match self {
            SchoolLevel::Elementary => "elementary",
            SchoolLevel::Middle => "middle",
            SchoolLevel::High => "high",
        }
    }

    pub fn from_key(key: &str) -> Option<SchoolLevel> {
        SchoolLevel::ALL.into_iter().find(|l| l.key() == key)
    }

    pub fn label(self) -> Text {
        match self {
            SchoolLevel::Elementary => Text::new("Elementary", "小学", "Primaria"),
            SchoolLevel::Middle => Text::new("Middle", "初中", "Secundaria"),
            SchoolLevel::High => Text::new("High", "高中", "Preparatoria"),
        }
    }
}

/// Coarse property type, from free-text or RESO `PropertySubType` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PropertyKind {
    SingleFamily,
    Townhouse,
    Condo,
    MultiFamily,
    Manufactured,
    Other,
}

impl PropertyKind {
    pub const ALL: [PropertyKind; 6] = [
        PropertyKind::SingleFamily,
        PropertyKind::Townhouse,
        PropertyKind::Condo,
        PropertyKind::MultiFamily,
        PropertyKind::Manufactured,
        PropertyKind::Other,
    ];

    pub fn key(self) -> &'static str {
        match self {
            PropertyKind::SingleFamily => "single_family",
            PropertyKind::Townhouse => "townhouse",
            PropertyKind::Condo => "condo",
            PropertyKind::MultiFamily => "multi_family",
            PropertyKind::Manufactured => "manufactured",
            PropertyKind::Other => "other",
        }
    }

    pub fn label(self) -> Text {
        match self {
            PropertyKind::SingleFamily => Text::new("Single-family home", "独立屋", "Casa unifamiliar"),
            PropertyKind::Townhouse => Text::new("Townhouse", "联排别墅", "Casa adosada"),
            PropertyKind::Condo => Text::new("Condo", "公寓", "Condominio"),
            PropertyKind::MultiFamily => Text::new("Multi-family (2–4 units)", "多户住宅（2–4 户）", "Multifamiliar (2–4 unidades)"),
            PropertyKind::Manufactured => Text::new("Manufactured home", "预制房屋", "Casa prefabricada"),
            PropertyKind::Other => Text::new("Other", "其他", "Otro"),
        }
    }

    /// Classifies a property-type string such as "Single-family (detached) + Permitted ADU",
    /// "Condominium" or the RESO `SingleFamilyResidence`. `None` for an empty string.
    pub fn classify(raw: &str) -> Option<PropertyKind> {
        let t = raw.trim().to_lowercase();
        if t.is_empty() {
            return None;
        }
        let has = |needles: &[&str]| needles.iter().any(|n| t.contains(n));
        Some(if has(&["condo", "apartment", "co-op", "cooperative"]) {
            PropertyKind::Condo
        } else if has(&["townho", "town home", "rowho", "row house"]) {
            PropertyKind::Townhouse
        } else if has(&["duplex", "triplex", "fourplex", "quadruplex", "multi", "2-4 unit", "2–4 unit"]) {
            PropertyKind::MultiFamily
        } else if has(&["manufactured", "mobile"]) {
            PropertyKind::Manufactured
        } else if has(&["single", "sfr", "detached", "house", "residence"]) {
            PropertyKind::SingleFamily
        } else {
            PropertyKind::Other
        })
    }
}

/// Labelled band for a Your Match score. The cut-offs mirror the report's verdict text (75 / 62 / 50)
/// and are v0, to be calibrated (plan 5.12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchBand {
    Strong,
    Good,
    Fair,
    Weak,
}

impl MatchBand {
    pub const ALL: [MatchBand; 4] = [MatchBand::Strong, MatchBand::Good, MatchBand::Fair, MatchBand::Weak];
    pub const STRONG_MIN: f64 = 75.0;
    pub const GOOD_MIN: f64 = 62.0;
    pub const FAIR_MIN: f64 = 50.0;

    pub fn from_score(score: f64) -> MatchBand {
        if score >= Self::STRONG_MIN {
            MatchBand::Strong
        } else if score >= Self::GOOD_MIN {
            MatchBand::Good
        } else if score >= Self::FAIR_MIN {
            MatchBand::Fair
        } else {
            MatchBand::Weak
        }
    }

    /// Lowest score in the band.
    pub fn min_score(self) -> f64 {
        match self {
            MatchBand::Strong => Self::STRONG_MIN,
            MatchBand::Good => Self::GOOD_MIN,
            MatchBand::Fair => Self::FAIR_MIN,
            MatchBand::Weak => 0.0,
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            MatchBand::Strong => "strong",
            MatchBand::Good => "good",
            MatchBand::Fair => "fair",
            MatchBand::Weak => "weak",
        }
    }

    pub fn label(self) -> Text {
        match self {
            MatchBand::Strong => Text::new("Strong match", "高度匹配", "Coincidencia fuerte"),
            MatchBand::Good => Text::new("Good match", "较好匹配", "Buena coincidencia"),
            MatchBand::Fair => Text::new("Fair match", "一般匹配", "Coincidencia aceptable"),
            MatchBand::Weak => Text::new("Weak match", "匹配度低", "Coincidencia débil"),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Catalog document
// ---------------------------------------------------------------------------------------------

/// The catalog as one JSON document, for a future `GET /api/match/catalog?lang=` endpoint: dimensions,
/// style families with their RESO values, presets with weights, screens, features and the smaller
/// vocabularies, labelled in `lang`. Keys and numbers are identical in every language.
pub fn catalog_json(lang: Lang) -> Value {
    let dimensions: Vec<Value> = Dimension::ALL
        .iter()
        .map(|d| {
            json!({
                "key": d.key(),
                "label": d.label().get(lang),
                "description": d.description().get(lang),
            })
        })
        .collect();
    let style_families: Vec<Value> = StyleFamily::ALL
        .iter()
        .map(|f| {
            json!({
                "key": f.key(),
                "label": f.label().get(lang),
                "resoValues": f.reso_values(),
            })
        })
        .collect();
    let reso_unknown: Vec<&str> = RESO_ARCHITECTURAL_STYLES
        .iter()
        .filter(|(_, fam)| fam.is_none())
        .map(|(v, _)| *v)
        .collect();
    let presets: Vec<Value> = PRESETS
        .iter()
        .map(|p| {
            let weights: serde_json::Map<String, Value> = Dimension::ALL
                .iter()
                .map(|d| (d.key().to_string(), json!(p.weights[d.index()])))
                .collect();
            json!({
                "key": p.key,
                "label": p.label.get(lang),
                "description": p.description.get(lang),
                "weights": weights,
            })
        })
        .collect();
    let keyed = |key: &str, label: Text| json!({ "key": key, "label": label.get(lang) });
    json!({
        "version": CATALOG_VERSION,
        "profileVersion": PROFILE_VERSION,
        "lang": lang.code(),
        "dimensions": dimensions,
        "styleFamilies": style_families,
        "resoUnknownStyles": reso_unknown,
        "stylePreferences": StylePreference::ALL.iter().map(|p| json!({
            "key": p.key(), "label": p.label().get(lang), "value": p.value(),
        })).collect::<Vec<_>>(),
        "presets": presets,
        "screens": Screen::ALL.iter().map(|s| keyed(s.key(), s.label())).collect::<Vec<_>>(),
        "features": Feature::ALL.iter().map(|f| keyed(f.key(), f.label())).collect::<Vec<_>>(),
        "loanTypes": LoanType::ALL.iter().map(|l| keyed(l.key(), l.label())).collect::<Vec<_>>(),
        "schoolLevels": SchoolLevel::ALL.iter().map(|l| keyed(l.key(), l.label())).collect::<Vec<_>>(),
        "propertyTypes": PropertyKind::ALL.iter().map(|k| keyed(k.key(), k.label())).collect::<Vec<_>>(),
        "matchBands": MatchBand::ALL.iter().map(|b| json!({
            "key": b.key(), "label": b.label().get(lang), "min": b.min_score(),
        })).collect::<Vec<_>>(),
        "defaults": {
            "trafficBuffer": DEFAULT_TRAFFIC_BUFFER,
            "trafficBufferRange": [MIN_TRAFFIC_BUFFER, MAX_TRAFFIC_BUFFER],
            "downPaymentPct": DEFAULT_DOWN_PAYMENT_PCT,
            "tripsPerWeek": DEFAULT_TRIPS_PER_WEEK,
            "maxPlaces": MAX_PLACES,
            "travelModes": ["drive"],
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_match_serde_names() {
        for d in Dimension::ALL {
            assert_eq!(serde_json::to_value(d).unwrap(), json!(d.key()));
            assert_eq!(Dimension::from_key(d.key()), Some(d));
        }
        for f in StyleFamily::ALL {
            assert_eq!(serde_json::to_value(f).unwrap(), json!(f.key()));
        }
        for s in Screen::ALL {
            assert_eq!(serde_json::to_value(s).unwrap(), json!(s.key()));
        }
        for f in Feature::ALL {
            assert_eq!(serde_json::to_value(f).unwrap(), json!(f.key()));
        }
        for l in LoanType::ALL {
            assert_eq!(serde_json::to_value(l).unwrap(), json!(l.key()));
        }
        for k in PropertyKind::ALL {
            assert_eq!(serde_json::to_value(k).unwrap(), json!(k.key()));
        }
        for b in MatchBand::ALL {
            assert_eq!(serde_json::to_value(b).unwrap(), json!(b.key()));
        }
        for p in StylePreference::ALL {
            assert_eq!(serde_json::to_value(p).unwrap(), json!(p.key()));
        }
        for l in Lang::ALL {
            assert_eq!(serde_json::to_value(l).unwrap(), json!(l.code()));
        }
    }

    #[test]
    fn every_reso_value_maps_to_exactly_one_family_or_unknown() {
        let mut families_used = std::collections::BTreeSet::new();
        for (value, fam) in RESO_ARCHITECTURAL_STYLES {
            let got = map_reso_style(value).expect("standard value must be found");
            match fam {
                Some(f) => {
                    assert_eq!(got, ResoStyle::Family(f), "{value}");
                    families_used.insert(f);
                }
                None => assert_eq!(got, ResoStyle::Unknown, "{value}"),
            }
            // The enumeration form ("SplitLevel") maps the same way.
            let compact: String = value.chars().filter(|c| c.is_alphanumeric()).collect();
            assert_eq!(map_reso_style(&compact), Some(got));
        }
        assert_eq!(families_used.len(), StyleFamily::COUNT, "every family has a RESO value");
        let unknown: Vec<_> = RESO_ARCHITECTURAL_STYLES.iter().filter(|(_, f)| f.is_none()).map(|(v, _)| *v).collect();
        assert_eq!(unknown, vec!["Other", "See Remarks"]);
        assert_eq!(map_reso_style("Spanish/Mediterranean"), None);
        assert_eq!(
            style_families_for_value("Spanish/Mediterranean"),
            vec![StyleFamily::Mediterranean]
        );
        assert_eq!(
            style_families_for_value("Traditional, Ranch"),
            vec![StyleFamily::Traditional, StyleFamily::Ranch]
        );
        assert_eq!(style_families_for_value("See Remarks"), Vec::<StyleFamily>::new());
        assert_eq!(style_families_for_value("Eichler"), vec![StyleFamily::MidCentury]);
    }

    #[test]
    fn catalog_is_complete_in_every_language() {
        let en = catalog_json(Lang::En);
        let en_preset_weights: Vec<(&str, &Value)> = en["presets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (p["key"].as_str().unwrap(), &p["weights"]))
            .collect();
        for lang in Lang::ALL {
            let c = catalog_json(lang);
            assert_eq!(c["dimensions"].as_array().unwrap().len(), 11);
            assert_eq!(c["styleFamilies"].as_array().unwrap().len(), 16);
            assert_eq!(c["presets"].as_array().unwrap().len(), 10);
            for list in ["dimensions", "styleFamilies", "presets", "screens", "features"] {
                for item in c[list].as_array().unwrap() {
                    assert!(!item["label"].as_str().unwrap().is_empty(), "{list} {lang:?}");
                }
            }
            let preset_weights: Vec<(&str, &Value)> = c["presets"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| (p["key"].as_str().unwrap(), &p["weights"]))
                .collect();
            assert_eq!(preset_weights, en_preset_weights, "presets must match across languages ({lang:?})");
            assert_eq!(c["defaults"], en["defaults"], "defaults must match across languages");
            assert_eq!(c["resoUnknownStyles"], en["resoUnknownStyles"]);
        }
        assert_eq!(Lang::parse("zh-Hans"), Lang::ZhCn);
        assert_eq!(Lang::parse("es-MX"), Lang::Es);
        assert_eq!(Lang::parse("fr"), Lang::En);
    }

    #[test]
    fn property_kinds_classify() {
        assert_eq!(PropertyKind::classify("Single-family (detached) + Permitted ADU"), Some(PropertyKind::SingleFamily));
        assert_eq!(PropertyKind::classify("SingleFamilyResidence"), Some(PropertyKind::SingleFamily));
        assert_eq!(PropertyKind::classify("Condominium"), Some(PropertyKind::Condo));
        assert_eq!(PropertyKind::classify("Townhouse"), Some(PropertyKind::Townhouse));
        assert_eq!(PropertyKind::classify("Duplex"), Some(PropertyKind::MultiFamily));
        assert_eq!(PropertyKind::classify(""), None);
    }
}
