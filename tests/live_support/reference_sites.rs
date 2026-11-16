#![allow(dead_code)]
//! Shared public civic reference sites for zero-mock live extension tests.
//!
//! Policy:
//! - Every site in this fixture is a public civic building (City Hall) or an explicit
//!   coordinate-only out-of-scope location. Zero residential addresses appear here or
//!   anywhere else in `public/tests/`.
//! - `tests/oss_license_and_secret_audit_test.rs` permits literal civic street addresses
//!   ONLY in this single file (`tests/live_support/reference_sites.rs`).

use open_estate::extensions::ExtensionQueryProto;
use std::collections::HashMap;
use std::net::TcpListener;

/// Public civic reference location used across live integration test suites.
#[derive(Debug, Clone, Copy)]
pub struct PublicCivicSite {
    pub id: &'static str,
    pub label: &'static str,
    pub address: &'static str,
    pub street: &'static str,
    pub city: &'static str,
    pub state: &'static str,
    pub zip: &'static str,
    pub county_fips: &'static str,
    pub county_name: &'static str,
    pub country: &'static str,
    pub lat: f64,
    pub lon: f64,
    pub apn: Option<&'static str>,
}

impl PublicCivicSite {
    /// Builds an [`ExtensionQueryProto`] populated with this civic site's address,
    /// county FIPS/name, coordinates, APN (when verified), and ISO country code.
    pub fn to_extension_query(&self) -> ExtensionQueryProto {
        let mut extra_params = HashMap::new();
        if !self.country.is_empty() {
            extra_params.insert("country".to_string(), self.country.to_string());
        }
        ExtensionQueryProto {
            address: self.address.to_string(),
            street: self.street.to_string(),
            city: self.city.to_string(),
            state: self.state.to_string(),
            zip: self.zip.to_string(),
            county_fips: self.county_fips.to_string(),
            lat: Some(self.lat),
            lon: Some(self.lon),
            extra_params,
            county_name: if self.county_name.is_empty() {
                None
            } else {
                Some(self.county_name.to_string())
            },
            sqft: None,
            year_built: None,
            is_condo_or_attached: None,
            hoa_covers_water_trash: None,
            is_likely_private_well: None,
            is_likely_private_septic: None,
            sdwis_water_name: None,
            sdwis_pwsid: None,
            gov_kwh_benchmark: None,
            has_solar: None,
            acs_broadband_pct: None,
            is_rural_area: None,
            apn: self.apn.map(str::to_string),
            parent_address: None,
            upstream_envelopes: Vec::new(),
        }
    }

    /// Returns `(lat, lon)` coordinates for coordinate-only live tests.
    pub const fn coordinates(&self) -> (f64, f64) {
        (self.lat, self.lon)
    }
}

/// Campbell City Hall (Campbell, Santa Clara County, CA — PG&E/SVCE, SJ Water CA4310011, West Valley Sanitation).
pub const CAMPBELL_CITY_HALL: PublicCivicSite = PublicCivicSite {
    id: "campbell_city_hall",
    label: "Campbell City Hall",
    address: "70 N 1st St, Campbell, CA 95008",
    street: "70 N 1st St",
    city: "Campbell",
    state: "CA",
    zip: "95008",
    county_fips: "06085",
    county_name: "Santa Clara",
    country: "US",
    lat: 37.288350,
    lon: -121.944163,
    apn: Some("279-41-067"),
};

/// Campbell Community Center (Campbell, Santa Clara County, CA — live Campbell MGO permits `BLD-2026-748` / `9400000568`).
pub const CAMPBELL_COMMUNITY_CENTER: PublicCivicSite = PublicCivicSite {
    id: "campbell_community_center",
    label: "Campbell Community Center",
    address: "1 W Campbell Ave, Campbell, CA 95008",
    street: "1 W Campbell Ave",
    city: "Campbell",
    state: "CA",
    zip: "95008",
    county_fips: "06085",
    county_name: "Santa Clara",
    country: "US",
    lat: 37.287840,
    lon: -121.954770,
    apn: None,
};

/// San Francisco City Hall (San Francisco, CA — DataSF live open-data permits).
pub const SF_CITY_HALL: PublicCivicSite = PublicCivicSite {
    id: "sf_city_hall",
    label: "San Francisco City Hall",
    address: "1 Dr Carlton B Goodlett Pl, San Francisco, CA 94102",
    street: "1 Dr Carlton B Goodlett Pl",
    city: "San Francisco",
    state: "CA",
    zip: "94102",
    county_fips: "06075",
    county_name: "San Francisco",
    country: "US",
    lat: 37.779260,
    lon: -122.419270,
    apn: None,
};

/// San Jose City Hall (San Jose, Santa Clara County, CA — municipal/county ortho & urban utilities).
pub const SAN_JOSE_CITY_HALL: PublicCivicSite = PublicCivicSite {
    id: "san_jose_city_hall",
    label: "San Jose City Hall",
    address: "200 E Santa Clara St, San Jose, CA 95113",
    street: "200 E Santa Clara St",
    city: "San Jose",
    state: "CA",
    zip: "95113",
    county_fips: "06085",
    county_name: "Santa Clara",
    country: "US",
    lat: 37.338040,
    lon: -121.885330,
    apn: None,
};

/// Seattle City Hall (Seattle, King County, WA — Seattle City Light municipal electric + King Co. ortho).
pub const SEATTLE_CITY_HALL: PublicCivicSite = PublicCivicSite {
    id: "seattle_city_hall",
    label: "Seattle City Hall",
    address: "600 4th Ave, Seattle, WA 98104",
    street: "600 4th Ave",
    city: "Seattle",
    state: "WA",
    zip: "98104",
    county_fips: "53033",
    county_name: "King",
    country: "US",
    lat: 47.603830,
    lon: -122.330060,
    apn: None,
};

/// Philadelphia City Hall (Philadelphia, PA — East Coast IOU territory & USGS fallback).
pub const PHILADELPHIA_CITY_HALL: PublicCivicSite = PublicCivicSite {
    id: "philadelphia_city_hall",
    label: "Philadelphia City Hall",
    address: "1400 John F Kennedy Blvd, Philadelphia, PA 19107",
    street: "1400 John F Kennedy Blvd",
    city: "Philadelphia",
    state: "PA",
    zip: "19107",
    county_fips: "42101",
    county_name: "Philadelphia",
    country: "US",
    lat: 39.952580,
    lon: -75.163570,
    apn: None,
};

/// Austin City Hall (Austin, Travis County, TX — Texas jurisdiction & non-CA/WA/PA geography).
pub const AUSTIN_CITY_HALL: PublicCivicSite = PublicCivicSite {
    id: "austin_city_hall",
    label: "Austin City Hall",
    address: "301 W 2nd St, Austin, TX 78701",
    street: "301 W 2nd St",
    city: "Austin",
    state: "TX",
    zip: "78701",
    county_fips: "48453",
    county_name: "Travis",
    country: "US",
    lat: 30.264980,
    lon: -97.746990,
    apn: None,
};

/// Miami Beach City Hall (Miami Beach, Miami-Dade County, FL — FEMA SFHA Zone AE coastal reference).
pub const MIAMI_BEACH_CITY_HALL: PublicCivicSite = PublicCivicSite {
    id: "miami_beach_city_hall",
    label: "Miami Beach City Hall",
    address: "1700 Convention Center Dr, Miami Beach, FL 33139",
    street: "1700 Convention Center Dr",
    city: "Miami Beach",
    state: "FL",
    zip: "33139",
    county_fips: "12086",
    county_name: "Miami-Dade",
    country: "US",
    lat: 25.792900,
    lon: -80.134200,
    apn: None,
};

/// New York City Hall (Manhattan, New York County, NY — NYC ACRIS / DOF Tax Lien civic reference).
pub const NYC_CITY_HALL: PublicCivicSite = PublicCivicSite {
    id: "nyc_city_hall",
    label: "New York City Hall",
    address: "260 Broadway, New York, NY 10007",
    street: "260 Broadway",
    city: "New York",
    state: "NY",
    zip: "10007",
    county_fips: "36061",
    county_name: "New York",
    country: "US",
    lat: 40.712800,
    lon: -74.006000,
    apn: None,
};

/// Brooklyn Borough Hall (Brooklyn, Kings County, NY — secondary NYC borough reference).
pub const BROOKLYN_BOROUGH_HALL: PublicCivicSite = PublicCivicSite {
    id: "brooklyn_borough_hall",
    label: "Brooklyn Borough Hall",
    address: "209 Joralemon St, Brooklyn, NY 11201",
    street: "209 Joralemon St",
    city: "Brooklyn",
    state: "NY",
    zip: "11201",
    county_fips: "36047",
    county_name: "Kings",
    country: "US",
    lat: 40.692900,
    lon: -73.990300,
    apn: None,
};

/// Columbus City Hall (Columbus, Franklin County, OH — Midwest / Ohio PR-1 title & utility reference).
pub const COLUMBUS_CITY_HALL: PublicCivicSite = PublicCivicSite {
    id: "columbus_city_hall",
    label: "Columbus City Hall",
    address: "90 W Broad St, Columbus, OH 43215",
    street: "90 W Broad St",
    city: "Columbus",
    state: "OH",
    zip: "43215",
    county_fips: "39049",
    county_name: "Franklin",
    country: "US",
    lat: 39.962300,
    lon: -83.002900,
    apn: None,
};

/// Toronto City Hall (Ontario, Canada — explicit out-of-US-scope reference site).
pub const TORONTO_CITY_HALL_OUT_OF_SCOPE: PublicCivicSite = PublicCivicSite {
    id: "toronto_city_hall",
    label: "Toronto City Hall (Out of US Scope)",
    address: "100 Queen St W, Toronto, ON M5H 2N2",
    street: "100 Queen St W",
    city: "Toronto",
    state: "ON",
    zip: "M5H 2N2",
    county_fips: "",
    county_name: "",
    country: "CA",
    lat: 43.653440,
    lon: -79.383930,
    apn: None,
};

/// Mid-Pacific ocean coordinate (explicit US coordinate with zero parcel/utility coverage).
pub const PACIFIC_OFFSHORE_OUT_OF_SCOPE: PublicCivicSite = PublicCivicSite {
    id: "pacific_offshore",
    label: "Pacific Offshore (Out of Coverage)",
    address: "",
    street: "",
    city: "",
    state: "",
    zip: "",
    county_fips: "",
    county_name: "",
    country: "US",
    lat: 36.000000,
    lon: -130.000000,
    apn: None,
};

/// All domestic US public civic reference sites.
pub const ALL_US_CIVIC_SITES: &[PublicCivicSite] = &[
    CAMPBELL_CITY_HALL,
    CAMPBELL_COMMUNITY_CENTER,
    SAN_JOSE_CITY_HALL,
    SF_CITY_HALL,
    SEATTLE_CITY_HALL,
    PHILADELPHIA_CITY_HALL,
    AUSTIN_CITY_HALL,
    MIAMI_BEACH_CITY_HALL,
    NYC_CITY_HALL,
    BROOKLYN_BOROUGH_HALL,
    COLUMBUS_CITY_HALL,
];

/// Returns a local closed TCP base URL (`http://127.0.0.1:<port>`) with nothing listening,
/// for testing genuine connection-refused error paths without mock servers.
pub fn unreachable_local_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral closed-port probe");
    let addr = listener.local_addr().expect("local_addr");
    drop(listener);
    format!("http://{addr}")
}
