//! Computed explanations (plan v3, section 5.4, rule 6). The top three reasons and top two trade-offs
//! of a home are the largest weighted differences between its utilities and the median of the search,
//! in score points; estimated values never explain anything. Text is rendered from fixed templates in
//! English, Simplified Chinese and Spanish; the LLM writes none of it.

use super::catalog::{Dimension, Lang, PropertyKind, StyleFamily, Text};
use super::engine::{
    DimensionScore, DropReason, DroppedDimension, EstimatedDimension, GateHit, GateReason, Notice,
    NoticeCode, RankResult, ScoredHome,
};
use super::utility::EstimateReason;
use serde::{Deserialize, Serialize};

pub const MAX_REASONS: usize = 3;
pub const MAX_TRADE_OFFS: usize = 2;
/// Differences smaller than this many score points are not worth explaining.
pub const MIN_IMPACT: f64 = 0.1;

/// One reason or trade-off.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Explanation {
    pub dimension: Dimension,
    /// This home's utility, 0–100.
    pub utility: f64,
    /// Median utility of the search, 0–100.
    pub median: f64,
    /// Weighted difference from the median, in score points (positive for a reason).
    pub impact: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// Reasons (largest positive impacts first) and trade-offs (most negative first) for one home.
pub fn explain_dimensions(
    dims: &[DimensionScore],
    medians: &[Option<f64>; Dimension::COUNT],
) -> (Vec<Explanation>, Vec<Explanation>) {
    let items: Vec<Explanation> = dims
        .iter()
        .filter(|d| !d.estimated)
        .filter_map(|d| {
            let median = medians[d.dimension.index()]?;
            Some(Explanation {
                dimension: d.dimension,
                utility: d.utility,
                median,
                impact: d.share * (d.utility - median),
                text: None,
            })
        })
        .collect();
    let mut reasons: Vec<Explanation> = items.iter().filter(|e| e.impact >= MIN_IMPACT).cloned().collect();
    reasons.sort_by(|a, b| b.impact.total_cmp(&a.impact).then(a.dimension.cmp(&b.dimension)));
    reasons.truncate(MAX_REASONS);
    let mut trade_offs: Vec<Explanation> = items.into_iter().filter(|e| e.impact <= -MIN_IMPACT).collect();
    trade_offs.sort_by(|a, b| a.impact.total_cmp(&b.impact).then(a.dimension.cmp(&b.dimension)));
    trade_offs.truncate(MAX_TRADE_OFFS);
    (reasons, trade_offs)
}

fn better(d: Dimension) -> Text {
    match d {
        Dimension::Cost => Text::new("More comfortable on your budget", "更符合您的预算", "Más holgado para su presupuesto"),
        Dimension::Schools => Text::new("Stronger assigned public schools", "学区公立学校更强", "Escuelas públicas asignadas más fuertes"),
        Dimension::Commute => Text::new("Shorter drives to your places", "到常去地点的车程更短", "Trayectos más cortos a sus lugares"),
        Dimension::Convenience => Text::new("More shops, parks and services nearby", "附近商店、公园和服务更多", "Más tiendas, parques y servicios cerca"),
        Dimension::Homefit => Text::new("Closer fit to your space needs", "更符合您的空间需求", "Se ajusta mejor a sus necesidades de espacio"),
        Dimension::Style => Text::new("Closer to the styles and features you like", "更接近您喜欢的风格和特色", "Más cerca de los estilos y características que le gustan"),
        Dimension::Hazards => Text::new("Lower natural-hazard exposure", "自然灾害风险更低", "Menor exposición a riesgos naturales"),
        Dimension::Crime => Text::new("Lower reported city crime rates", "所在城市报告的犯罪率较低", "Tasas de delitos reportadas más bajas en la ciudad"),
        Dimension::Environment => Text::new("Cleaner, quieter surroundings", "周边环境更清洁、更安静", "Entorno más limpio y tranquilo"),
        Dimension::Investment => Text::new("Stronger investment outlook", "投资前景更好", "Mejores perspectivas de inversión"),
        Dimension::Fengshui => Text::new("Stronger feng shui reading", "风水解读更佳", "Mejor lectura de feng shui"),
    }
}

fn worse(d: Dimension) -> Text {
    match d {
        Dimension::Cost => Text::new("Tighter on your budget", "预算压力更大", "Más ajustado para su presupuesto"),
        Dimension::Schools => Text::new("Weaker assigned public schools", "学区公立学校较弱", "Escuelas públicas asignadas más débiles"),
        Dimension::Commute => Text::new("Longer drives to your places", "到常去地点的车程更长", "Trayectos más largos a sus lugares"),
        Dimension::Convenience => Text::new("Fewer shops and services within reach", "附近商店和服务较少", "Menos tiendas y servicios al alcance"),
        Dimension::Homefit => Text::new("Weaker fit to your space needs", "与您的空间需求差距较大", "Se ajusta menos a sus necesidades de espacio"),
        Dimension::Style => Text::new("Further from the styles and features you like", "与您喜欢的风格和特色差距较大", "Más lejos de los estilos y características que le gustan"),
        Dimension::Hazards => Text::new("Higher natural-hazard exposure", "自然灾害风险更高", "Mayor exposición a riesgos naturales"),
        Dimension::Crime => Text::new("Higher reported city crime rates", "所在城市报告的犯罪率较高", "Tasas de delitos reportadas más altas en la ciudad"),
        Dimension::Environment => Text::new("More pollution or noise nearby", "附近污染或噪音更多", "Más contaminación o ruido cerca"),
        Dimension::Investment => Text::new("Weaker investment outlook", "投资前景较弱", "Perspectivas de inversión más débiles"),
        Dimension::Fengshui => Text::new("Weaker feng shui reading", "风水解读较弱", "Lectura de feng shui más débil"),
    }
}

/// "Shorter drives to your places: 93 vs 84 typical for this search (+1.2 pts)".
pub fn render_explanation(e: &Explanation, lang: Lang) -> String {
    let phrase = (if e.impact >= 0.0 { better(e.dimension) } else { worse(e.dimension) }).get(lang);
    let (u, m, i) = (e.utility.round(), e.median.round(), e.impact);
    match lang {
        Lang::En => format!("{phrase}: {u:.0} vs {m:.0} typical for this search ({i:+.1} pts)"),
        Lang::ZhCn => format!("{phrase}：{u:.0} 分，本次搜索中位数 {m:.0} 分（{i:+.1} 分）"),
        Lang::Es => format!("{phrase}: {u:.0} frente a {m:.0} típico en esta búsqueda ({i:+.1} pts)"),
    }
}

fn money(v: f64) -> String {
    let n = v.round() as i64;
    let s = n.abs().to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    if n < 0 {
        format!("-${out}")
    } else {
        format!("${out}")
    }
}

/// Text for a failed must-have or deal-breaker.
pub fn render_gate(g: &GateHit, lang: Lang) -> String {
    let label = g.label.as_deref().unwrap_or("");
    match g.reason {
        GateReason::SfhaFlood => match (lang, label.is_empty()) {
            (Lang::En, false) => format!("In a FEMA high-risk flood zone ({label})"),
            (Lang::ZhCn, false) => format!("位于 FEMA 高风险洪水区（{label}）"),
            (Lang::Es, false) => format!("En una zona de alto riesgo de inundación de FEMA ({label})"),
            (_, true) => Text::new(
                "In a FEMA high-risk flood zone",
                "位于 FEMA 高风险洪水区",
                "En una zona de alto riesgo de inundación de FEMA",
            )
            .get(lang)
            .to_string(),
        },
        GateReason::VeryHighFire => Text::new(
            "In a very high fire hazard severity zone",
            "位于极高火灾危险区",
            "En una zona de riesgo de incendio muy alto",
        )
        .get(lang)
        .to_string(),
        GateReason::FreewayWithin500ft => match g.value {
            Some(ft) => match lang {
                Lang::En => format!("Freeway about {ft:.0} ft away (limit 500 ft)"),
                Lang::ZhCn => format!("高速公路约 {ft:.0} 英尺（上限 500 英尺）"),
                Lang::Es => format!("Autopista a unos {ft:.0} pies (límite 500 pies)"),
            },
            None => Text::new("Freeway within 500 ft", "500 英尺内有高速公路", "Autopista a menos de 500 pies")
                .get(lang)
                .to_string(),
        },
        GateReason::TJunction => Text::new(
            "A street points straight at the house (T-junction)",
            "有道路直冲房屋（路冲）",
            "Una calle apunta directamente a la casa (cruce en T)",
        )
        .get(lang)
        .to_string(),
        GateReason::HoaOver => {
            let (v, l) = (money(g.value.unwrap_or(0.0)), money(g.limit.unwrap_or(0.0)));
            match lang {
                Lang::En => format!("HOA {v}/month is above your {l} limit"),
                Lang::ZhCn => format!("HOA 每月 {v}，超过您的上限 {l}"),
                Lang::Es => format!("La cuota de HOA de {v} al mes supera su límite de {l}"),
            }
        }
        GateReason::StyleAvoid => {
            let style = StyleFamily::from_key(label).map(|f| f.label().get(lang)).unwrap_or(label);
            match lang {
                Lang::En => format!("Most likely a style you avoid: {style}"),
                Lang::ZhCn => format!("很可能是您想避免的风格：{style}"),
                Lang::Es => format!("Muy probablemente un estilo que evita: {style}"),
            }
        }
        GateReason::AddressDigits => match lang {
            Lang::En => format!("House number contains {label}"),
            Lang::ZhCn => format!("门牌号含有数字 {label}"),
            Lang::Es => format!("El número de la casa contiene {label}"),
        },
        GateReason::PropertyType => {
            let kind = PropertyKind::ALL
                .into_iter()
                .find(|k| k.key() == label)
                .map(|k| k.label().get(lang))
                .unwrap_or(label);
            match lang {
                Lang::En => format!("Property type is {kind}, not one you selected"),
                Lang::ZhCn => format!("房屋类型为{kind}，不在您选择的类型中"),
                Lang::Es => format!("El tipo de propiedad es {kind}, no uno de los que eligió"),
            }
        }
    }
}

/// Text for a dimension left out of every score.
pub fn render_drop(d: &DroppedDimension, lang: Lang) -> String {
    let label = d.dimension.label().get(lang);
    match d.reason {
        DropReason::NoData => match lang {
            Lang::En => format!("{label}: no home in this search has data for it, so it was left out."),
            Lang::ZhCn => format!("{label}：本次搜索中没有房屋有这项数据，因此未计入。"),
            Lang::Es => format!("{label}: ninguna casa de esta búsqueda tiene datos, así que se omitió."),
        },
        DropReason::NoStylePreference => Text::new(
            "Style: mark at least one style you love or avoid, or wish for a feature, to use this slider.",
            "风格：请至少标记一种喜欢或想避免的风格，或勾选想要的特色，此滑块才会生效。",
            "Estilo: marque al menos un estilo que le encante o quiera evitar, o elija una característica, para usar este control.",
        )
        .get(lang)
        .to_string(),
        DropReason::NoPlaces => Text::new(
            "Commute: add at least one place you go to use this slider.",
            "通勤：请至少添加一个常去地点，此滑块才会生效。",
            "Trayectos: agregue al menos un lugar que frecuenta para usar este control.",
        )
        .get(lang)
        .to_string(),
        DropReason::NoBudget => Text::new(
            "Cost: enter a monthly budget or a maximum price to use this slider.",
            "成本：请输入每月预算或最高价格，此滑块才会生效。",
            "Costo: ingrese un presupuesto mensual o un precio máximo para usar este control.",
        )
        .get(lang)
        .to_string(),
    }
}

/// Text for an estimated dimension of one home.
pub fn render_estimate(e: &EstimatedDimension, lang: Lang) -> String {
    let label = e.dimension.label().get(lang);
    match e.reason {
        EstimateReason::ImputedMedian => match lang {
            Lang::En => format!("{label}: no data for this home, so the search median is used (estimated)."),
            Lang::ZhCn => format!("{label}：此房屋缺少数据，采用本次搜索的中位数（估计值）。"),
            Lang::Es => format!("{label}: no hay datos para esta casa; se usa la mediana de la búsqueda (estimado)."),
        },
        EstimateReason::NoStyleEvidence => Text::new(
            "Style: no style evidence for this home, so its unknown style counts as “fine” (estimated).",
            "风格：此房屋没有风格依据，未知风格按“可以接受”计算（估计值）。",
            "Estilo: no hay evidencia del estilo de esta casa; el estilo desconocido cuenta como “me parece bien” (estimado).",
        )
        .get(lang)
        .to_string(),
        EstimateReason::RoadDistanceEstimate => Text::new(
            "Commute: estimated from road distance, not a routed drive time.",
            "通勤：根据道路距离估算，并非路线规划得出的驾车时间。",
            "Trayectos: estimado por distancia, no es un tiempo de manejo calculado por ruta.",
        )
        .get(lang)
        .to_string(),
        EstimateReason::SourceEstimate => match lang {
            Lang::En => format!("{label}: the source value is itself an estimate."),
            Lang::ZhCn => format!("{label}：来源数据本身就是估计值。"),
            Lang::Es => format!("{label}: el dato de origen ya es una estimación."),
        },
    }
}

/// Text for a search-level notice.
pub fn render_notice(n: &Notice, lang: Lang) -> String {
    match n.code {
        NoticeCode::NoScorableDimensions => Text::new(
            "None of the weighted dimensions has data for these homes, so no score could be computed.",
            "加权的各项维度在这些房屋上都没有数据，因此无法计算得分。",
            "Ninguna de las dimensiones con peso tiene datos para estas casas, así que no se pudo calcular un puntaje.",
        )
        .get(lang)
        .to_string(),
        NoticeCode::PlaceWithoutDriveTimes => {
            let place = n.place_label.as_deref().unwrap_or("?");
            match lang {
                Lang::En => format!("No drive times to “{place}” are available yet, so it was left out of the commute score."),
                Lang::ZhCn => format!("暂时没有到“{place}”的驾车时间，通勤得分未计入该地点。"),
                Lang::Es => format!("Aún no hay tiempos de manejo a “{place}”, así que se omitió del puntaje de trayectos."),
            }
        }
    }
}

/// Fills the `text` fields of one home in `lang`.
pub fn localize_home(h: &mut ScoredHome, lang: Lang) {
    for e in h.reasons.iter_mut().chain(h.trade_offs.iter_mut()) {
        e.text = Some(render_explanation(e, lang));
    }
    for e in &mut h.estimated {
        e.text = Some(render_estimate(e, lang));
    }
    for g in &mut h.gates {
        g.text = Some(render_gate(g, lang));
    }
}

/// Fills every `text` field of a ranking in `lang`. Scores and order are untouched: the ranking is
/// identical in every language.
pub fn localize(result: &mut RankResult, lang: Lang) {
    for d in &mut result.dropped {
        d.text = Some(render_drop(d, lang));
    }
    for n in &mut result.notices {
        n.text = Some(render_notice(n, lang));
    }
    for h in result
        .ranked
        .iter_mut()
        .chain(result.near_misses.iter_mut())
        .chain(result.flagged.iter_mut())
    {
        localize_home(h, lang);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explanation_text_in_three_languages() {
        let e = Explanation { dimension: Dimension::Commute, utility: 92.96, median: 83.7, impact: 1.2, text: None };
        assert_eq!(render_explanation(&e, Lang::En), "Shorter drives to your places: 93 vs 84 typical for this search (+1.2 pts)");
        assert!(render_explanation(&e, Lang::ZhCn).starts_with("到常去地点的车程更短"));
        assert!(render_explanation(&e, Lang::Es).starts_with("Trayectos más cortos"));
        assert_eq!(money(16496.0), "$16,496");
        assert_eq!(money(450.0), "$450");
    }
}
