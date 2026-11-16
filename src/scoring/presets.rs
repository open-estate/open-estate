//! The ten lifestyle presets (plan v3, section 5.3): starting weights offered to everyone, identical in
//! every language. Weights are percentages over the eleven dimensions in [`Dimension::ALL`] order and
//! each preset sums to 100. They are v0 proposals, to be tuned against real searches.

use super::catalog::{Dimension, Text};
use super::profile::Profile;
use std::collections::BTreeMap;

/// A named set of starting weights.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    pub key: &'static str,
    pub label: Text,
    pub description: Text,
    /// cost, schools, commute, convenience, homefit, style, hazards, crime, environment, investment, fengshui
    pub weights: [u8; Dimension::COUNT],
}

impl Preset {
    pub fn weight(&self, d: Dimension) -> u8 {
        self.weights[d.index()]
    }

    pub fn weights_map(&self) -> BTreeMap<Dimension, f64> {
        Dimension::ALL
            .iter()
            .map(|d| (*d, f64::from(self.weights[d.index()])))
            .collect()
    }

    /// A profile with this preset's weights and every other field at its default.
    pub fn to_profile(&self) -> Profile {
        Profile {
            weights: self.weights_map(),
            preset: Some(self.key.to_string()),
            ..Profile::default()
        }
    }
}

/// The preset table from the plan, in its order.
pub static PRESETS: [Preset; 10] = [
    Preset {
        key: "balanced",
        label: Text::new("Balanced", "均衡", "Equilibrado"),
        description: Text::new(
            "A bit of everything, with cost and commute first.",
            "各方面兼顾，成本与通勤略优先。",
            "Un poco de todo, con prioridad al costo y al trayecto.",
        ),
        weights: [16, 9, 13, 10, 12, 6, 9, 8, 7, 8, 2],
    },
    Preset {
        key: "education_first",
        label: Text::new("Education-first", "教育优先", "Educación primero"),
        description: Text::new(
            "Assigned public schools carry the most weight.",
            "学区公立学校权重最高。",
            "Las escuelas públicas asignadas pesan más.",
        ),
        weights: [12, 28, 9, 6, 12, 4, 7, 9, 5, 6, 2],
    },
    Preset {
        key: "commute_first",
        label: Text::new("Commute-first", "通勤优先", "Trayecto primero"),
        description: Text::new(
            "Short drives to the places you go matter most.",
            "到常去地点的车程最重要。",
            "Lo más importante son los trayectos cortos a sus lugares.",
        ),
        weights: [14, 4, 28, 14, 9, 5, 6, 6, 5, 7, 2],
    },
    Preset {
        key: "multigenerational",
        label: Text::new("Multigenerational household", "多代同堂", "Hogar multigeneracional"),
        description: Text::new(
            "Room for extended family: space, layout and in-law units.",
            "为大家庭留足空间：面积、格局和附属住宅单元。",
            "Espacio para la familia extendida: tamaño, distribución y unidades adicionales.",
        ),
        weights: [16, 9, 9, 10, 24, 4, 7, 8, 5, 6, 2],
    },
    Preset {
        key: "first_home",
        label: Text::new(
            "First home, budget-focused",
            "首次购房，预算优先",
            "Primera vivienda, enfocada en el presupuesto",
        ),
        description: Text::new(
            "Keeping the monthly cost comfortable comes first.",
            "首先确保每月开支宽裕。",
            "Lo primero es un costo mensual cómodo.",
        ),
        weights: [32, 5, 13, 9, 13, 3, 6, 7, 3, 7, 2],
    },
    Preset {
        key: "investor",
        label: Text::new("Investor", "投资者", "Inversionista"),
        description: Text::new(
            "Appreciation, rental yield and resale come first.",
            "增值、租金回报和转售优先。",
            "Primero la plusvalía, el rendimiento de renta y la reventa.",
        ),
        weights: [15, 7, 5, 7, 7, 3, 10, 7, 5, 32, 2],
    },
    Preset {
        key: "feng_shui",
        label: Text::new("Feng-shui-conscious", "注重风水", "Atento al feng shui"),
        description: Text::new(
            "Feng shui and orientation count alongside the basics.",
            "在基本条件之外，同样看重风水与朝向。",
            "El feng shui y la orientación cuentan junto con lo básico.",
        ),
        weights: [12, 12, 7, 7, 12, 6, 8, 7, 7, 7, 15],
    },
    Preset {
        key: "quiet_healthy",
        label: Text::new("Quiet and healthy", "安静健康", "Tranquilo y saludable"),
        description: Text::new(
            "Clean air, low noise and low natural-hazard exposure come first.",
            "空气清新、噪音低、自然灾害风险低优先。",
            "Primero aire limpio, poco ruido y baja exposición a riesgos naturales.",
        ),
        weights: [10, 5, 7, 8, 10, 4, 18, 10, 22, 4, 2],
    },
    Preset {
        key: "walkable",
        label: Text::new("Walkable lifestyle", "步行生活", "Estilo de vida caminable"),
        description: Text::new(
            "Shops, parks and services within walking distance.",
            "商店、公园和服务设施步行可达。",
            "Tiendas, parques y servicios a poca distancia a pie.",
        ),
        weights: [12, 4, 14, 28, 7, 6, 6, 8, 6, 7, 2],
    },
    Preset {
        key: "design_lover",
        label: Text::new("Design lover", "设计爱好者", "Amante del diseño"),
        description: Text::new(
            "The house's style and character matter a lot.",
            "非常看重房屋的风格与特色。",
            "El estilo y el carácter de la casa importan mucho.",
        ),
        weights: [14, 5, 10, 10, 14, 24, 6, 6, 5, 4, 2],
    },
];

/// Looks up a preset by key.
pub fn preset(key: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.key == key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_sum_to_100_and_keys_are_unique() {
        let mut keys = std::collections::BTreeSet::new();
        for p in PRESETS.iter() {
            let sum: u32 = p.weights.iter().map(|w| u32::from(*w)).sum();
            assert_eq!(sum, 100, "{}", p.key);
            assert!(keys.insert(p.key), "duplicate preset key {}", p.key);
            assert!(p.to_profile().validate().is_ok(), "{}", p.key);
        }
        assert_eq!(preset("investor").unwrap().weight(Dimension::Investment), 32);
    }
}
