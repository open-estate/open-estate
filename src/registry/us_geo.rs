//! Canonical 50 US states + DC + 5 inhabited territories (`US_JURISDICTIONS`) with 2-letter postal
//! abbreviations, 2-digit ANSI/FIPS codes, full names, and official US Census divisions/regions.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsJurisdiction {
    pub abbr: &'static str,
    pub fips2: &'static str,
    pub name: &'static str,
    pub name_lower: &'static str,
    pub census_division: &'static str,
    pub census_region: &'static str,
    pub is_territory: bool,
}

pub const US_JURISDICTIONS: &[UsJurisdiction] = &[
    UsJurisdiction { abbr: "AL", fips2: "01", name: "Alabama", name_lower: "alabama", census_division: "East South Central", census_region: "South", is_territory: false },
    UsJurisdiction { abbr: "AK", fips2: "02", name: "Alaska", name_lower: "alaska", census_division: "Pacific", census_region: "West", is_territory: false },
    UsJurisdiction { abbr: "AZ", fips2: "04", name: "Arizona", name_lower: "arizona", census_division: "Mountain", census_region: "West", is_territory: false },
    UsJurisdiction { abbr: "AR", fips2: "05", name: "Arkansas", name_lower: "arkansas", census_division: "West South Central", census_region: "South", is_territory: false },
    UsJurisdiction { abbr: "CA", fips2: "06", name: "California", name_lower: "california", census_division: "Pacific", census_region: "West", is_territory: false },
    UsJurisdiction { abbr: "CO", fips2: "08", name: "Colorado", name_lower: "colorado", census_division: "Mountain", census_region: "West", is_territory: false },
    UsJurisdiction { abbr: "CT", fips2: "09", name: "Connecticut", name_lower: "connecticut", census_division: "New England", census_region: "Northeast", is_territory: false },
    UsJurisdiction { abbr: "DE", fips2: "10", name: "Delaware", name_lower: "delaware", census_division: "South Atlantic", census_region: "South", is_territory: false },
    UsJurisdiction { abbr: "DC", fips2: "11", name: "District of Columbia", name_lower: "district of columbia", census_division: "South Atlantic", census_region: "South", is_territory: false },
    UsJurisdiction { abbr: "FL", fips2: "12", name: "Florida", name_lower: "florida", census_division: "South Atlantic", census_region: "South", is_territory: false },
    UsJurisdiction { abbr: "GA", fips2: "13", name: "Georgia", name_lower: "georgia", census_division: "South Atlantic", census_region: "South", is_territory: false },
    UsJurisdiction { abbr: "HI", fips2: "15", name: "Hawaii", name_lower: "hawaii", census_division: "Pacific", census_region: "West", is_territory: false },
    UsJurisdiction { abbr: "ID", fips2: "16", name: "Idaho", name_lower: "idaho", census_division: "Mountain", census_region: "West", is_territory: false },
    UsJurisdiction { abbr: "IL", fips2: "17", name: "Illinois", name_lower: "illinois", census_division: "East North Central", census_region: "Midwest", is_territory: false },
    UsJurisdiction { abbr: "IN", fips2: "18", name: "Indiana", name_lower: "indiana", census_division: "East North Central", census_region: "Midwest", is_territory: false },
    UsJurisdiction { abbr: "IA", fips2: "19", name: "Iowa", name_lower: "iowa", census_division: "West North Central", census_region: "Midwest", is_territory: false },
    UsJurisdiction { abbr: "KS", fips2: "20", name: "Kansas", name_lower: "kansas", census_division: "West North Central", census_region: "Midwest", is_territory: false },
    UsJurisdiction { abbr: "KY", fips2: "21", name: "Kentucky", name_lower: "kentucky", census_division: "East South Central", census_region: "South", is_territory: false },
    UsJurisdiction { abbr: "LA", fips2: "22", name: "Louisiana", name_lower: "louisiana", census_division: "West South Central", census_region: "South", is_territory: false },
    UsJurisdiction { abbr: "ME", fips2: "23", name: "Maine", name_lower: "maine", census_division: "New England", census_region: "Northeast", is_territory: false },
    UsJurisdiction { abbr: "MD", fips2: "24", name: "Maryland", name_lower: "maryland", census_division: "South Atlantic", census_region: "South", is_territory: false },
    UsJurisdiction { abbr: "MA", fips2: "25", name: "Massachusetts", name_lower: "massachusetts", census_division: "New England", census_region: "Northeast", is_territory: false },
    UsJurisdiction { abbr: "MI", fips2: "26", name: "Michigan", name_lower: "michigan", census_division: "East North Central", census_region: "Midwest", is_territory: false },
    UsJurisdiction { abbr: "MN", fips2: "27", name: "Minnesota", name_lower: "minnesota", census_division: "West North Central", census_region: "Midwest", is_territory: false },
    UsJurisdiction { abbr: "MS", fips2: "28", name: "Mississippi", name_lower: "mississippi", census_division: "East South Central", census_region: "South", is_territory: false },
    UsJurisdiction { abbr: "MO", fips2: "29", name: "Missouri", name_lower: "missouri", census_division: "West North Central", census_region: "Midwest", is_territory: false },
    UsJurisdiction { abbr: "MT", fips2: "30", name: "Montana", name_lower: "montana", census_division: "Mountain", census_region: "West", is_territory: false },
    UsJurisdiction { abbr: "NE", fips2: "31", name: "Nebraska", name_lower: "nebraska", census_division: "West North Central", census_region: "Midwest", is_territory: false },
    UsJurisdiction { abbr: "NV", fips2: "32", name: "Nevada", name_lower: "nevada", census_division: "Mountain", census_region: "West", is_territory: false },
    UsJurisdiction { abbr: "NH", fips2: "33", name: "New Hampshire", name_lower: "new hampshire", census_division: "New England", census_region: "Northeast", is_territory: false },
    UsJurisdiction { abbr: "NJ", fips2: "34", name: "New Jersey", name_lower: "new jersey", census_division: "Middle Atlantic", census_region: "Northeast", is_territory: false },
    UsJurisdiction { abbr: "NM", fips2: "35", name: "New Mexico", name_lower: "new mexico", census_division: "Mountain", census_region: "West", is_territory: false },
    UsJurisdiction { abbr: "NY", fips2: "36", name: "New York", name_lower: "new york", census_division: "Middle Atlantic", census_region: "Northeast", is_territory: false },
    UsJurisdiction { abbr: "NC", fips2: "37", name: "North Carolina", name_lower: "north carolina", census_division: "South Atlantic", census_region: "South", is_territory: false },
    UsJurisdiction { abbr: "ND", fips2: "38", name: "North Dakota", name_lower: "north dakota", census_division: "West North Central", census_region: "Midwest", is_territory: false },
    UsJurisdiction { abbr: "OH", fips2: "39", name: "Ohio", name_lower: "ohio", census_division: "East North Central", census_region: "Midwest", is_territory: false },
    UsJurisdiction { abbr: "OK", fips2: "40", name: "Oklahoma", name_lower: "oklahoma", census_division: "West South Central", census_region: "South", is_territory: false },
    UsJurisdiction { abbr: "OR", fips2: "41", name: "Oregon", name_lower: "oregon", census_division: "Pacific", census_region: "West", is_territory: false },
    UsJurisdiction { abbr: "PA", fips2: "42", name: "Pennsylvania", name_lower: "pennsylvania", census_division: "Middle Atlantic", census_region: "Northeast", is_territory: false },
    UsJurisdiction { abbr: "RI", fips2: "44", name: "Rhode Island", name_lower: "rhode island", census_division: "New England", census_region: "Northeast", is_territory: false },
    UsJurisdiction { abbr: "SC", fips2: "45", name: "South Carolina", name_lower: "south carolina", census_division: "South Atlantic", census_region: "South", is_territory: false },
    UsJurisdiction { abbr: "SD", fips2: "46", name: "South Dakota", name_lower: "south dakota", census_division: "West North Central", census_region: "Midwest", is_territory: false },
    UsJurisdiction { abbr: "TN", fips2: "47", name: "Tennessee", name_lower: "tennessee", census_division: "East South Central", census_region: "South", is_territory: false },
    UsJurisdiction { abbr: "TX", fips2: "48", name: "Texas", name_lower: "texas", census_division: "West South Central", census_region: "South", is_territory: false },
    UsJurisdiction { abbr: "UT", fips2: "49", name: "Utah", name_lower: "utah", census_division: "Mountain", census_region: "West", is_territory: false },
    UsJurisdiction { abbr: "VT", fips2: "50", name: "Vermont", name_lower: "vermont", census_division: "New England", census_region: "Northeast", is_territory: false },
    UsJurisdiction { abbr: "VA", fips2: "51", name: "Virginia", name_lower: "virginia", census_division: "South Atlantic", census_region: "South", is_territory: false },
    UsJurisdiction { abbr: "WA", fips2: "53", name: "Washington", name_lower: "washington", census_division: "Pacific", census_region: "West", is_territory: false },
    UsJurisdiction { abbr: "WV", fips2: "54", name: "West Virginia", name_lower: "west virginia", census_division: "South Atlantic", census_region: "South", is_territory: false },
    UsJurisdiction { abbr: "WI", fips2: "55", name: "Wisconsin", name_lower: "wisconsin", census_division: "East North Central", census_region: "Midwest", is_territory: false },
    UsJurisdiction { abbr: "WY", fips2: "56", name: "Wyoming", name_lower: "wyoming", census_division: "Mountain", census_region: "West", is_territory: false },
    // US Territories
    UsJurisdiction { abbr: "AS", fips2: "60", name: "American Samoa", name_lower: "american samoa", census_division: "Territories", census_region: "Territories", is_territory: true },
    UsJurisdiction { abbr: "GU", fips2: "66", name: "Guam", name_lower: "guam", census_division: "Territories", census_region: "Territories", is_territory: true },
    UsJurisdiction { abbr: "MP", fips2: "69", name: "Northern Mariana Islands", name_lower: "northern mariana islands", census_division: "Territories", census_region: "Territories", is_territory: true },
    UsJurisdiction { abbr: "PR", fips2: "72", name: "Puerto Rico", name_lower: "puerto rico", census_division: "Territories", census_region: "Territories", is_territory: true },
    UsJurisdiction { abbr: "VI", fips2: "78", name: "U.S. Virgin Islands", name_lower: "virgin islands", census_division: "Territories", census_region: "Territories", is_territory: true },
];

/// Look up a US state, DC, or territory by 2-letter postal code, 2-digit FIPS code, or full name.
pub fn lookup_us_jurisdiction(raw: &str) -> Option<&'static UsJurisdiction> {
    let cleaned = raw.trim().trim_matches('.');
    if cleaned.is_empty() {
        return None;
    }
    if cleaned.len() <= 2 && cleaned.chars().all(|c| c.is_ascii_digit()) {
        let padded = format!("{cleaned:0>2}");
        return US_JURISDICTIONS.iter().find(|j| j.fips2 == padded);
    }
    if cleaned.len() == 2 {
        let up = cleaned.to_ascii_uppercase();
        if up == "DC" {
            return US_JURISDICTIONS.iter().find(|j| j.abbr == "DC");
        }
        return US_JURISDICTIONS.iter().find(|j| j.abbr == up);
    }
    let low = cleaned
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    if low == "d.c" || low == "d.c." || low == "washington dc" || low == "washington d.c" {
        return US_JURISDICTIONS.iter().find(|j| j.abbr == "DC");
    }
    if low == "u.s. virgin islands" || low == "us virgin islands" {
        return US_JURISDICTIONS.iter().find(|j| j.abbr == "VI");
    }
    US_JURISDICTIONS.iter().find(|j| j.name_lower == low)
}

/// Normalize a 2-letter abbreviation or full state/territory name to canonical 2-letter postal code.
pub fn normalize_us_state_abbr(raw: &str) -> Option<&'static str> {
    lookup_us_jurisdiction(raw).map(|j| j.abbr)
}

/// Return the 2-digit ANSI/FIPS code (`"06"`, `"36"`, …) for a state/territory postal abbreviation.
pub fn state_fips_for_abbr(abbr: &str) -> Option<&'static str> {
    let up = abbr.trim().to_ascii_uppercase();
    US_JURISDICTIONS
        .iter()
        .find(|j| j.abbr == up)
        .map(|j| j.fips2)
}

/// Return the 2-letter postal abbreviation (`"CA"`, `"NY"`, …) for a 2-digit state FIPS code (or 5-digit county FIPS prefix).
pub fn state_abbr_for_fips(fips: &str) -> Option<&'static str> {
    let f = fips.trim();
    if f.len() < 2 {
        return None;
    }
    let f2 = &f[..2];
    US_JURISDICTIONS
        .iter()
        .find(|j| j.fips2 == f2)
        .map(|j| j.abbr)
}

/// Return the full state name (`"California"`) for a 2-letter abbreviation.
pub fn state_name_for_abbr(abbr: &str) -> Option<&'static str> {
    let up = abbr.trim().to_ascii_uppercase();
    US_JURISDICTIONS
        .iter()
        .find(|j| j.abbr == up)
        .map(|j| j.name)
}

/// True if `raw` is a valid 50-state, DC, or US territory postal code or full name.
pub fn is_valid_us_state_or_territory(raw: &str) -> bool {
    lookup_us_jurisdiction(raw).is_some()
}

/// True if `raw` is one of the 50 US states or DC (excluding territories).
pub fn is_valid_us_state_50_or_dc(raw: &str) -> bool {
    lookup_us_jurisdiction(raw).is_some_and(|j| !j.is_territory)
}

/// Official US Census division and region for a 2-letter state code.
pub fn census_division_and_region(abbr: &str) -> (&'static str, &'static str) {
    let up = abbr.trim().to_ascii_uppercase();
    US_JURISDICTIONS
        .iter()
        .find(|j| j.abbr == up && !j.is_territory)
        .map(|j| (j.census_division, j.census_region))
        .unwrap_or(("U.S. National", "National"))
}

/// All 51 two-letter codes for the 50 US states + DC.
pub fn all_50_states_and_dc_abbrs() -> impl Iterator<Item = &'static str> {
    US_JURISDICTIONS
        .iter()
        .filter(|j| !j.is_territory)
        .map(|j| j.abbr)
}
