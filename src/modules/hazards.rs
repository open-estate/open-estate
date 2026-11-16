// Natural Hazards module (`hazards`):
// Screens a single US residential property for FEMA NFHL flood zones & SFHA proximity,
// USGS ASCE 7-22 design shaking, ComCat historical seismicity, Quaternary faults,
// CAL FIRE Fire Hazard Severity Zones (CA) + USFS Wildfire Hazard Potential (national),
// FEMA National Risk Index (census tract), EPA/USGS/CGS Radon, and California CGS
// Seismic Hazard Zones (Alquist-Priolo, liquefaction, landslide, tsunami, dam inundation) + NHD.

use anyhow::{anyhow, Result};
use chrono::{TimeZone, Utc};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use url::Url;

use crate::shared::{
    arcgis_representative_point, bearing_deg, compass8, haversine_m, haversine_mi,
    nearest_on_polyline, point_in_ring, round_n, url_host_label, EnvConfig, HttpClient,
    ModuleOutput, SourceItem, FOOT_M, MILE_M,
};

const AGO_CGS: &str = "https://services2.arcgis.com/zr3KAIbsRSUyARHG/arcgis/rest/services";
const CALFIRE: &str = "https://services1.arcgis.com/jUJYIo9tSA7EHvfZ/arcgis/rest/services";
const CALOES: &str =
    "https://services.arcgis.com/BLN4oKB0N1YSgvY8/arcgis/rest/services/MyHazards_Hazard_Areas/FeatureServer";

pub const URL_NFHL_ZONES: &str =
    "https://hazards.fema.gov/arcgis/rest/services/public/NFHL/MapServer/28";
pub const URL_NFHL_PANELS: &str =
    "https://hazards.fema.gov/arcgis/rest/services/public/NFHL/MapServer/3";
pub const URL_NFHL_MSC_ZONES: &str =
    "https://msc.fema.gov/arcgis/rest/services/NFHL_Print/NFHLQuery/MapServer/28";
pub const URL_NFHL_MSC_PANELS: &str =
    "https://msc.fema.gov/arcgis/rest/services/NFHL_Print/NFHLQuery/MapServer/3";
pub const URL_NFHL_LIVING_ATLAS_ZONES: &str =
    "https://services.arcgis.com/P3ePLMYs2RVChkJx/arcgis/rest/services/USA_Flood_Hazard_Reduced_Set_gdb/FeatureServer/0";

pub const NFHL_SOURCE_PRIMARY: &str =
    "FEMA National Flood Hazard Layer (NFHL) - Flood Hazard Zones";
pub const NFHL_SOURCE_MSC: &str =
    "FEMA Map Service Center (MSC) National Flood Hazard Layer (NFHL)";
pub const NFHL_SOURCE_LIVING_ATLAS: &str =
    "FEMA NFHL via Esri Living Atlas (USA Flood Hazard Areas)";
pub const URL_NRI: &str = "https://services.arcgis.com/XG15cJAlne2vxtgt/arcgis/rest/services/National_Risk_Index_Census_Tracts/FeatureServer/0";
pub const URL_ASCE722: &str = "https://earthquake.usgs.gov/ws/designmaps/asce7-22.json";
pub const URL_COMCAT_QUERY: &str = "https://earthquake.usgs.gov/fdsnws/event/1/query";
pub const URL_COMCAT_COUNT: &str = "https://earthquake.usgs.gov/fdsnws/event/1/count";
pub const URL_QFAULTS: &str =
    "https://earthquake.usgs.gov/arcgis/rest/services/haz/Qfaults/MapServer/21";
pub const URL_WHP: &str = "https://imagery.geoplatform.gov/iipp/rest/services/Fire_Aviation/USFS_EDW_RMRS_WildfireHazardPotentialClassified/ImageServer";
pub const URL_GRP: &str = "https://services.arcgis.com/cJ9YHowT8TU7DUyn/arcgis/rest/services/Geologic_Radon_Potential_Map_of_the_United_States/FeatureServer/0";
pub const URL_EPQS: &str = "https://epqs.nationalmap.gov/v1/json";
pub const URL_3DEP_IMAGE: &str =
    "https://elevation.nationalmap.gov/arcgis/rest/services/3DEPElevation/ImageServer";
pub const URL_OPEN_METEO_ELEVATION: &str = "https://api.open-meteo.com/v1/elevation";

pub const ELEV_SOURCE_EPQS: &str = "USGS 3DEP Elevation Point Query Service";
pub const ELEV_SOURCE_3DEP_IMAGE: &str = "USGS 3DEP Elevation ImageServer";
pub const ELEV_SOURCE_COPERNICUS: &str =
    "Copernicus DEM GLO-90 via Open-Meteo.com Elevation API (CC BY 4.0 Non-Commercial)";

/// How long the USGS 3DEP services may take before the Copernicus DEM answer is used instead.
/// EPQS routinely needs 10 s or more; left to the generic retry path it stalled the whole hazards
/// module for half a minute and still produced no elevation.
const ELEV_USGS_BUDGET: std::time::Duration = std::time::Duration::from_millis(6_000);
/// Delay before the 3DEP ImageServer (the raster service behind EPQS) is queried as a hedge.
const ELEV_HEDGE_DELAY: std::time::Duration = std::time::Duration::from_millis(1_500);
/// Transport timeout for elevation requests. Deliberately longer than `ELEV_USGS_BUDGET`: a slow
/// elevation service is abandoned by our own deadline, never by a transport timeout, so it cannot
/// put the egress proxies into their failure cool-down.
const ELEV_REQ_TIMEOUT_MS: u64 = 9_000;

const REQ_TIMEOUT_MS: u64 = 9_500;
const TTL_7D: i64 = 86_400 * 7;

/// Per-attempt timeout, retry budget, and hedging deadlines for FEMA NFHL queries.
/// `hazards.fema.gov` intermittently resets TLS or stalls on a subset of CDN edge IPs, so we
/// retry transient failures and hedge to official FEMA MSC / Living Atlas mirrors within a
/// bounded total budget.
const NFHL_REQ_TIMEOUT_MS: u64 = 5_500;
const NFHL_RETRIES: usize = 2;
const NFHL_PRIMARY_BUDGET: std::time::Duration = std::time::Duration::from_millis(6_500);
const NFHL_HEDGE_DELAY: std::time::Duration = std::time::Duration::from_millis(2_000);
const NFHL_TOTAL_BUDGET: std::time::Duration = std::time::Duration::from_millis(12_000);

const NRI_HAZ: &[(&str, &str)] = &[
    ("ERQK", "Earthquake"),
    ("IFLD", "Riverine Flooding"),
    ("CFLD", "Coastal Flooding"),
    ("WFIR", "Wildfire"),
    ("HRCN", "Hurricane"),
    ("TRND", "Tornado"),
    ("SWND", "Strong Wind"),
    ("HAIL", "Hail"),
    ("HWAV", "Heat Wave"),
    ("CWAV", "Cold Wave"),
    ("WNTW", "Winter Weather"),
    ("ISTM", "Ice Storm"),
    ("DRGT", "Drought"),
    ("LTNG", "Lightning"),
    ("LNDS", "Landslide"),
    ("TSUN", "Tsunami"),
    ("VLCN", "Volcanic Activity"),
    ("AVLN", "Avalanche"),
];

const OTHER_HAZ: &[&str] = &[
    "HRCN", "TRND", "SWND", "HAIL", "HWAV", "CWAV", "WNTW", "ISTM", "DRGT", "LTNG", "LNDS", "TSUN",
    "VLCN", "AVLN",
];

// EPA Map of Radon Zones (1993), 3,142 counties across 51 states/DC.
// Each state maps to concatenated 4-char records "<countyFips3><zone>".
const RADON_TABLE: &[(&str, &str)] = &[
    ("01", "0012003300520072009201120133015101730192021202330253027102910313033103530371039304130432045304720492051205330552057205910613063206520673069307110732075207710791081208310852087208910913093209520973099310121031105210721093111211321152117211921211123312521272129313131332"),
    ("02", "01330163020205030603068307030902100310531103122213031503164317021803185318831953198322032303240226132703275328232903"),
    ("04", "001200320052007200920112012201320152017201920212023202520272"),
    ("05", "001300330052007200920113013301520173019302130233025302730293031303330353037303930413043304530473049205120533055305730593061306320652067306930713073307530773079308130833085308730892091309330953097209931013103310531073109311131133115311731193121212331253127312921313133313521372139314131433145314731493"),
    ("06", "0012003200520073009201130132015301720192021302330253027202920313033303530372039204130432045304730493051205320553057205930612063206520673069207120733075207730792081208310852087208930912093309530973099310131033105310721092111111331152"),
    ("08", "001100320051007200910111013101510171019102120232025102710291031103310351037203910411043104510471049105110532055105710591061106310651067206910711073107510771079208110831085108710891091109310951097109911011103110521072109211121131115111711191121112311251"),
    ("09", "00110033005200710091011101320152"),
    ("10", "001300320053"),
    ("11", "0013"),
    ("12", "0012003300530073009301130133015301720193021302320273029303130333035303730393041304330453047304930513053305530572059306130633065306730693071307320753077307930813083208530862087308930913093309530973099310131033105210731093111311331153117311931213123312521273129313131333"),
    ("13", "001300330053007300930112013201520173019302130233025302730293031303330352037303930433045204720493051305330553057205920613063206530671069307130733075307720793081308330852087308910913093309530972099310131033105210731093111211321152117211921211123212531273129313131332135113721392141314321452147214921512153315531572159216131633165316731693171217331753177317931813183318531872189319131933195219731992201320532072209321122133215321722192221222322253227222932312233323532373239324122433245224722493251325332552257225932613263226532673269327132733275327732793281228332852287328932912293229522972299330133033305330733093311231323153317331933213"),
    ("15", "00130033005300730093"),
    ("16", "00120033005200720091011201310151017101920211023102510272029203120331035103710391041204310453047104910512053205510571059106130632065206720693071207320752077207910812083208510873"),
    ("17", "001100330052007100910111013101510171019102120232025202720291031203320352037103910411043204510472049205120531055205710592061106310652067106920711073107510772079208120831085108720891091209310951097209911012103110511071109111121131115111721192121212311251127312911311133213521371139114111431145214711491151215331551157215921611163216521671169117111732175117711791181218311852187118921912193219511972199220112031"),
    ("18", "00110031005100710091011101320151017101910212023102520272029203110331035103720391041104320451047204910512053105520571059106110631065106710691071207320751077207910811083208510871089209120931095109710991101210311051107110921112113111521171119212121232125212721292131213311351137213911411143114511472149215111532155215711591161116321651167216911711173217511771179118111831"),
    ("19", "001100310051007100910111013101510171019102110231025102710291031103310351037103910411043104510471049105110531055105710591061106310651067106910711073107510771079108110831085108710891091109310951097109911011103110511071109111111131115111711191121112311251127112911311133113511371139114111431145114711491151115311551157115911611163116511671169117111731175117711791181118311851187118911911193119511971"),
    ("20", "001200320051007200910112013101520172019202120231025202710291031203320352037203910411043204510472049205110531055105710592061106310651067106910711073207510772079208110831085108720891091109310951097109921011103110511072109111121131115111711191121212311252127212921311133213511371139214111431145114711491151115311552157115911611163116511671169117111732175217721791181118311852187118921912193119511972199120112031205220722091"),
    ("21", "001100310052007300910112013201520171019202110232025202720291031203320353037203930412043204510472049105120532055205710592061206320652067106920712073107530772079208120833085208710892091209320952097109911012103210531072109211111131115211721192121212321252127212921312133213521371139214121432145314721492151215321551157315921612163216521671169117111732175217721791181218321852187218921911193219521972199120112032205220712091211221322152217121922212223222522271229223122332235223722391"),
    ("22", "0013003300530073009301130133015301730193021302330253027302930313033303530373039304130433045304730493051305330553057305930613063306530673069307130733075307730793081308330853087308930913093309530973099310131033105310731093111311331153117311931213123312531273"),
    ("23", "0011003100510071009101110132015101710191021102320251027202920311"),
    ("24", "001200320051009101130131015201720193021102320251027102930311033203530372039304130431045304735102"),
    ("25", "00120032005200720091011201320152017101920212023202530271"),
    ("26", "00120032005300720092011301320152017301930213023102510271029203130333035303720393041304320452047204920513053205530573059106120633065206720693071207330751077107930812083208530872089209110932095309730993101310321053107310921113113311521172119212131233125212731293131313331353137213931412143314531473149115121533155215731593161116331653"),
    ("27", "001200320051007200920111013101510172019102120231025202710292031203310352037103910411043104510471049105110531055105710592061206310651067106910712073107520772079108110831085108710891091109310952097209911011103110511071109111111131115211711191121112311251127112911311133113511372139114111431145114711491151115311551157115911611163116511671169117111731"),
    ("28", "0013003200530073009301130133015301720193021302330252027302930313033303530373039304130433045304730493051305330553057305930613063306530673069307130733075307730793081208330853087208930913093309530973099310131032105310731093111311331152117311931213123312531273129313131333135313731393141314331452147314931513153315531573159316131633"),
    ("29", "0012003100510072009201120132015201720192021102330252027202920312033203520371039204120432045204710491051205320552057205920612063206520672069307120732075207720792081208320852087108920912093109510972099210121032105210721092111211321152117211921212123212521272129213121333135213721392141214331452147114921512153215531572159216121632165116721692171217321752177217921812183218521862187218921952197219922013203220522073209221122132215221722192221222322252227222925102"),
    ("30", "00110031005100710091011101310151017101910211023102510271029103110331035103720391041104310451047104910511053105510571059106110631065206710692071107310751077107910811083108510871089109110931095109720991101110321051107210911112"),
    ("31", "001100320053007200930111013201510173019202110231025102710292031303320351037103910412043104520472049205110531055105720591061106310651067106930713073107530771079208110831085108710893091309320951097109911012103210521071109111131133115311731191121212321251127112911311133113521371139114111431145114711493151115311551157215911612163216521671169117131731175217711791181118331851"),
    ("32", "00120033005100720092011101320151017101910211023202710292031203315101"),
    ("33", "0012003100520072009201120132015201720192"),
    ("34", "001300320052007200930112013201520172019102110232025102710293031203320351037103920411"),
    ("35", "001100320052006200710092011201320152017201920212023202520272028202920312033103520372039104120432045204710491051205320551057205920612"),
    ("36", "00110031005300710091011101310151017101920211023102510271029103130333035303710391041304330452047304920511053105520572059306130632065206710691071107320752077107910813083108530873089209120932095109710991101110331051107110911111113311511172119312111231"),
    ("37", "0013003200510073009201120133015301730193021102320253027202930313033203520373039104130432045204730493051305330553057305930613063306530672069207120733075207730793081308330853087208910913093309530972099210131033105310731092111211321152117311931211123312531273129313131333135313731393141314331453147314921513153315531571159316121633165316731692171217321751177317931812183218521873189119131932195319721992"),
    ("38", "00110031005100710091011101310151017101910211023102510271029103110331035103710391041104310451047104910511053105510571059106110631065106710691071107310751077107910811083108510871089109110931095109710991101110311051"),
    ("39", "0011003100510072009201110131015201710191021102310252027102910311033103520371039204110432045104710491051205320552057105910611063106510671069207120732075107710792081108310852087208910911093209520971099210111032105210711091111211311152117111911212123212521271129113111332135113721391141114321452147114911511153115521571159116111632165116721691171217321751"),
    ("40", "00120033005300720093011301330153017301930212023302520273029303130333035303730393041204330452047304930513053305530573059306130633065306730693071307330753077307930813083308530873089309130933095309720993101310331053107310931113113311531173119312131233125312731293131313331352137313921413143314531473149315131533"),
    ("41", "001200330053007200920113013201530173019302120232025202720293031203330352037203930413043304520473049205120533055205730592061206330652067206920712"),
    ("42", "0011003100510071009101110131015101710191021202310251027102910311033103510371039204110431045104720492051205320551057105920611063106520671069107110732075107710791081108320852087108910911093109510971099110131032105210711091111211311151117111911211123212521272129113111331"),
    ("44", "00130031005200720091"),
    ("45", "0012003300530072009301130133015301730193021202330253027302930313033303530373039304130433045104730493051305330553057305920613063306530673069307130732075307720793081308320853087308930912"),
    ("46", "003100510072009101110131015101710192021102310251027102910311033203510371039104120431045104720491051105320552057105910611063206510671069107120731075207710791081208310851087108910911093209520971099110111032105110711091111111321151117111911212123212511271129113511372"),
    ("47", "00110031005200730091011101330152017301920212023202510272029203120333035303710392041204320453047304920513053305510571059106130631065206710693071207310753077207930811083208510871089109120931095309730991101110311051107110921112113311531171119112111231125212711293131313321351137213921412143314511472149115131533155215731591161216311652167316911712173117521772179118111833185218711891"),
    ("48", "00130033005300730093011201330153017201930213023302530273029303130333035303730393041304320453047304930513053305530573059306130633065206730692071307330753077307930813083308530873089309130933095309730993101310331053107210921112113311531172119312131233125312731292131313331353137313931413143314531473149315131532155315731593161316331653167316921713173317531773179218131833185318731892191319331952197319932013203320522073209321122133215321732192221322332253227322922313233223532373239324132432245324732493251325332553257325932613263326532673269327132733275327732792281328332853287328932913293329522973299230133032305230733093311331333153317331923213323332533273329333133333335333733393341234333453347334933513353335533572359236133633365336733692371337333752377237933812383338533873389239133932395339733993401340334053407340934113413341534173419342124233425342734293431343334353437243934413443244534473449345134533455345734593461346334653467346934713473347534773479348134833485348734893491349334953497349935013503350535073"),
    ("49", "00120032005200710092011201310152017201910212023202520272029203110332035203720391041104320452047104920512053205520572"),
    ("50", "00120032005200730092011201330152017201920212023202520272"),
    ("51", "00130032005100710092011101320151017101920211023102510272029103110333035203630372041104310451047204910512053105730591061206320651067206910711073307510772079208130832085308730891091109330953097309931013103310511072109111121132115311721193121112521273131313331351137113911411143114511472149315321551157215931611163116511671169117111731175317711791181318331851187119111933195219711993510351515201530154025503570358015901595360016101620263036402650366016703678168026831685169017003710372027303735374037501760377017751790180038103820183038401"),
    ("53", "001200320052007300930111013201530172019102120232025202730293031303330353037203920413043204530471049305110533055305730591061306310651067306930712073307520772"),
    ("54", "0012003100530072009101120132015201720192021202310251027102910311033203520371039304120432045304730492051105320551057105930611063106510672069107110732075107710792081208320852087208910912093209520972099210121031105210721093"),
    ("55", "001200320052007200920111013201520172019202120231025102710291031203320352037203910412043104510471049105120532055105720592061206320651067106920712073107520772078107920812083208520872089209110931095209710992101210311051107210911112113211511172119212121231125212711292131113311351137213921411"),
    ("56", "00110031005100710091011101310151017101910211023102510271029103120331035103710391041104310452"),
];

static SFHA_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(A|AE|AH|AO|AR|A99|V|VE|A\d{1,2}|V\d{1,2})$").unwrap());
static HREF_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r#"href="([^"]+)"[^>]*>([^<]+)<"#).unwrap());
static FEET_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)(\d+)\s*Feet").unwrap());

/// Look up EPA county radon zone (`1`, `2`, or `3`) for a 5-digit county FIPS code.
pub fn epa_radon_zone(county_fips5: &str) -> Option<u8> {
    let digits: String = county_fips5.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || digits.len() > 5 {
        return None;
    }
    let f = format!("{digits:0>5}");
    let st = &f[0..2];
    let co = &f[2..5];
    let (_, rec) = RADON_TABLE.iter().find(|&&(s, _)| s == st)?;
    let bytes = rec.as_bytes();
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if &rec[i..i + 3] == co {
            return rec[i + 3..i + 4].parse::<u8>().ok();
        }
        i += 4;
    }
    None
}

pub fn epa_radon_label(zone: u8) -> &'static str {
    match zone {
        1 => "Zone 1 - highest potential (predicted average indoor level > 4 pCi/L)",
        2 => "Zone 2 - moderate potential (2-4 pCi/L)",
        3 => "Zone 3 - low potential (< 2 pCi/L)",
        _ => "Unknown radon zone",
    }
}

pub fn flood_zone_info(zone: &str, subtype: &str) -> (bool, &'static str, String) {
    let z = zone.trim().to_uppercase();
    let sub = subtype.to_uppercase();
    let sfha = SFHA_RE.is_match(&z);
    let (risk, mut label) = if z.starts_with('V') {
        ("very high", "Coastal high-hazard area: 1% annual chance flood with storm-wave action".to_string())
    } else if z == "AO" {
        ("high", "1% annual chance shallow flooding (sheet flow, usually 1-3 ft deep)".to_string())
    } else if z == "AH" {
        ("high", "1% annual chance shallow flooding (ponding, usually 1-3 ft deep)".to_string())
    } else if z == "AE" || (z.starts_with('A') && z[1..].chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false)) {
        ("high", "1% annual chance (100-year) floodplain with base flood elevations".to_string())
    } else if z == "A" {
        ("high", "1% annual chance (100-year) floodplain, no base flood elevation determined".to_string())
    } else if z == "AR" {
        ("high", "1% annual chance floodplain behind a levee being restored".to_string())
    } else if z == "A99" {
        ("high", "1% annual chance floodplain to be protected by a federal levee under construction".to_string())
    } else if (z == "X" && (sub.contains("0.2 PCT") || sub.contains("1 PCT") || sub.contains("LEVEE"))) || z == "B" {
        let lbl = if sub.contains("LEVEE") {
            "Moderate risk: reduced flood risk due to a levee (residual risk)"
        } else if sub.contains("1 PCT") {
            "Moderate risk: 1% annual chance flooding less than 1 ft deep or from a small drainage area"
        } else {
            "Moderate risk: 0.2% annual chance (500-year) floodplain"
        };
        ("moderate", lbl.to_string())
    } else if z == "X" || z == "C" {
        ("minimal", "Minimal flood hazard (outside the mapped 500-year floodplain)".to_string())
    } else if z == "D" {
        ("undetermined", "Possible but undetermined flood hazard (area not studied by FEMA)".to_string())
    } else if z == "OPEN WATER" {
        ("high", "Mapped as open water".to_string())
    } else if z == "AREA NOT INCLUDED" {
        ("unknown", "Area not included in the FEMA flood map".to_string())
    } else {
        ("unknown", "Unrecognized flood zone".to_string())
    };
    if sfha && sub.contains("FLOODWAY") {
        label.push_str(" - regulatory floodway (most restrictive)");
    }
    (sfha, risk, label)
}

fn val_num(v: Option<&Value>) -> Option<f64> {
    let x = v?;
    let n = x
        .as_f64()
        .or_else(|| x.as_str().and_then(|s| s.trim().parse::<f64>().ok()))?;
    if !n.is_finite() || n <= -9999.0 {
        None
    } else {
        Some(n)
    }
}

fn val_str<'a>(v: Option<&'a Value>) -> Option<&'a str> {
    v.and_then(|x| x.as_str()).map(|s| s.trim()).filter(|s| !s.is_empty())
}

fn iso_date_ms(v: Option<&Value>) -> Option<String> {
    let ms = v.and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f as i64)))?;
    Utc.timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.format("%Y-%m-%d").to_string())
}

fn fmt_mi(m: f64) -> String {
    if m < 160.0 {
        format!("{} ft", (m / FOOT_M).round() as i64)
    } else if m < 16_093.0 {
        format!("{:.1} mi", m / MILE_M)
    } else {
        format!("{:.0} mi", m / MILE_M)
    }
}

fn fmt_dist(m: f64) -> String {
    let metric = if m < 1000.0 {
        format!("{} m", m.round() as i64)
    } else {
        format!("{:.1} km", m / 1000.0)
    };
    format!("{} ({metric})", fmt_mi(m))
}

fn is_rated_nri(r: &str) -> bool {
    matches!(
        r,
        "Very High" | "Relatively High" | "Relatively Moderate" | "Relatively Low" | "Very Low"
    )
}

/// Parse ArcGIS `rings` or `paths` (`[[[lon, lat], ...], ...]`) into `Vec<Vec<(f64, f64)>>` (`(lat, lon)`).
fn parse_coord_lists(arr_val: Option<&Value>) -> Vec<Vec<(f64, f64)>> {
    let Some(outer) = arr_val.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(outer.len());
    for seq in outer {
        if let Some(pts) = seq.as_array() {
            let mut coords = Vec::with_capacity(pts.len());
            for p in pts {
                if let (Some(lon), Some(lat)) = (
                    p.get(0).and_then(|v| v.as_f64()),
                    p.get(1).and_then(|v| v.as_f64()),
                ) {
                    if lat.is_finite() && lon.is_finite() {
                        coords.push((lat, lon));
                    }
                }
            }
            if !coords.is_empty() {
                out.push(coords);
            }
        }
    }
    out
}

/// Minimum distance in meters from `(lat, lon)` to an ArcGIS polygon (`0.0` if inside), plus nearest `(lat, lon)`.
fn polygon_distance(lat: f64, lon: f64, geom: &Value) -> Option<(f64, f64, f64)> {
    let rings = parse_coord_lists(geom.get("rings"));
    if rings.is_empty() {
        let (rlat, rlon) = arcgis_representative_point(geom)?;
        return Some((haversine_m(lat, lon, rlat, rlon), rlat, rlon));
    }
    let mut inside = false;
    for r in &rings {
        if point_in_ring(lat, lon, r) {
            inside = !inside;
        }
    }
    if inside {
        return Some((0.0, lat, lon));
    }
    paths_distance(lat, lon, &rings)
}

/// Minimum distance in meters from `(lat, lon)` to polyline paths `Vec<Vec<(lat, lon)>>`.
fn paths_distance(lat: f64, lon: f64, paths: &[Vec<(f64, f64)>]) -> Option<(f64, f64, f64)> {
    let mut best: Option<(f64, f64, f64)> = None;
    for p in paths {
        if p.len() == 1 {
            let d = haversine_m(lat, lon, p[0].0, p[0].1);
            if best.map(|b| d < b.0).unwrap_or(true) {
                best = Some((d, p[0].0, p[0].1));
            }
        } else if let Some(hit) = nearest_on_polyline(lat, lon, p) {
            if best.map(|b| hit.dist_m < b.0).unwrap_or(true) {
                best = Some((hit.dist_m, hit.lat, hit.lon));
            }
        }
    }
    best
}

fn shoelace_area_xy(pts: &[(f64, f64)]) -> f64 {
    if pts.len() < 3 {
        return 0.0;
    }
    let mut sum = 0.0;
    for i in 0..pts.len() {
        let j = (i + 1) % pts.len();
        sum += pts[i].0 * pts[j].1 - pts[j].0 * pts[i].1;
    }
    0.5 * sum
}

fn sutherland_hodgman_clip_ring(
    raw_ring: &[(f64, f64)],
    min_x: f64,
    max_x: f64,
    min_y: f64,
    max_y: f64,
) -> Option<Vec<(f64, f64)>> {
    let mut poly: Vec<(f64, f64)> = raw_ring.to_vec();
    if poly.len() > 1 {
        let f = poly[0];
        let l = *poly.last().unwrap();
        if (f.0 - l.0).abs() < 1e-11 && (f.1 - l.1).abs() < 1e-11 {
            poly.pop();
        }
    }
    if poly.len() < 3 {
        return None;
    }
    let (mut rmin_x, mut rmax_x, mut rmin_y, mut rmax_y) = (f64::INFINITY, f64::NEG_INFINITY, f64::INFINITY, f64::NEG_INFINITY);
    for &(x, y) in &poly {
        rmin_x = rmin_x.min(x);
        rmax_x = rmax_x.max(x);
        rmin_y = rmin_y.min(y);
        rmax_y = rmax_y.max(y);
    }
    if rmax_x < min_x || rmin_x > max_x || rmax_y < min_y || rmin_y > max_y {
        return None;
    }

    fn clip_edge<FI, FX>(pts: &[(f64, f64)], inside: FI, intersect: FX) -> Vec<(f64, f64)>
    where
        FI: Fn((f64, f64)) -> bool,
        FX: Fn((f64, f64), (f64, f64)) -> (f64, f64),
    {
        if pts.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(pts.len() + 4);
        let mut prev = *pts.last().unwrap();
        let mut prev_in = inside(prev);
        for &curr in pts {
            let curr_in = inside(curr);
            if curr_in {
                if !prev_in {
                    out.push(intersect(prev, curr));
                }
                out.push(curr);
            } else if prev_in {
                out.push(intersect(prev, curr));
            }
            prev = curr;
            prev_in = curr_in;
        }
        out
    }

    let intersect_x = |edge_x: f64, a: (f64, f64), b: (f64, f64)| -> (f64, f64) {
        let dx = b.0 - a.0;
        if dx.abs() < 1e-15 {
            (edge_x, a.1)
        } else {
            let t = ((edge_x - a.0) / dx).clamp(0.0, 1.0);
            (edge_x, a.1 + t * (b.1 - a.1))
        }
    };
    let intersect_y = |edge_y: f64, a: (f64, f64), b: (f64, f64)| -> (f64, f64) {
        let dy = b.1 - a.1;
        if dy.abs() < 1e-15 {
            (a.0, edge_y)
        } else {
            let t = ((edge_y - a.1) / dy).clamp(0.0, 1.0);
            (a.0 + t * (b.0 - a.0), edge_y)
        }
    };

    poly = clip_edge(&poly, |p| p.0 >= min_x, |a, b| intersect_x(min_x, a, b));
    poly = clip_edge(&poly, |p| p.0 <= max_x, |a, b| intersect_x(max_x, a, b));
    poly = clip_edge(&poly, |p| p.1 >= min_y, |a, b| intersect_y(min_y, a, b));
    poly = clip_edge(&poly, |p| p.1 <= max_y, |a, b| intersect_y(max_y, a, b));

    let mut dedup: Vec<(f64, f64)> = Vec::with_capacity(poly.len());
    for p in poly {
        if let Some(last) = dedup.last() {
            if (p.0 - last.0).abs() < 1e-11 && (p.1 - last.1).abs() < 1e-11 {
                continue;
            }
        }
        dedup.push(p);
    }
    if dedup.len() > 1 {
        let f = dedup[0];
        let l = *dedup.last().unwrap();
        if (f.0 - l.0).abs() < 1e-11 && (f.1 - l.1).abs() < 1e-11 {
            dedup.pop();
        }
    }
    if dedup.len() < 3 || shoelace_area_xy(&dedup).abs() < 1e-12 {
        None
    } else {
        Some(dedup)
    }
}

fn dp_simplify_open(pts: &[(f64, f64)], tol_deg: f64, cos_lat: f64) -> Vec<(f64, f64)> {
    if pts.len() <= 2 {
        return pts.to_vec();
    }
    let tol_sq = tol_deg * tol_deg;
    let mut keep = vec![false; pts.len()];
    keep[0] = true;
    keep[pts.len() - 1] = true;
    let mut stack = vec![(0usize, pts.len() - 1)];
    while let Some((start, end)) = stack.pop() {
        if end <= start + 1 {
            continue;
        }
        let a = (pts[start].0 * cos_lat, pts[start].1);
        let b = (pts[end].0 * cos_lat, pts[end].1);
        let dx = b.0 - a.0;
        let dy = b.1 - a.1;
        let len_sq = dx * dx + dy * dy;
        let (mut best_idx, mut best_d2) = (start, -1.0_f64);
        for (i, pt) in pts.iter().enumerate().take(end).skip(start + 1) {
            let px = pt.0 * cos_lat;
            let py = pt.1;
            let d2 = if len_sq < 1e-24 {
                (px - a.0).powi(2) + (py - a.1).powi(2)
            } else {
                let t = (((px - a.0) * dx + (py - a.1) * dy) / len_sq).clamp(0.0, 1.0);
                let cx = a.0 + t * dx;
                let cy = a.1 + t * dy;
                (px - cx).powi(2) + (py - cy).powi(2)
            };
            if d2 > best_d2 {
                best_d2 = d2;
                best_idx = i;
            }
        }
        if best_d2 > tol_sq && best_idx > start && best_idx < end {
            keep[best_idx] = true;
            stack.push((start, best_idx));
            stack.push((best_idx, end));
        }
    }
    pts.iter().enumerate().filter_map(|(i, &p)| if keep[i] { Some(p) } else { None }).collect()
}

fn simplify_and_round_ring(
    open: &[(f64, f64)],
    tol_deg: f64,
    cos_lat: f64,
    max_pts: usize,
) -> Option<Vec<(f64, f64)>> {
    if open.len() < 3 {
        return None;
    }
    let mut split_idx = 1usize;
    let mut max_d2 = -1.0_f64;
    for (i, p) in open.iter().enumerate().skip(1) {
        let d2 = ((p.0 - open[0].0) * cos_lat).powi(2) + (p.1 - open[0].1).powi(2);
        if d2 > max_d2 {
            max_d2 = d2;
            split_idx = i;
        }
    }
    let run_dp = |tol: f64| -> Vec<(f64, f64)> {
        let first_half = dp_simplify_open(&open[..=split_idx], tol, cos_lat);
        let mut tail = Vec::with_capacity(open.len() - split_idx + 1);
        tail.extend_from_slice(&open[split_idx..]);
        tail.push(open[0]);
        let second_half = dp_simplify_open(&tail, tol, cos_lat);
        let mut out = first_half;
        if second_half.len() > 2 {
            out.extend_from_slice(&second_half[1..second_half.len() - 1]);
        }
        out
    };
    let mut tol = tol_deg;
    let mut simplified = run_dp(tol);
    if max_pts >= 4 {
        for _ in 0..10 {
            if simplified.len() <= max_pts {
                break;
            }
            tol *= 1.5;
            let next = run_dp(tol);
            if next.len() < 3 {
                break;
            }
            simplified = next;
        }
    }
    let mut rounded: Vec<(f64, f64)> = Vec::with_capacity(simplified.len() + 1);
    for (x, y) in simplified {
        let rx = round_n(x, 5);
        let ry = round_n(y, 5);
        if rounded.last().map(|&(px, py)| px == rx && py == ry).unwrap_or(false) {
            continue;
        }
        rounded.push((rx, ry));
    }
    if rounded.len() > 1 && rounded.first() == rounded.last() {
        rounded.pop();
    }
    if rounded.len() < 3 || shoelace_area_xy(&rounded).abs() < 1e-11 {
        return None;
    }
    Some(rounded)
}

fn clip_polyline_liang_barsky(
    pts: &[(f64, f64)],
    min_x: f64,
    max_x: f64,
    min_y: f64,
    max_y: f64,
) -> Vec<Vec<(f64, f64)>> {
    let mut out = Vec::new();
    let mut cur: Vec<(f64, f64)> = Vec::new();
    for w in pts.windows(2) {
        let (x0, y0) = w[0];
        let (x1, y1) = w[1];
        let dx = x1 - x0;
        let dy = y1 - y0;
        let mut t0 = 0.0_f64;
        let mut t1 = 1.0_f64;
        let mut ok = true;
        for (p, q) in [(-dx, x0 - min_x), (dx, max_x - x0), (-dy, y0 - min_y), (dy, max_y - y0)] {
            if p.abs() < 1e-15 {
                if q < 0.0 {
                    ok = false;
                    break;
                }
            } else {
                let r = q / p;
                if p < 0.0 {
                    if r > t1 { ok = false; break; }
                    if r > t0 { t0 = r; }
                } else {
                    if r < t0 { ok = false; break; }
                    if r < t1 { t1 = r; }
                }
            }
        }
        if !ok {
            if cur.len() >= 2 {
                out.push(std::mem::take(&mut cur));
            } else {
                cur.clear();
            }
            continue;
        }
        let a = (x0 + t0 * dx, y0 + t0 * dy);
        let b = (x0 + t1 * dx, y0 + t1 * dy);
        if cur.is_empty() {
            cur.push(a);
        } else if let Some(&last) = cur.last() {
            if (last.0 - a.0).abs() > 1e-9 || (last.1 - a.1).abs() > 1e-9 {
                if cur.len() >= 2 {
                    out.push(std::mem::take(&mut cur));
                } else {
                    cur.clear();
                }
                cur.push(a);
            }
        }
        if cur.last().map(|&p| (p.0 - b.0).abs() > 1e-11 || (p.1 - b.1).abs() > 1e-11).unwrap_or(true) {
            cur.push(b);
        }
        if t1 < 1.0 - 1e-9 {
            if cur.len() >= 2 {
                out.push(std::mem::take(&mut cur));
            } else {
                cur.clear();
            }
        }
    }
    if cur.len() >= 2 {
        out.push(cur);
    }
    out
}

/// Clip an ArcGIS Polygon/Polyline/Point geometry to a parcel-centered bounding box (`half_box_m`),
/// simplify parts with Douglas-Peucker (`tol_deg`), preserve outer/hole rings cleanly, and round to 5 decimals.
pub fn clip_and_simplify_arcgis_geometry(
    geom: &Value,
    center_lat: f64,
    center_lon: f64,
    half_box_m: f64,
    tol_deg: f64,
    max_pts_per_part: usize,
) -> Option<Value> {
    if let (Some(x), Some(y)) = (geom.get("x").and_then(|v| v.as_f64()), geom.get("y").and_then(|v| v.as_f64())) {
        return Some(json!({ "type": "Point", "coordinates": [round_n(x, 5), round_n(y, 5)] }));
    }
    let cos_lat = center_lat.to_radians().cos().abs().max(0.1);
    let dlat = half_box_m / 111_320.0;
    let dlon = half_box_m / (111_320.0 * cos_lat);
    let (min_x, max_x) = (center_lon - dlon, center_lon + dlon);
    let (min_y, max_y) = (center_lat - dlat, center_lat + dlat);

    if let Some(rings_arr) = geom.get("rings").and_then(|v| v.as_array()) {
        struct RingItem {
            is_outer: bool,
            area_abs: f64,
            pts: Vec<(f64, f64)>,
        }
        let mut parsed: Vec<(f64, Vec<(f64, f64)>)> = Vec::with_capacity(rings_arr.len());
        let mut any_neg = false;
        for r in rings_arr {
            let Some(pts_arr) = r.as_array() else { continue };
            let xy: Vec<(f64, f64)> = pts_arr
                .iter()
                .filter_map(|p| Some((p.get(0)?.as_f64()?, p.get(1)?.as_f64()?)))
                .filter(|(x, y)| x.is_finite() && y.is_finite())
                .collect();
            if xy.len() < 3 {
                continue;
            }
            let a = shoelace_area_xy(&xy);
            if a < 0.0 {
                any_neg = true;
            }
            parsed.push((a, xy));
        }

        let mut clipped_items: Vec<RingItem> = Vec::new();
        for (raw_area, xy) in parsed {
            if let Some(cl) = sutherland_hodgman_clip_ring(&xy, min_x, max_x, min_y, max_y) {
                let area_abs = shoelace_area_xy(&cl).abs();
                let is_outer = if any_neg { raw_area < 0.0 } else { true };
                clipped_items.push(RingItem { is_outer, area_abs, pts: cl });
            }
        }
        if clipped_items.is_empty() {
            return None;
        }
        let dense_raster_fan = clipped_items.len() > 80;
        let mut outers: Vec<RingItem> = Vec::new();
        let mut holes: Vec<RingItem> = Vec::new();
        for item in clipped_items {
            if dense_raster_fan && item.area_abs < 5e-8 {
                continue;
            }
            if item.is_outer {
                outers.push(item);
            } else {
                holes.push(item);
            }
        }
        if dense_raster_fan {
            outers.sort_by(|a, b| b.area_abs.partial_cmp(&a.area_abs).unwrap_or(std::cmp::Ordering::Equal));
            holes.sort_by(|a, b| b.area_abs.partial_cmp(&a.area_abs).unwrap_or(std::cmp::Ordering::Equal));
            outers.truncate(64);
            holes.truncate(96);
        }

        // Simplify outers & convert to RFC 7946 CCW rings + (lat, lon) containment rings sorted smallest-first.
        let mut outer_polys: Vec<(f64, Vec<(f64, f64)>, Vec<Vec<Value>>)> = Vec::new();
        for o in outers {
            let Some(mut ring) = simplify_and_round_ring(&o.pts, tol_deg, cos_lat, max_pts_per_part) else {
                continue;
            };
            if shoelace_area_xy(&ring) < 0.0 {
                ring.reverse();
            }
            let lat_lon: Vec<(f64, f64)> = ring.iter().map(|&(x, y)| (y, x)).collect();
            let mut gj_ring: Vec<Value> = ring.iter().map(|&(x, y)| json!([x, y])).collect();
            gj_ring.push(gj_ring[0].clone());
            outer_polys.push((o.area_abs, lat_lon, vec![gj_ring]));
        }
        if outer_polys.is_empty() {
            return None;
        }
        // Sort ascending by area so each hole attaches to its tightest containing outer polygon.
        outer_polys.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

        for h in holes {
            let Some(mut h_ring) = simplify_and_round_ring(&h.pts, tol_deg, cos_lat, max_pts_per_part) else {
                continue;
            };
            if shoelace_area_xy(&h_ring) > 0.0 {
                h_ring.reverse();
            }
            let probe = h_ring
                .iter()
                .find(|&&(x, y)| x > min_x + 1e-6 && x < max_x - 1e-6 && y > min_y + 1e-6 && y < max_y - 1e-6)
                .copied()
                .unwrap_or(h_ring[0]);
            if let Some((_, _, rings_out)) = outer_polys.iter_mut().find(|(_, lat_lon, _)| point_in_ring(probe.1, probe.0, lat_lon)) {
                let mut gj_hole: Vec<Value> = h_ring.iter().map(|&(x, y)| json!([x, y])).collect();
                gj_hole.push(gj_hole[0].clone());
                rings_out.push(gj_hole);
            }
        }

        let mut poly_vals: Vec<Vec<Vec<Value>>> = outer_polys.into_iter().map(|(_, _, r)| r).collect();
        return if poly_vals.len() == 1 {
            Some(json!({ "type": "Polygon", "coordinates": poly_vals.remove(0) }))
        } else {
            Some(json!({ "type": "MultiPolygon", "coordinates": poly_vals }))
        };
    }

    if let Some(paths_arr) = geom.get("paths").and_then(|v| v.as_array()) {
        let mut lines_out: Vec<Vec<Value>> = Vec::new();
        for path in paths_arr {
            let Some(pts_arr) = path.as_array() else { continue };
            let xy: Vec<(f64, f64)> = pts_arr
                .iter()
                .filter_map(|p| Some((p.get(0)?.as_f64()?, p.get(1)?.as_f64()?)))
                .filter(|(x, y)| x.is_finite() && y.is_finite())
                .collect();
            if xy.len() < 2 {
                continue;
            }
            for sub in clip_polyline_liang_barsky(&xy, min_x, max_x, min_y, max_y) {
                let mut tol = tol_deg;
                let mut simp = dp_simplify_open(&sub, tol, cos_lat);
                if max_pts_per_part >= 4 {
                    for _ in 0..8 {
                        if simp.len() <= max_pts_per_part {
                            break;
                        }
                        tol *= 1.5;
                        let next = dp_simplify_open(&sub, tol, cos_lat);
                        if next.len() < 2 {
                            break;
                        }
                        simp = next;
                    }
                }
                let mut rounded: Vec<Value> = Vec::with_capacity(simp.len());
                for (x, y) in simp {
                    let rx = round_n(x, 5);
                    let ry = round_n(y, 5);
                    let dup = rounded.last().map(|v| v[0].as_f64() == Some(rx) && v[1].as_f64() == Some(ry)).unwrap_or(false);
                    if !dup {
                        rounded.push(json!([rx, ry]));
                    }
                }
                if rounded.len() >= 2 {
                    lines_out.push(rounded);
                }
            }
        }
        if lines_out.is_empty() {
            return None;
        }
        return Some(json!({ "type": "MultiLineString", "coordinates": lines_out }));
    }

    None
}

const HAZARD_TILE_TTL_SECS: u64 = 30 * 86_400;
const TRACT_INDICATORS_TTL_SECS: u64 = 90 * 86_400;

fn digits_only(raw: &str) -> String {
    raw.chars().filter(|c| c.is_ascii_digit()).collect()
}

fn valid_tract11(raw: Option<&str>) -> Option<String> {
    raw.map(digits_only)
        .filter(|s| s.len() == 11 && s.chars().all(|c| c.is_ascii_digit()))
}

fn geo_str(geo: &Value, key: &str) -> Option<String> {
    geo.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn extract_tract11_from_geo(geo: &Value) -> Option<String> {
    if let Some(t) = valid_tract11(
        geo_str(geo, "tractGeoid")
            .or_else(|| geo_str(geo, "censusTract"))
            .as_deref(),
    ) {
        return Some(t);
    }
    let raw_tract = digits_only(geo_str(geo, "tract").as_deref().unwrap_or(""));
    if raw_tract.len() == 11 {
        return Some(raw_tract);
    }
    let county_fips = digits_only(geo_str(geo, "countyFips").as_deref().unwrap_or(""));
    if raw_tract.len() == 6 && county_fips.len() == 5 {
        return Some(format!("{county_fips}{raw_tract}"));
    }
    None
}

fn geohash5_geo_index(
    lat: f64,
    lon: f64,
    gh5: &str,
    geo: &Value,
) -> crate::db::shared_cache_store::GeoIndexColumns {
    let bbox = crate::extensions::shared_cache::decode_geohash_bbox(gh5);
    crate::db::shared_cache_store::GeoIndexColumns {
        geohash5: Some(gh5.to_string()),
        geohash6: crate::extensions::shared_cache::encode_geohash(lat, lon, 6),
        geohash7: crate::extensions::shared_cache::encode_geohash(lat, lon, 7),
        tract: extract_tract11_from_geo(geo),
        zip: geo_str(geo, "zip").or_else(|| geo_str(geo, "postalCode")),
        county: geo_str(geo, "county"),
        state: geo_str(geo, "state"),
        bbox_south: bbox.map(|(s, _, _, _)| s),
        bbox_west: bbox.map(|(_, w, _, _)| w),
        bbox_north: bbox.map(|(_, _, n, _)| n),
        bbox_east: bbox.map(|(_, _, _, e)| e),
    }
}

fn tract_geo_index(
    lat: f64,
    lon: f64,
    tract11: &str,
    geo: &Value,
) -> crate::db::shared_cache_store::GeoIndexColumns {
    crate::db::shared_cache_store::GeoIndexColumns {
        geohash5: crate::extensions::shared_cache::encode_geohash(lat, lon, 5),
        geohash6: crate::extensions::shared_cache::encode_geohash(lat, lon, 6),
        geohash7: crate::extensions::shared_cache::encode_geohash(lat, lon, 7),
        tract: Some(tract11.to_string()),
        zip: geo_str(geo, "zip").or_else(|| geo_str(geo, "postalCode")),
        county: geo_str(geo, "county"),
        state: geo_str(geo, "state"),
        ..Default::default()
    }
}

fn geohash_proto_bbox(
    gh5: &str,
) -> Option<crate::extensions::proto::shared_v1::GeoBoundingBoxProto> {
    crate::extensions::shared_cache::decode_geohash_bbox(gh5).map(|(s, w, n, e)| {
        crate::extensions::proto::shared_v1::GeoBoundingBoxProto {
            south: s,
            west: w,
            north: n,
            east: e,
        }
    })
}

fn feature_bbox_proto(
    geom: &Value,
) -> Option<crate::extensions::proto::shared_v1::GeoBoundingBoxProto> {
    let mut south = f64::INFINITY;
    let mut west = f64::INFINITY;
    let mut north = f64::NEG_INFINITY;
    let mut east = f64::NEG_INFINITY;
    let mut any = false;
    for list in parse_coord_lists(geom.get("rings").or_else(|| geom.get("paths"))) {
        for (lat, lon) in list {
            if lat.is_finite() && lon.is_finite() {
                south = south.min(lat);
                north = north.max(lat);
                west = west.min(lon);
                east = east.max(lon);
                any = true;
            }
        }
    }
    any.then_some(crate::extensions::proto::shared_v1::GeoBoundingBoxProto {
        south,
        west,
        north,
        east,
    })
}

fn stringify_attributes(attrs: Option<&Value>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Some(obj) = attrs.and_then(Value::as_object) else {
        return out;
    };
    for (k, v) in obj {
        match v {
            Value::Null => {}
            Value::String(s) if !s.trim().is_empty() => {
                out.insert(k.clone(), s.trim().to_string());
            }
            Value::Number(n) => {
                out.insert(k.clone(), n.to_string());
            }
            Value::Bool(b) => {
                out.insert(k.clone(), b.to_string());
            }
            _ => {}
        }
    }
    out
}

async fn get_cached_hazard_polygon_tile(
    http: &HttpClient,
    db: Option<&crate::db::DbStore>,
    gh5: &str,
    layer: &str,
) -> Option<(Vec<Value>, String, String)> {
    if http.is_bypassing_cache_reads() {
        return None;
    }
    let db = db?;
    let (row, tile) = crate::extensions::shared_cache::get_shared_dataset_proto::<
        crate::extensions::proto::shared_v1::HazardPolygonTileProto,
    >(
        Some(crate::extensions::shared_cache::SharedExtensionCache::global()),
        db,
        crate::extensions::shared_cache::dataset_ids::HAZARD_POLYGON_TILE,
        crate::extensions::shared_cache::ShareScope::Geohash5,
        gh5,
        layer,
    )
    .await
    .ok()??;
    let raw_json = row.payload_json.as_deref()?;
    let payload: Value = serde_json::from_str(raw_json).ok()?;
    let feats = payload.get("features")?.as_array()?.clone();
    let src_name = payload
        .get("sourceName")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(tile.source_name.as_str())
        .to_string();
    let src_url = payload
        .get("sourceUrl")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(tile.source_url.as_str())
        .to_string();
    Some((feats, src_name, src_url))
}

async fn put_cached_hazard_polygon_tile(
    db: Option<&crate::db::DbStore>,
    lat: f64,
    lon: f64,
    gh5: &str,
    layer: &str,
    geo: &Value,
    feats: &[Value],
    source_name: &str,
    source_url: &str,
    radius_m: f64,
) {
    let Some(db) = db else {
        return;
    };
    if feats.len() >= 900 {
        return;
    }
    let (clat, clon) = crate::extensions::shared_cache::decode_geohash_center(gh5).unwrap_or((lat, lon));
    let mut proto_feats = Vec::with_capacity(feats.len().min(200));
    for f in feats.iter().take(200) {
        let a = f.get("attributes");
        let geom = f.get("geometry");
        let zone_code = a
            .and_then(|x| {
                val_str(x.get("FLD_ZONE"))
                    .or_else(|| val_str(x.get("HAZ_CLASS")))
                    .or_else(|| val_str(x.get("FHSZ_Description")))
                    .or_else(|| val_str(x.get("FHSZ")))
            })
            .unwrap_or("")
            .to_string();
        let label = a
            .and_then(|x| {
                val_str(x.get("ZONE_SUBTY"))
                    .or_else(|| val_str(x.get("HAZ_NAME")))
                    .or_else(|| val_str(x.get("DAM_NAME")))
                    .or_else(|| val_str(x.get("SRA")))
            })
            .unwrap_or(zone_code.as_str())
            .to_string();
        let high_hazard = a
            .and_then(|x| val_str(x.get("SFHA_TF")))
            .map(|s| s.eq_ignore_ascii_case("T"))
            .unwrap_or_else(|| !zone_code.is_empty());
        let geometry_geojson = geom
            .and_then(|g| clip_and_simplify_arcgis_geometry(g, clat, clon, radius_m, 0.00018, 120))
            .map(|v| v.to_string())
            .unwrap_or_default();
        proto_feats.push(crate::extensions::proto::shared_v1::HazardPolygonFeatureProto {
            layer: layer.to_string(),
            zone_code,
            label,
            high_hazard,
            bbox: geom.and_then(feature_bbox_proto),
            geometry_geojson,
            attributes: stringify_attributes(a),
        });
    }
    let tile = crate::extensions::proto::shared_v1::HazardPolygonTileProto {
        tile_key: gh5.to_string(),
        layer: layer.to_string(),
        bbox: geohash_proto_bbox(gh5),
        features: proto_feats,
        blob_hash: None,
        source_name: source_name.to_string(),
        source_url: source_url.to_string(),
        fetched_at_rfc3339: crate::shared::now_iso(),
    };
    let payload_json = serde_json::to_string(&json!({
        "sourceName": source_name,
        "sourceUrl": source_url,
        "features": feats,
    }))
    .ok();
    let provenance_json = serde_json::to_string(&json!({
        "dataset": crate::extensions::shared_cache::dataset_ids::HAZARD_POLYGON_TILE,
        "layer": layer,
        "scope": "geohash5",
        "scopeKey": gh5,
        "sourceName": source_name,
        "sourceUrl": source_url,
    }))
    .ok();
    let geo_cols = geohash5_geo_index(lat, lon, gh5, geo);
    if let Err(err) = crate::extensions::shared_cache::put_shared_dataset_proto(
        Some(crate::extensions::shared_cache::SharedExtensionCache::global()),
        db,
        crate::extensions::shared_cache::dataset_ids::HAZARD_POLYGON_TILE,
        crate::extensions::shared_cache::ShareScope::Geohash5,
        gh5,
        layer,
        geo_cols,
        &tile,
        payload_json,
        provenance_json,
        None,
        HAZARD_TILE_TTL_SECS,
    )
    .await
    {
        tracing::warn!(geohash5 = gh5, layer = layer, error = %err, "hazards: failed to persist HAZARD_POLYGON_TILE");
    }
}

async fn arcgis_radius_tile_cached(
    http: &HttpClient,
    db: Option<&crate::db::DbStore>,
    lat: f64,
    lon: f64,
    geo: &Value,
    layer: &str,
    url: &str,
    out_fields: &str,
    parcel_radius_m: f64,
    extra: &[(&str, String)],
    source_name: &str,
) -> Result<Vec<Value>> {
    let filter_parcel_polygons = |feats: Vec<Value>| -> Vec<Value> {
        feats
            .into_iter()
            .filter(|f| {
                f.get("geometry")
                    .and_then(|g| polygon_distance(lat, lon, g))
                    .map(|(dm, _, _)| dm <= parcel_radius_m)
                    .unwrap_or(true)
            })
            .collect()
    };
    let gh5 = crate::extensions::shared_cache::encode_geohash(lat, lon, 5);
    if let Some(ref gh5) = gh5 {
        if let Some((feats, _, _)) = get_cached_hazard_polygon_tile(http, db, gh5, layer).await {
            return Ok(filter_parcel_polygons(feats));
        }
    }
    if db.is_some() && !http.is_offline_external_blocked() {
        if let Some(ref gh5) = gh5 {
            if let Some((clat, clon)) = crate::extensions::shared_cache::decode_geohash_center(gh5) {
                let tile_radius_m = parcel_radius_m + 3_500.0;
                let feats = http
                    .arcgis_radius_query(url, clat, clon, tile_radius_m, out_fields, extra, REQ_TIMEOUT_MS)
                    .await?;
                put_cached_hazard_polygon_tile(
                    db,
                    lat,
                    lon,
                    gh5,
                    layer,
                    geo,
                    &feats,
                    source_name,
                    url,
                    tile_radius_m,
                )
                .await;
                return Ok(filter_parcel_polygons(feats));
            }
        }
    }
    http.arcgis_radius_query(url, lat, lon, parcel_radius_m, out_fields, extra, REQ_TIMEOUT_MS)
        .await
}

const FAULT_TILE_PARAMS_HASH: &str = "usgs_qfaults_33km";

async fn q_faults_tile_cached(
    http: &HttpClient,
    db: Option<&crate::db::DbStore>,
    lat: f64,
    lon: f64,
    geo: &Value,
    out_fields: &str,
    extra: &[(&str, String)],
) -> Result<Vec<Value>> {
    let filter_parcel_radius = |feats: Vec<Value>| -> Vec<Value> {
        feats
            .into_iter()
            .filter(|f| {
                f.get("geometry")
                    .and_then(|g| paths_distance(lat, lon, &parse_coord_lists(g.get("paths"))))
                    .map(|(dm, _, _)| dm <= 30_000.0)
                    .unwrap_or(false)
            })
            .collect()
    };
    let gh5 = crate::extensions::shared_cache::encode_geohash(lat, lon, 5);
    if !http.is_bypassing_cache_reads() {
        if let (Some(db), Some(ref gh5)) = (db, &gh5) {
            if let Ok(Some((row, _tile))) = crate::extensions::shared_cache::get_shared_dataset_proto::<
                crate::extensions::proto::shared_v1::FaultLinesTileProto,
            >(
                Some(crate::extensions::shared_cache::SharedExtensionCache::global()),
                db,
                crate::extensions::shared_cache::dataset_ids::FAULT_LINES,
                crate::extensions::shared_cache::ShareScope::Geohash5,
                gh5,
                FAULT_TILE_PARAMS_HASH,
            )
            .await
            {
                if let Some(feats) = row
                    .payload_json
                    .as_deref()
                    .and_then(|s| serde_json::from_str::<Vec<Value>>(s).ok())
                {
                    return Ok(filter_parcel_radius(feats));
                }
            }
        }
    }

    if let (Some(db), Some(ref gh5)) = (db, &gh5) {
        if !http.is_offline_external_blocked() {
            if let Some((clat, clon)) = crate::extensions::shared_cache::decode_geohash_center(gh5) {
                let feats = http
                    .arcgis_radius_query(URL_QFAULTS, clat, clon, 33_500.0, out_fields, extra, REQ_TIMEOUT_MS)
                    .await?;
                if feats.len() < 1990 {
                    let mut proto_faults = Vec::with_capacity(feats.len().min(200));
                    for f in feats.iter().take(200) {
                        let Some(a) = f.get("attributes") else { continue };
                        let age = val_str(a.get("age")).unwrap_or("");
                        proto_faults.push(crate::extensions::proto::shared_v1::FaultLineFeatureProto {
                            fault_id: val_str(a.get("fault_id")).unwrap_or("").to_string(),
                            fault_name: val_str(a.get("fault_name")).unwrap_or("Unnamed fault").to_string(),
                            slip_rate: val_str(a.get("slip_rate")).unwrap_or("").to_string(),
                            age_class: age.to_string(),
                            alquist_priolo: fault_age_rank(age) >= 4,
                            geometry_geojson: f
                                .get("geometry")
                                .and_then(|g| clip_and_simplify_arcgis_geometry(g, clat, clon, 33_500.0, 0.00025, 120))
                                .map(|v| v.to_string())
                                .unwrap_or_default(),
                        });
                    }
                    let tile = crate::extensions::proto::shared_v1::FaultLinesTileProto {
                        tile_key: gh5.clone(),
                        bbox: geohash_proto_bbox(gh5),
                        faults: proto_faults,
                        source_url: URL_QFAULTS.to_string(),
                        fetched_at_rfc3339: crate::shared::now_iso(),
                    };
                    let payload_json = serde_json::to_string(&feats).ok();
                    let provenance_json = serde_json::to_string(&json!({
                        "dataset": crate::extensions::shared_cache::dataset_ids::FAULT_LINES,
                        "scope": "geohash5",
                        "scopeKey": gh5,
                        "sourceUrl": URL_QFAULTS,
                    }))
                    .ok();
                    let geo_cols = geohash5_geo_index(lat, lon, gh5, geo);
                    if let Err(err) = crate::extensions::shared_cache::put_shared_dataset_proto(
                        Some(crate::extensions::shared_cache::SharedExtensionCache::global()),
                        db,
                        crate::extensions::shared_cache::dataset_ids::FAULT_LINES,
                        crate::extensions::shared_cache::ShareScope::Geohash5,
                        gh5,
                        FAULT_TILE_PARAMS_HASH,
                        geo_cols,
                        &tile,
                        payload_json,
                        provenance_json,
                        None,
                        HAZARD_TILE_TTL_SECS,
                    )
                    .await
                    {
                        tracing::warn!(geohash5 = %gh5, error = %err, "hazards: failed to persist FAULT_LINES");
                    }
                }
                return Ok(filter_parcel_radius(feats));
            }
        }
    }

    http.arcgis_radius_query(URL_QFAULTS, lat, lon, 30_000.0, out_fields, extra, REQ_TIMEOUT_MS)
        .await
}

async fn q_nri_with_db(
    http: &HttpClient,
    db: Option<&crate::db::DbStore>,
    lat: f64,
    lon: f64,
    tract11: Option<&str>,
    geo: &Value,
) -> Result<Option<Value>> {
    let valid_hint = valid_tract11(tract11);
    if !http.is_bypassing_cache_reads() {
        if let (Some(db), Some(ref t)) = (db, &valid_hint) {
            if let Ok(Some((row, _proto))) = crate::extensions::shared_cache::get_shared_dataset_proto::<
                crate::extensions::proto::shared_v1::TractIndicatorsProto,
            >(
                Some(crate::extensions::shared_cache::SharedExtensionCache::global()),
                db,
                crate::extensions::shared_cache::dataset_ids::TRACT_INDICATORS,
                crate::extensions::shared_cache::ShareScope::CensusTract,
                t,
                "fema_nri",
            )
            .await
            {
                if let Some(v) = row
                    .payload_json
                    .as_deref()
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                    .filter(|v| v.is_object())
                {
                    return Ok(Some(v));
                }
            }
        }
    }

    let attr_opt = q_nri(http, lat, lon, tract11).await?;
    if let (Some(db), Some(ref attr)) = (db, &attr_opt) {
        let tract_key = valid_tract11(val_str(attr.get("TRACTFIPS"))).or(valid_hint);
        if let Some(t11) = tract_key {
            let mut nri_hazard_scores = HashMap::new();
            for &(code, _) in NRI_HAZ {
                if let Some(sc) = val_num(attr.get(&format!("{code}_RISKS"))) {
                    nri_hazard_scores.insert(code.to_string(), sc);
                }
            }
            let proto = crate::extensions::proto::shared_v1::TractIndicatorsProto {
                tract_geoid: t11.clone(),
                state_fips: t11.get(..2).unwrap_or("").to_string(),
                county_fips: t11.get(..5).unwrap_or("").to_string(),
                nri_composite_score: val_num(attr.get("RISK_SCORE")),
                nri_composite_rating: val_str(attr.get("RISK_RATNG")).map(str::to_string),
                nri_hazard_scores,
                vintage: val_str(attr.get("NRI_VER"))
                    .unwrap_or("FEMA NRI")
                    .to_string(),
                fetched_at_rfc3339: crate::shared::now_iso(),
                ..Default::default()
            };
            let payload_json = serde_json::to_string(attr).ok();
            let provenance_json = serde_json::to_string(&json!({
                "dataset": crate::extensions::shared_cache::dataset_ids::TRACT_INDICATORS,
                "scope": "census_tract",
                "scopeKey": t11,
                "sourceName": "FEMA National Risk Index",
                "sourceUrl": URL_NRI,
            }))
            .ok();
            let geo_cols = tract_geo_index(lat, lon, &t11, geo);
            if let Err(err) = crate::extensions::shared_cache::put_shared_dataset_proto(
                Some(crate::extensions::shared_cache::SharedExtensionCache::global()),
                db,
                crate::extensions::shared_cache::dataset_ids::TRACT_INDICATORS,
                crate::extensions::shared_cache::ShareScope::CensusTract,
                &t11,
                "fema_nri",
                geo_cols,
                &proto,
                payload_json,
                provenance_json,
                None,
                TRACT_INDICATORS_TTL_SECS,
            )
            .await
            {
                tracing::warn!(tract = %t11, error = %err, "hazards: failed to persist TRACT_INDICATORS (fema_nri)");
            }
        }
    }
    Ok(attr_opt)
}

async fn q_nri(http: &HttpClient, lat: f64, lon: f64, tract11: Option<&str>) -> Result<Option<Value>> {
    let mut flds = vec![
        "NRI_ID".to_string(), "TRACTFIPS".to_string(), "COUNTY".to_string(), "STATEABBRV".to_string(),
        "POPULATION".to_string(), "BUILDVALUE".to_string(), "RISK_SCORE".to_string(), "RISK_RATNG".to_string(),
        "RISK_SPCTL".to_string(), "EAL_SCORE".to_string(), "EAL_RATNG".to_string(), "EAL_VALT".to_string(),
        "SOVI_SCORE".to_string(), "SOVI_RATNG".to_string(), "RESL_SCORE".to_string(), "RESL_RATNG".to_string(),
        "NRI_VER".to_string(),
    ];
    for &(code, _) in NRI_HAZ {
        flds.push(format!("{code}_RISKS"));
        flds.push(format!("{code}_RISKR"));
        flds.push(format!("{code}_EALT"));
    }
    let out_fields = flds.join(",");
    let mut rows = http.arcgis_point_query(URL_NRI, lat, lon, &out_fields, &[], REQ_TIMEOUT_MS).await.unwrap_or_default();
    if rows.is_empty() {
        if let Some(t) = tract11.filter(|t| t.len() == 11 && t.chars().all(|c| c.is_ascii_digit())) {
            let j = http.arcgis_query(
                URL_NRI,
                &[("where", format!("TRACTFIPS='{t}'")), ("outFields", out_fields), ("returnGeometry", "false".into())],
                REQ_TIMEOUT_MS,
            ).await?;
            rows = j.get("features").and_then(|f| f.as_array())
                .map(|a| a.iter().filter_map(|x| x.get("attributes").cloned()).collect())
                .unwrap_or_default();
        }
    }
    Ok(rows.into_iter().next())
}

async fn q_asce(http: &HttpClient, lat: f64, lon: f64) -> Result<Value> {
    for sc in ["D", "Default"] {
        let u = Url::parse_with_params(
            URL_ASCE722,
            &[
                ("latitude", format!("{:.5}", lat)),
                ("longitude", format!("{:.5}", lon)),
                ("riskCategory", "II".into()),
                ("siteClass", sc.into()),
                ("title", "Open Estate".into()),
            ],
        )?;
        if let Ok(j) = http.get_json_opts(u.as_str(), REQ_TIMEOUT_MS, 1, TTL_7D, None).await {
            if j.pointer("/response/data").is_some() {
                return Ok(j);
            }
        }
    }
    Err(anyhow!("USGS ASCE 7-22 returned no design data"))
}

async fn q_comcat_count(http: &HttpClient, lat: f64, lon: f64) -> Result<i64> {
    let lat_s = format!("{:.5}", lat);
    let lon_s = format!("{:.5}", lon);
    let u = Url::parse_with_params(
        URL_COMCAT_COUNT,
        &[
            ("format", "geojson"),
            ("latitude", lat_s.as_str()),
            ("longitude", lon_s.as_str()),
            ("starttime", "1900-01-01"),
            ("eventtype", "earthquake"),
            ("maxradiuskm", "50"),
            ("minmagnitude", "4.0"),
        ],
    )?;
    let j = http.get_json_opts(u.as_str(), REQ_TIMEOUT_MS, 1, TTL_7D, None).await?;
    j.get("count").and_then(|v| v.as_i64()).ok_or_else(|| anyhow!("No ComCat count"))
}

async fn q_comcat_list(http: &HttpClient, lat: f64, lon: f64, radius_km: u32, min_mag: &str, limit: u32) -> Result<Vec<Value>> {
    let lat_s = format!("{:.5}", lat);
    let lon_s = format!("{:.5}", lon);
    let rad_s = radius_km.to_string();
    let lim_s = limit.to_string();
    let u = Url::parse_with_params(
        URL_COMCAT_QUERY,
        &[
            ("format", "geojson"),
            ("latitude", lat_s.as_str()),
            ("longitude", lon_s.as_str()),
            ("starttime", "1900-01-01"),
            ("eventtype", "earthquake"),
            ("maxradiuskm", rad_s.as_str()),
            ("minmagnitude", min_mag),
            ("orderby", "magnitude"),
            ("limit", lim_s.as_str()),
        ],
    )?;
    let j = http.get_json_opts(u.as_str(), REQ_TIMEOUT_MS, 1, TTL_7D, None).await?;
    Ok(j.get("features").and_then(|f| f.as_array()).cloned().unwrap_or_default())
}

async fn q_whp(http: &HttpClient, lat: f64, lon: f64) -> Result<Option<i64>> {
    let geom = json!({ "x": lon, "y": lat, "spatialReference": { "wkid": 4326 } }).to_string();
    let u = Url::parse_with_params(
        &format!("{URL_WHP}/identify"),
        &[
            ("geometry", geom.as_str()),
            ("geometryType", "esriGeometryPoint"),
            ("returnGeometry", "false"),
            ("returnCatalogItems", "false"),
            ("f", "json"),
        ],
    )?;
    let j = http.get_json_opts(u.as_str(), REQ_TIMEOUT_MS, 0, TTL_7D, None).await?;
    Ok(val_num(j.get("value")).map(|n| n.round() as i64))
}

/// Ground elevation at a point, with the dataset that supplied it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Elevation {
    pub meters: f64,
    pub source: &'static str,
    pub source_url: &'static str,
}

/// Elevation services signal "no data" with sentinels such as `-1000000`; accept only values
/// that can be a real ground elevation in the United States.
fn plausible_elevation_m(v: Option<f64>) -> Option<f64> {
    v.filter(|m| m.is_finite() && (-500.0..=7_000.0).contains(m))
}

async fn q_elevation_epqs(http: &HttpClient, lat: f64, lon: f64) -> Result<f64> {
    let lon_s = lon.to_string();
    let lat_s = lat.to_string();
    let u = Url::parse_with_params(
        URL_EPQS,
        &[("x", lon_s.as_str()), ("y", lat_s.as_str()), ("units", "Meters"), ("wkid", "4326"), ("includeDate", "false")],
    )?;
    let j = http.get_json_opts(u.as_str(), ELEV_REQ_TIMEOUT_MS, 0, TTL_7D, None).await?;
    plausible_elevation_m(val_num(j.get("value")))
        .ok_or_else(|| anyhow!("USGS EPQS returned no elevation for this point"))
}

async fn q_elevation_3dep_image(http: &HttpClient, lat: f64, lon: f64) -> Result<f64> {
    let geom = json!({ "x": lon, "y": lat, "spatialReference": { "wkid": 4326 } }).to_string();
    let u = Url::parse_with_params(
        &format!("{URL_3DEP_IMAGE}/getSamples"),
        &[
            ("geometry", geom.as_str()),
            ("geometryType", "esriGeometryPoint"),
            ("returnFirstValueOnly", "true"),
            ("f", "json"),
        ],
    )?;
    let j = http.get_json_opts(u.as_str(), ELEV_REQ_TIMEOUT_MS, 0, TTL_7D, None).await?;
    plausible_elevation_m(val_num(j.pointer("/samples/0/value")))
        .ok_or_else(|| anyhow!("USGS 3DEP ImageServer returned no elevation for this point"))
}

async fn q_elevation_copernicus(http: &HttpClient, lat: f64, lon: f64) -> Result<f64> {
    let u = format!("{URL_OPEN_METEO_ELEVATION}?latitude={lat:.6}&longitude={lon:.6}");
    let j = http.get_json_opts(&u, ELEV_REQ_TIMEOUT_MS, 0, TTL_7D, None).await?;
    plausible_elevation_m(val_num(j.pointer("/elevation/0")))
        .ok_or_else(|| anyhow!("Open-Meteo returned no elevation for this point"))
}

/// Resolves the site elevation from the first source to answer in preference order:
/// USGS 3DEP (EPQS, hedged with the ImageServer behind it) within `usgs_budget`, otherwise the
/// Copernicus GLO-90 DEM, which is queried concurrently from the start. The whole resolution is
/// capped at 1.5 x `usgs_budget`, so no combination of slow services can stall the hazards
/// module. The three lookups are passed in so the policy is testable offline.
pub async fn resolve_elevation<A, B, C>(
    epqs: A,
    image_server: B,
    copernicus: C,
    usgs_budget: std::time::Duration,
    hedge_delay: std::time::Duration,
) -> Result<Elevation>
where
    A: std::future::Future<Output = Result<f64>>,
    B: std::future::Future<Output = Result<f64>>,
    C: std::future::Future<Output = Result<f64>>,
{
    let usgs = async {
        let hedged = async {
            tokio::time::sleep(hedge_delay).await;
            image_server.await
        };
        tokio::pin!(epqs);
        tokio::pin!(hedged);
        let (mut epqs_done, mut hedged_done) = (false, false);
        let mut last_err = anyhow!("USGS 3DEP elevation unavailable");
        loop {
            tokio::select! {
                r = &mut epqs, if !epqs_done => {
                    epqs_done = true;
                    match r {
                        Ok(m) => return Ok(Elevation { meters: m, source: ELEV_SOURCE_EPQS, source_url: URL_EPQS }),
                        Err(e) => last_err = e,
                    }
                }
                r = &mut hedged, if !hedged_done => {
                    hedged_done = true;
                    match r {
                        Ok(m) => return Ok(Elevation { meters: m, source: ELEV_SOURCE_3DEP_IMAGE, source_url: URL_3DEP_IMAGE }),
                        Err(e) => last_err = e,
                    }
                }
                else => return Err(last_err),
            }
        }
    };
    let overall_deadline = tokio::time::Instant::now() + usgs_budget + usgs_budget / 2;
    tokio::pin!(usgs);
    tokio::pin!(copernicus);
    let deadline = tokio::time::sleep(usgs_budget);
    tokio::pin!(deadline);
    let mut backup: Option<Result<f64>> = None;
    let usgs_err = loop {
        tokio::select! {
            biased;
            r = &mut usgs => match r {
                Ok(el) => return Ok(el),
                Err(e) => break e,
            },
            r = &mut copernicus, if backup.is_none() => backup = Some(r),
            _ = &mut deadline => {
                break anyhow!(
                    "USGS 3DEP elevation services did not respond within {:.1}s",
                    usgs_budget.as_secs_f64()
                );
            }
        }
    };
    let backup = match backup {
        Some(done) => done,
        None => tokio::time::timeout_at(overall_deadline, copernicus)
            .await
            .unwrap_or_else(|_| Err(anyhow!("Copernicus DEM elevation service did not respond in time"))),
    };
    match backup {
        Ok(m) => Ok(Elevation {
            meters: m,
            source: ELEV_SOURCE_COPERNICUS,
            source_url: "https://open-meteo.com/en/docs/elevation-api",
        }),
        Err(backup_err) => Err(anyhow!("{usgs_err}; {backup_err}")),
    }
}

async fn q_elevation(http: &HttpClient, lat: f64, lon: f64) -> Result<Elevation> {
    // Boxed so three HTTP futures do not enlarge the (already large) hazards fan-out future.
    Box::pin(resolve_elevation(
        q_elevation_epqs(http, lat, lon),
        q_elevation_3dep_image(http, lat, lon),
        q_elevation_copernicus(http, lat, lon),
        ELEV_USGS_BUDGET,
        ELEV_HEDGE_DELAY,
    ))
    .await
}

/// Endpoint configuration and retry/hedge budgets for FEMA NFHL flood lookups.
#[derive(Debug, Clone)]
pub struct NfhlEndpoints {
    pub primary_zones: String,
    pub primary_panels: String,
    pub msc_zones: String,
    pub msc_panels: String,
    pub living_atlas_zones: Option<String>,
    pub primary_budget: std::time::Duration,
    pub hedge_delay: std::time::Duration,
    pub total_budget: std::time::Duration,
    pub req_timeout_ms: u64,
    pub retries: usize,
}

impl Default for NfhlEndpoints {
    fn default() -> Self {
        Self {
            primary_zones: URL_NFHL_ZONES.to_string(),
            primary_panels: URL_NFHL_PANELS.to_string(),
            msc_zones: URL_NFHL_MSC_ZONES.to_string(),
            msc_panels: URL_NFHL_MSC_PANELS.to_string(),
            living_atlas_zones: Some(URL_NFHL_LIVING_ATLAS_ZONES.to_string()),
            primary_budget: NFHL_PRIMARY_BUDGET,
            hedge_delay: NFHL_HEDGE_DELAY,
            total_budget: NFHL_TOTAL_BUDGET,
            req_timeout_ms: NFHL_REQ_TIMEOUT_MS,
            retries: NFHL_RETRIES,
        }
    }
}

/// Source attribution for an NFHL query result, recording the exact endpoint that answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NfhlSourceAttribution {
    pub name: &'static str,
    pub url: String,
}

/// Combined result of the three NFHL flood queries (`point_rows`, `near_features`, `panel_rows`).
pub struct FloodFetch {
    pub point_rows: Result<(Vec<Value>, NfhlSourceAttribution)>,
    pub near_features: Result<(Vec<Value>, NfhlSourceAttribution)>,
    pub panel_rows: Result<(Vec<Value>, NfhlSourceAttribution)>,
}

/// Runs `primary` first, launching `secondary` concurrently after `hedge_delay` (or immediately
/// when `primary` fails). Primary always wins if it succeeds within `primary_budget`; otherwise
/// `secondary` is used up to `total_budget`. Returns `(value, used_secondary)`.
pub async fn resolve_nfhl_with_hedge<T, P, S>(
    primary: P,
    secondary: S,
    primary_budget: std::time::Duration,
    hedge_delay: std::time::Duration,
    total_budget: std::time::Duration,
) -> Result<(T, bool)>
where
    P: std::future::Future<Output = Result<T>>,
    S: std::future::Future<Output = Result<T>>,
{
    let overall_deadline = tokio::time::Instant::now() + total_budget;
    tokio::pin!(primary);
    tokio::pin!(secondary);
    let hedge = tokio::time::sleep(hedge_delay);
    tokio::pin!(hedge);
    let deadline = tokio::time::sleep(primary_budget);
    tokio::pin!(deadline);
    let mut secondary_started = false;
    let mut secondary_result: Option<Result<T>> = None;

    let primary_err = loop {
        tokio::select! {
            biased;
            r = &mut primary => match r {
                Ok(v) => return Ok((v, false)),
                Err(e) => break e,
            },
            _ = &mut hedge, if !secondary_started => {
                secondary_started = true;
            }
            r = &mut secondary, if secondary_started && secondary_result.is_none() => {
                secondary_result = Some(r);
            }
            _ = &mut deadline => {
                break anyhow!(
                    "primary NFHL service did not respond within {:.1}s",
                    primary_budget.as_secs_f64()
                );
            }
        }
    };

    let sec = match secondary_result {
        Some(done) => done,
        None => tokio::time::timeout_at(overall_deadline, secondary)
            .await
            .unwrap_or_else(|_| Err(anyhow!("NFHL mirror service did not respond in time"))),
    };
    match sec {
        Ok(v) => Ok((v, true)),
        Err(sec_err) => Err(anyhow!("{primary_err}; mirror: {sec_err}")),
    }
}

async fn nfhl_arcgis_query(
    http: &HttpClient,
    layer_url: &str,
    params: &[(&str, String)],
    timeout_ms: u64,
    retries: usize,
) -> Result<Vec<Value>> {
    let base = format!("{}/query", layer_url.trim_end_matches('/'));
    let mut parsed = Url::parse(&base)?;
    {
        let mut qp = parsed.query_pairs_mut();
        qp.append_pair("f", "json");
        for (k, v) in params {
            qp.append_pair(k, v);
        }
    }
    let j = http
        .get_json_opts(parsed.as_str(), timeout_ms, retries, TTL_7D, None)
        .await?;
    if let Some(err) = j.get("error") {
        return Err(anyhow!(
            "ArcGIS error {} from {}: {}",
            err.get("code").unwrap_or(&Value::Null),
            url_host_label(layer_url),
            err.get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown")
        ));
    }
    j.get("features")
        .and_then(|f| f.as_array())
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "ArcGIS response from {} missing features array",
                url_host_label(layer_url)
            )
        })
}

async fn nfhl_point_query(
    http: &HttpClient,
    layer_url: &str,
    lat: f64,
    lon: f64,
    out_fields: &str,
    timeout_ms: u64,
    retries: usize,
) -> Result<Vec<Value>> {
    let params = [
        ("geometry", format!("{lon},{lat}")),
        ("geometryType", "esriGeometryPoint".to_string()),
        ("inSR", "4326".to_string()),
        ("spatialRel", "esriSpatialRelIntersects".to_string()),
        ("outFields", out_fields.to_string()),
        ("returnGeometry", "false".to_string()),
    ];
    let feats = nfhl_arcgis_query(http, layer_url, &params, timeout_ms, retries).await?;
    Ok(feats
        .into_iter()
        .filter_map(|f| f.get("attributes").cloned())
        .collect())
}

async fn nfhl_radius_query(
    http: &HttpClient,
    layer_url: &str,
    lat: f64,
    lon: f64,
    meters: f64,
    out_fields: &str,
    extra: &[(&str, String)],
    timeout_ms: u64,
    retries: usize,
) -> Result<Vec<Value>> {
    let mut params = vec![
        ("geometry", format!("{lon},{lat}")),
        ("geometryType", "esriGeometryPoint".to_string()),
        ("inSR", "4326".to_string()),
        ("spatialRel", "esriSpatialRelIntersects".to_string()),
        ("distance", format!("{}", meters.round() as i64)),
        ("units", "esriSRUnit_Meter".to_string()),
        ("outFields", out_fields.to_string()),
        ("returnGeometry", "true".to_string()),
        ("outSR", "4326".to_string()),
    ];
    for (k, v) in extra {
        params.push((*k, v.clone()));
    }
    nfhl_arcgis_query(http, layer_url, &params, timeout_ms, retries).await
}

fn label_fema_res<T>(
    res: Result<(T, bool)>,
    primary_url: &str,
    msc_url: &str,
) -> Result<(T, NfhlSourceAttribution)> {
    res.map(|(val, used_msc)| {
        let attr = if used_msc {
            NfhlSourceAttribution {
                name: NFHL_SOURCE_MSC,
                url: msc_url.to_string(),
            }
        } else {
            NfhlSourceAttribution {
                name: NFHL_SOURCE_PRIMARY,
                url: primary_url.to_string(),
            }
        };
        (val, attr)
    })
}

/// Fetches FEMA NFHL flood zone (point), nearby SFHA polygons (2 km), and FIRM panel info
/// using the default official FEMA primary and mirror endpoints and optional shared SQLite DB.
pub async fn fetch_flood_with_db(
    http: &HttpClient,
    db: Option<&crate::db::DbStore>,
    lat: f64,
    lon: f64,
    geo: &Value,
) -> FloodFetch {
    fetch_flood_with_endpoints_and_db(http, db, lat, lon, geo, &NfhlEndpoints::default()).await
}

/// Fetches FEMA NFHL flood zone (point), nearby SFHA polygons (2 km), and FIRM panel info
/// using the default official FEMA primary and mirror endpoints.
pub async fn fetch_flood(http: &HttpClient, lat: f64, lon: f64) -> FloodFetch {
    let hub_db = crate::extensions::ExtensionHub::global().active_db();
    fetch_flood_with_endpoints_and_db(
        http,
        hub_db.as_ref(),
        lat,
        lon,
        &Value::Null,
        &NfhlEndpoints::default(),
    )
    .await
}

/// Fetches FEMA NFHL flood zone, nearby SFHA polygons, and FIRM panel info against `endpoints`,
/// retrying transient failures and failing over from primary (`hazards.fema.gov`) to the FEMA
/// Map Service Center (`msc.fema.gov`) and Esri Living Atlas (`services.arcgis.com`) mirrors.
pub async fn fetch_flood_with_endpoints(
    http: &HttpClient,
    lat: f64,
    lon: f64,
    endpoints: &NfhlEndpoints,
) -> FloodFetch {
    fetch_flood_with_endpoints_and_db(http, None, lat, lon, &Value::Null, endpoints).await
}

async fn fetch_flood_with_endpoints_and_db(
    http: &HttpClient,
    db: Option<&crate::db::DbStore>,
    lat: f64,
    lon: f64,
    geo: &Value,
    endpoints: &NfhlEndpoints,
) -> FloodFetch {
    let pt_fields = "FLD_ZONE,ZONE_SUBTY,SFHA_TF,STATIC_BFE,DEPTH,DFIRM_ID";
    let near_fields = "FLD_ZONE,ZONE_SUBTY,STATIC_BFE,DEPTH";
    let panel_fields = "FIRM_PAN,EFF_DATE,PANEL_TYP";
    let sfha_extra = [
        ("where", "SFHA_TF = 'T'".to_string()),
        ("maxAllowableOffset", "0.00005".to_string()),
    ];
    let la_timeout = std::time::Duration::from_millis(endpoints.req_timeout_ms + 1_000);

    let pt_fut = Box::pin(async {
        let res = resolve_nfhl_with_hedge(
            nfhl_point_query(
                http,
                &endpoints.primary_zones,
                lat,
                lon,
                pt_fields,
                endpoints.req_timeout_ms,
                endpoints.retries,
            ),
            nfhl_point_query(
                http,
                &endpoints.msc_zones,
                lat,
                lon,
                pt_fields,
                endpoints.req_timeout_ms,
                endpoints.retries,
            ),
            endpoints.primary_budget,
            endpoints.hedge_delay,
            endpoints.total_budget,
        )
        .await;
        match label_fema_res(res, &endpoints.primary_zones, &endpoints.msc_zones) {
            Ok(ok) => Ok(ok),
            Err(fema_err) => {
                if let Some(la_url) = endpoints.living_atlas_zones.as_deref() {
                    let la_res = tokio::time::timeout(
                        la_timeout,
                        nfhl_point_query(
                            http,
                            la_url,
                            lat,
                            lon,
                            pt_fields,
                            endpoints.req_timeout_ms,
                            endpoints.retries,
                        ),
                    )
                    .await;
                    match la_res {
                        // Living Atlas Reduced Set omits minimal Zone X & Zone D polygons, so only
                        // accept a non-empty hit; an empty response must never be presented as X/D.
                        Ok(Ok(rows)) if !rows.is_empty() => {
                            return Ok((
                                rows,
                                NfhlSourceAttribution {
                                    name: NFHL_SOURCE_LIVING_ATLAS,
                                    url: la_url.to_string(),
                                },
                            ));
                        }
                        Ok(Ok(_)) => {
                            tracing::debug!(
                                "hazards: Living Atlas flood layer had no polygon at point; skipping Reduced Set empty result"
                            );
                        }
                        Ok(Err(la_err)) => {
                            return Err(anyhow!("{fema_err}; living atlas: {la_err}"));
                        }
                        Err(_) => {
                            return Err(anyhow!("{fema_err}; living atlas timed out"));
                        }
                    }
                }
                Err(fema_err)
            }
        }
    });

    let near_fut = Box::pin(async {
        let gh5 = crate::extensions::shared_cache::encode_geohash(lat, lon, 5);
        let filter_near_sfha = |feats: Vec<Value>| -> Vec<Value> {
            feats
                .into_iter()
                .filter(|f| {
                    f.get("geometry")
                        .and_then(|g| polygon_distance(lat, lon, g))
                        .map(|(dm, _, _)| dm <= 2_000.0)
                        .unwrap_or(true)
                })
                .collect()
        };
        if let Some(ref gh5) = gh5 {
            if let Some((feats, src_name, src_url)) =
                get_cached_hazard_polygon_tile(http, db, gh5, "fema_nfhl_sfha").await
            {
                let name = if src_name == NFHL_SOURCE_MSC {
                    NFHL_SOURCE_MSC
                } else if src_name == NFHL_SOURCE_LIVING_ATLAS {
                    NFHL_SOURCE_LIVING_ATLAS
                } else {
                    NFHL_SOURCE_PRIMARY
                };
                return Ok((
                    filter_near_sfha(feats),
                    NfhlSourceAttribution {
                        name,
                        url: src_url,
                    },
                ));
            }
        }
        let tile_center = gh5
            .as_deref()
            .and_then(crate::extensions::shared_cache::decode_geohash_center)
            .filter(|_| db.is_some() && !http.is_offline_external_blocked());
        let (q_lat, q_lon, q_rad) = match tile_center {
            Some((clat, clon)) => (clat, clon, 5_500.0),
            None => (lat, lon, 2_000.0),
        };
        let res = resolve_nfhl_with_hedge(
            nfhl_radius_query(
                http,
                &endpoints.primary_zones,
                q_lat,
                q_lon,
                q_rad,
                near_fields,
                &sfha_extra,
                endpoints.req_timeout_ms,
                endpoints.retries,
            ),
            nfhl_radius_query(
                http,
                &endpoints.msc_zones,
                q_lat,
                q_lon,
                q_rad,
                near_fields,
                &sfha_extra,
                endpoints.req_timeout_ms,
                endpoints.retries,
            ),
            endpoints.primary_budget,
            endpoints.hedge_delay,
            endpoints.total_budget,
        )
        .await;
        let out = match label_fema_res(res, &endpoints.primary_zones, &endpoints.msc_zones) {
            Ok(ok) => Ok(ok),
            Err(fema_err) => {
                let mut fb_res = Err(fema_err);
                if let Some(la_url) = endpoints.living_atlas_zones.as_deref() {
                    let la_res = tokio::time::timeout(
                        la_timeout,
                        nfhl_radius_query(
                            http,
                            la_url,
                            q_lat,
                            q_lon,
                            q_rad,
                            near_fields,
                            &sfha_extra,
                            endpoints.req_timeout_ms,
                            endpoints.retries,
                        ),
                    )
                    .await;
                    match la_res {
                        Ok(Ok(feats)) => {
                            fb_res = Ok((
                                feats,
                                NfhlSourceAttribution {
                                    name: NFHL_SOURCE_LIVING_ATLAS,
                                    url: la_url.to_string(),
                                },
                            ));
                        }
                        Ok(Err(la_err)) => {
                            fb_res = Err(anyhow!("{}; living atlas: {la_err}", fb_res.unwrap_err()));
                        }
                        Err(_) => {
                            fb_res = Err(anyhow!("{}; living atlas timed out", fb_res.unwrap_err()));
                        }
                    }
                }
                fb_res
            }
        };
        match out {
            Ok((feats, attr)) => {
                if let (Some(gh5), Some(_)) = (gh5.as_deref(), tile_center) {
                    if feats.len() < 900 && (!feats.is_empty() || attr.name != NFHL_SOURCE_LIVING_ATLAS) {
                        put_cached_hazard_polygon_tile(
                            db,
                            lat,
                            lon,
                            gh5,
                            "fema_nfhl_sfha",
                            geo,
                            &feats,
                            attr.name,
                            &attr.url,
                            5_500.0,
                        )
                        .await;
                    }
                    return Ok((filter_near_sfha(feats), attr));
                }
                Ok((feats, attr))
            }
            Err(e) => Err(e),
        }
    });

    let panel_fut = Box::pin(async {
        let res = resolve_nfhl_with_hedge(
            nfhl_point_query(
                http,
                &endpoints.primary_panels,
                lat,
                lon,
                panel_fields,
                endpoints.req_timeout_ms,
                endpoints.retries,
            ),
            nfhl_point_query(
                http,
                &endpoints.msc_panels,
                lat,
                lon,
                panel_fields,
                endpoints.req_timeout_ms,
                endpoints.retries,
            ),
            endpoints.primary_budget,
            endpoints.hedge_delay,
            endpoints.total_budget,
        )
        .await;
        label_fema_res(res, &endpoints.primary_panels, &endpoints.msc_panels)
    });

    let (point_rows, near_features, panel_rows) = tokio::join!(pt_fut, near_fut, panel_fut);
    FloodFetch {
        point_rows,
        near_features,
        panel_rows,
    }
}

async fn q_mapserver_identify(http: &HttpClient, url_base: &str, lat: f64, lon: f64) -> Result<Vec<Value>> {
    let d = 0.01;
    let geom_s = format!("{lon},{lat}");
    let ext_s = format!("{},{},{},{}", lon - d, lat - d, lon + d, lat + d);
    let u = Url::parse_with_params(
        &format!("{url_base}/identify"),
        &[
            ("geometry", geom_s.as_str()),
            ("geometryType", "esriGeometryPoint"),
            ("sr", "4326"),
            ("tolerance", "0"),
            ("layers", "all"),
            ("mapExtent", ext_s.as_str()),
            ("imageDisplay", "400,400,96"),
            ("returnGeometry", "false"),
            ("f", "json"),
        ],
    )?;
    let j = http.get_json_opts(u.as_str(), REQ_TIMEOUT_MS, 0, TTL_7D, None).await?;
    Ok(j.get("results").and_then(|r| r.as_array()).cloned().unwrap_or_default())
}

fn interp(x: f64, pts: &[(f64, f64)]) -> f64 {
    if x <= pts[0].0 {
        return pts[0].1;
    }
    for i in 1..pts.len() {
        if x <= pts[i].0 {
            let (x0, y0) = pts[i - 1];
            let (x1, y1) = pts[i];
            return y0 + ((y1 - y0) * (x - x0)) / (x1 - x0);
        }
    }
    pts[pts.len() - 1].1
}

fn radon_concern_level(rz: &Value) -> Option<&'static str> {
    let classify = |s: &str| -> Option<&'static str> {
        let l = s.to_ascii_lowercase();
        if l.contains("high") { Some("bad") }
        else if l.contains("moderate") { Some("watch") }
        else if l.contains("low") { Some("good") }
        else { None }
    };
    if let Some(p) = rz.pointer("/california/potential").and_then(|v| v.as_str()).and_then(classify) {
        return Some(p);
    }
    match rz.get("epaZone").and_then(|v| v.as_i64()) {
        Some(1) => return Some("bad"),
        Some(2) => return Some("watch"),
        Some(3) => return Some("good"),
        _ => {}
    }
    rz.pointer("/geologic/potential").and_then(|v| v.as_str()).and_then(classify)
}

/// Compute 0–100 safety sub-scores (`flood`, `earthquake`, `wildfire`, `wind`, `radon`, `other`, `overall`) + `scoreBasis`.
pub fn compute_scores(data: &Value, nri_all: &HashMap<String, Value>, is_ca: bool) -> (Value, Value) {
    let mut b_flood = Vec::new();
    let mut b_quake = Vec::new();
    let mut b_fire = Vec::new();
    let mut b_other = Vec::new();

    let nri_pen = |codes: &[&str], basis: &mut Vec<String>| -> f64 {
        let worst = codes
            .iter()
            .filter_map(|c| nri_all.get(*c))
            .max_by(|x, y| {
                let sx = x.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let sy = y.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
                sx.partial_cmp(&sy).unwrap_or(std::cmp::Ordering::Equal)
            });
        let Some(w) = worst else { return 0.0 };
        let r = w.get("rating").and_then(|v| v.as_str()).unwrap_or("");
        let nm = w.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let p = if r == "Very High" { 10.0 } else if r == "Relatively High" { 5.0 } else { 0.0 };
        if p > 0.0 {
            basis.push(format!("NRI {nm} {r} -{}", p as i64));
        }
        p
    };

    // 1. FLOOD
    let fl = data.get("flood").unwrap_or(&Value::Null);
    let fl_zone = fl.get("zone").and_then(|v| v.as_str()).unwrap_or("");
    let fl_risk = fl.get("risk").and_then(|v| v.as_str()).unwrap_or("");
    let fl_sfha = fl.get("sfha").and_then(|v| v.as_bool()).unwrap_or(false);
    let flood_score: Option<i64> = if !fl_zone.is_empty() || !fl_risk.is_empty() {
        let z = fl_zone.to_uppercase();
        let mut s = if z.starts_with('V') {
            10.0
        } else if z == "AR" || z == "A99" {
            35.0
        } else if fl_sfha {
            25.0
        } else if fl_risk == "moderate" {
            65.0
        } else if fl_risk == "undetermined" {
            70.0
        } else if fl_risk == "minimal" {
            92.0
        } else {
            75.0
        };
        b_flood.push(format!("Zone {} base {}", if fl_zone.is_empty() { "?" } else { fl_zone }, s as i64));
        if !fl_sfha {
            if let Some(dm) = fl.pointer("/nearestSfha/distM").and_then(|v| v.as_f64()) {
                let p = if dm <= 100.0 { 15.0 } else if dm <= 300.0 { 10.0 } else if dm <= 1000.0 { 5.0 } else { 0.0 };
                if p > 0.0 {
                    s -= p;
                    b_flood.push(format!("SFHA {} m away -{}", dm.round() as i64, p as i64));
                }
            }
        }
        s -= nri_pen(&["IFLD", "CFLD"], &mut b_flood);
        if is_ca {
            if data.pointer("/other/damInundation/inArea").and_then(|v| v.as_bool()) == Some(true) {
                s -= 10.0;
                b_flood.push("dam-failure inundation area -10".into());
            }
            if data.pointer("/other/seaLevelRise/exposed").and_then(|v| v.as_bool()) == Some(true) {
                let ft = data.pointer("/other/seaLevelRise/firstExposedAtFt").and_then(|v| v.as_f64()).unwrap_or(6.0);
                let p = if ft <= 3.0 { 10.0 } else { 5.0 };
                s -= p;
                b_flood.push(format!("sea-level-rise exposure -{}", p as i64));
            }
            if data.pointer("/other/leveeProtected").and_then(|v| v.as_bool()) == Some(true) {
                s -= 5.0;
                b_flood.push("levee-protected area -5".into());
            }
        }
        Some(s.clamp(0.0, 100.0).round() as i64)
    } else {
        ["IFLD", "CFLD"]
            .iter()
            .filter_map(|c| nri_all.get(*c))
            .filter(|h| h.get("rating").and_then(|v| v.as_str()).map(is_rated_nri).unwrap_or(false))
            .max_by(|a, b| {
                a.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0)
                    .partial_cmp(&b.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .and_then(|w| {
                let r = w.get("rating").and_then(|v| v.as_str())?;
                let sc = match r {
                    "Very High" => 30, "Relatively High" => 45, "Relatively Moderate" => 60,
                    "Relatively Low" => 75, "Very Low" => 85, _ => return None,
                };
                b_flood.push(format!("no FEMA zone; NRI {} {r} base {sc}", w.get("name").and_then(|v| v.as_str()).unwrap_or("")));
                Some(sc)
            })
    };

    // 2. EARTHQUAKE
    let mut quake_f = data
        .pointer("/earthquake/designMaps/pgaM")
        .or_else(|| data.pointer("/earthquake/designMaps/pga"))
        .and_then(|v| v.as_f64())
        .map(|pga| {
            let q = interp(pga, &[(0.1, 95.0), (0.2, 85.0), (0.4, 70.0), (0.6, 55.0), (0.8, 45.0), (1.2, 30.0)]);
            b_quake.push(format!("PGA_M {pga} g base {}", q.round() as i64));
            q
        })
        .or_else(|| {
            let r = nri_all.get("ERQK")?.get("rating")?.as_str()?;
            let q = match r {
                "Very Low" => 90.0, "Relatively Low" => 80.0, "Relatively Moderate" => 65.0,
                "Relatively High" => 50.0, "Very High" => 35.0, _ => return None,
            };
            b_quake.push(format!("NRI earthquake {r} base {}", q as i64));
            Some(q)
        });
    if let Some(ref mut q) = quake_f {
        if let Some(dkm) = data.pointer("/earthquake/faults/nearestRecentlyActive/distKm").and_then(|v| v.as_f64()) {
            let dm = dkm * 1000.0;
            let p = if dm <= 1000.0 { 15.0 } else if dm <= 3000.0 { 10.0 } else if dm <= 10_000.0 { 5.0 } else { 0.0 };
            if p > 0.0 {
                *q -= p;
                b_quake.push(format!("active fault {dkm} km -{}", p as i64));
            }
        }
        if data.pointer("/earthquake/california/alquistPriolo/inZone").and_then(|v| v.as_bool()) == Some(true) {
            *q -= 20.0;
            b_quake.push("Alquist-Priolo zone -20".into());
        }
        if data.pointer("/earthquake/california/liquefaction/inZone").and_then(|v| v.as_bool()) == Some(true) {
            *q -= 10.0;
            b_quake.push("liquefaction zone -10".into());
        }
        if data.pointer("/earthquake/california/landslide/inZone").and_then(|v| v.as_bool()) == Some(true) {
            *q -= 10.0;
            b_quake.push("EQ-induced landslide zone -10".into());
        }
    }
    let quake_score = quake_f.map(|q| q.clamp(5.0, 100.0).round() as i64);

    // 3. WILDFIRE
    let mut fire_f: Option<f64> = if let Some(cls) = data.pointer("/wildfire/california/fhszClass").and_then(|v| v.as_i64()) {
        let mut f = match cls { 3 => 15.0, 2 => 35.0, 1 => 60.0, _ => 92.0 };
        let lbl = data.pointer("/wildfire/california/fhsz").and_then(|v| v.as_str()).unwrap_or("none");
        b_fire.push(format!("FHSZ {lbl} base {}", f as i64));
        if cls < 2 {
            if let Some(dm) = data.pointer("/wildfire/california/nearestHighOrVeryHigh/distM").and_then(|v| v.as_f64()) {
                let p = if dm <= 300.0 { 15.0 } else if dm <= 1000.0 { 10.0 } else if dm <= 2000.0 { 5.0 } else { 0.0 };
                if p > 0.0 {
                    f -= p;
                    b_fire.push(format!("High/Very High FHSZ {} m away -{}", dm.round() as i64, p as i64));
                }
            }
        }
        Some(f)
    } else if let Some(wc) = data.pointer("/wildfire/whp/class").and_then(|v| v.as_i64()) {
        let f = match wc { 1 => Some(90.0), 2 => Some(80.0), 3 => Some(60.0), 4 => Some(35.0), 5 => Some(15.0), 6 => Some(92.0), 7 => Some(95.0), _ => None };
        if let Some(fv) = f {
            let lbl = data.pointer("/wildfire/whp/label").and_then(|v| v.as_str()).unwrap_or("");
            b_fire.push(format!("USFS WHP {lbl} base {}", fv as i64));
        }
        f
    } else if let Some(r) = data.pointer("/wildfire/nri/rating").and_then(|v| v.as_str()) {
        let fv = match r {
            "Very Low" => 90.0, "Relatively Low" => 80.0, "Relatively Moderate" => 60.0,
            "Relatively High" => 40.0, "Very High" => 20.0, _ => 90.0,
        };
        b_fire.push(format!("NRI wildfire {r} base {}", fv as i64));
        Some(fv)
    } else {
        None
    };
    if let Some(ref mut fv) = fire_f {
        *fv -= nri_pen(&["WFIR"], &mut b_fire);
    }
    let fire_score = fire_f.map(|f| f.clamp(0.0, 100.0).round() as i64);

    // 4. WIND & RADON & OTHER
    let has_nri_data = !nri_all.is_empty() || data.get("nri").map(|v| !v.is_null()).unwrap_or(false);
    let max_wind = ["SWND", "TRND", "HRCN"]
        .iter()
        .filter_map(|c| nri_all.get(*c))
        .filter_map(|h| h.get("score").and_then(|v| v.as_f64()))
        .fold(0.0_f64, f64::max);
    let wind_score: Option<i64> = if max_wind > 0.0 {
        Some((100.0 - 0.65 * max_wind).clamp(10.0, 98.0).round() as i64)
    } else if has_nri_data {
        Some(90)
    } else {
        None
    };

    let rz = data.get("radon").unwrap_or(&Value::Null);
    let rl = radon_concern_level(rz);
    let radon_score: Option<i64> = match rl {
        Some("bad") => Some(45),
        Some("watch") => Some(72),
        Some("good") => Some(92),
        _ => None,
    };

    let mut others: Vec<&Value> = OTHER_HAZ
        .iter()
        .filter_map(|c| nri_all.get(*c))
        .filter(|h| {
            h.get("rating").and_then(|v| v.as_str()).map(is_rated_nri).unwrap_or(false)
                && h.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0) > 0.0
        })
        .collect();
    others.sort_by(|a, b| {
        b.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0)
            .partial_cmp(&a.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut other_f: Option<f64> = if nri_all.contains_key("ERQK") || !others.is_empty() {
        let s1 = others.first().and_then(|h| h.get("score")).and_then(|v| v.as_f64()).unwrap_or(0.0);
        let s2 = others.get(1).and_then(|h| h.get("score")).and_then(|v| v.as_f64()).unwrap_or(0.0);
        let base = 100.0 - 0.5 * s1 - 0.2 * s2;
        let top_lbl: Vec<String> = others.iter().take(2).map(|h| {
            format!("{} {}", h.get("name").and_then(|v| v.as_str()).unwrap_or(""), h.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0))
        }).collect();
        b_other.push(format!("NRI other hazards (top: {}) base {}", if top_lbl.is_empty() { "none".into() } else { top_lbl.join(", ") }, base.round() as i64));
        Some(base)
    } else if rz.get("epaZone").and_then(|v| v.as_i64()).is_some() || rz.get("geologic").filter(|v| !v.is_null()).is_some() {
        Some(90.0)
    } else {
        None
    };
    if let Some(ref mut ov) = other_f {
        let rp = match rl { Some("bad") => 10.0, Some("watch") => 5.0, _ => 0.0 };
        if rp > 0.0 { *ov -= rp; b_other.push(format!("radon -{}", rp as i64)); }
        if data.pointer("/other/tsunamiHazardArea").and_then(|v| v.as_bool()) == Some(true) {
            *ov -= 15.0; b_other.push("tsunami hazard area -15".into());
        }
        if let Some(lsc) = data.pointer("/earthquake/california/landslideSusceptibility/class").and_then(|v| v.as_i64()) {
            if lsc >= 5 {
                let p = if lsc >= 8 { 10.0 } else { 5.0 };
                *ov -= p; b_other.push(format!("landslide susceptibility class {lsc} -{}", p as i64));
            }
        }
    }
    let other_score = other_f.map(|o| o.clamp(0.0, 100.0).round() as i64);

    let subs: Vec<f64> = [flood_score, quake_score, fire_score, other_score]
        .into_iter()
        .flatten()
        .map(|v| v as f64)
        .collect();
    let overall = if subs.is_empty() {
        None
    } else {
        let mean = subs.iter().sum::<f64>() / (subs.len() as f64);
        let min_v = subs.iter().copied().fold(f64::INFINITY, f64::min);
        Some((0.6 * mean + 0.4 * min_v).round() as i64)
    };

    (
        json!({
            "overall": overall,
            "flood": flood_score,
            "earthquake": quake_score,
            "wildfire": fire_score,
            "wind": wind_score,
            "radon": radon_score,
            "other": other_score,
        }),
        json!({
            "flood": b_flood,
            "earthquake": b_quake,
            "wildfire": b_fire,
            "other": b_other,
        }),
    )
}

pub fn build_highlights(data: &Value, is_ca: bool) -> Vec<Value> {
    let mut out = Vec::new();
    let mut add = |level: &str, text: String| out.push(json!({ "level": level, "text": text }));

    if let Some(fl) = data.get("flood") {
        if let Some(z) = fl.get("zone").and_then(|v| v.as_str()) {
            let sfha = fl.get("sfha").and_then(|v| v.as_bool()).unwrap_or(false);
            let risk = fl.get("risk").and_then(|v| v.as_str()).unwrap_or("");
            let desc = fl.get("description").and_then(|v| v.as_str()).unwrap_or("");
            if sfha {
                add("bad", format!("In FEMA flood Zone {z}, a high-risk Special Flood Hazard Area ({}). Flood insurance is required for federally backed mortgages; get an elevation certificate and a quote early.", desc.to_lowercase()));
            } else if risk == "undetermined" {
                add("watch", "FEMA flood Zone D: flood risk here has not been studied, so it is undetermined rather than low. Flood insurance is not federally required, but ask for a quote and check local drainage history.".into());
            } else if risk == "moderate" {
                add("watch", format!("FEMA flood Zone X (shaded): moderate flood risk ({}). Insurance is optional but often inexpensive.", desc.trim_start_matches("Moderate risk: ")));
            } else if risk == "minimal" {
                add("good", "FEMA flood Zone X: minimal flood hazard, outside the mapped 100- and 500-year floodplains.".into());
            }
            if !sfha {
                if let Some(ns) = fl.get("nearestSfha").filter(|v| !v.is_null()) {
                    let dm = ns.get("distM").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let nz = ns.get("zone").and_then(|v| v.as_str()).unwrap_or("A");
                    let dir = ns.get("direction").and_then(|v| v.as_str()).unwrap_or("nearby");
                    add(if dm <= 500.0 { "watch" } else { "good" }, format!("Nearest FEMA high-risk flood zone (Zone {nz}) is {} to the {dir}.", fmt_dist(dm)));
                } else {
                    add("good", "No FEMA high-risk flood zone (A/V zones) within 2 km (1.2 mi).".into());
                }
            }
        } else if fl.get("checked").and_then(|v| v.as_bool()) == Some(false) {
            add("watch", "FEMA flood zone (NFHL) check unavailable — confirm official flood-zone designation before relying on flood-insurance status.".into());
        } else if fl.get("checked").and_then(|v| v.as_bool()) == Some(true)
            && fl.get("mapped").and_then(|v| v.as_bool()) == Some(false)
        {
            add("watch", "FEMA NFHL has no mapped flood-zone polygon at this location (unmapped area).".into());
        }
    }

    if is_ca {
        if let Some(dam) = data.pointer("/other/damInundation") {
            if dam.get("inArea").and_then(|v| v.as_bool()) == Some(true) {
                let names: Vec<&str> = dam.get("dams").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
                add("bad", format!("Inside a mapped dam-failure inundation area ({} dam{}); this must be disclosed on the California Natural Hazard Disclosure statement.", names.join(", "), if names.len() > 1 { "s" } else { "" }));
            } else if let Some(near) = dam.get("nearbyWithin1km").and_then(|v| v.as_array()).filter(|a| !a.is_empty()) {
                let names: Vec<&str> = near.iter().filter_map(|x| x.as_str()).collect();
                add("good", format!("Outside mapped dam-failure inundation areas (the {} dam inundation area{} within 1 km).", names.join(", "), if names.len() > 1 { "s are" } else { " is" }));
            }
        }
        if data.pointer("/other/seaLevelRise/exposed").and_then(|v| v.as_bool()) == Some(true) {
            let ft = data.pointer("/other/seaLevelRise/firstExposedAtFt").and_then(|v| v.as_i64()).unwrap_or(6);
            add("watch", format!("Exposed to flooding with {ft} ft of sea-level rise (USGS CoSMoS projections)."));
        }
        if data.pointer("/other/tsunamiHazardArea").and_then(|v| v.as_bool()) == Some(true) {
            add("bad", "Inside a California tsunami hazard (evacuation) area.".into());
        }
    }

    if let Some(g) = data.pointer("/earthquake/designMaps/pgaM").or_else(|| data.pointer("/earthquake/designMaps/pga")).and_then(|v| v.as_f64()) {
        let word = if g >= 0.8 { "very strong" } else if g >= 0.5 { "strong" } else if g >= 0.2 { "moderate" } else { "low" };
        let advice = if g >= 0.2 { " Check foundation bolting / cripple-wall bracing (especially pre-1980 wood frame) and price earthquake insurance." } else { "" };
        let sdc = data.pointer("/earthquake/designMaps/sdc").and_then(|v| v.as_str()).unwrap_or("n/a");
        add(if g >= 0.5 { "bad" } else if g >= 0.2 { "watch" } else { "good" }, format!("Earthquake shaking potential is {word}: building-code (ASCE 7-22) peak ground acceleration {g} g, Seismic Design Category {sdc}.{advice}"));
    }

    let fmt_fault = |f: &Value| -> (String, String, String, f64) {
        let nm = f.get("name").and_then(|v| v.as_str()).unwrap_or("Fault");
        let sec = f.get("section").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
        let full = match sec { Some(s) => format!("{nm} ({s})"), None => nm.to_string() };
        let dm = f.get("distM").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let dkm = f.get("distKm").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let dmi = f.get("distMi").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let dir = f.get("direction").and_then(|v| v.as_str()).unwrap_or("");
        let d_str = if dm < 50.0 { "mapped within ~50 m of the property".into() } else { format!("{dkm} km ({dmi:.1} mi) {dir}").trim().to_string() };
        let age_note = f.get("ageNote").and_then(|v| v.as_str()).map(|s| s.to_string())
            .unwrap_or_else(|| format!("is of {} age", f.get("age").and_then(|v| v.as_str()).unwrap_or("unknown")));
        let slip = f.get("slipRate").and_then(|v| v.as_str())
            .filter(|s| !s.to_ascii_lowercase().contains("unspecified"))
            .map(|s| format!(", slip rate {}", s.to_lowercase()))
            .unwrap_or_default();
        (full, d_str, format!("{age_note}{slip}"), dkm)
    };

    if let Some(nf) = data.pointer("/earthquake/faults/nearest/0") {
        let (full, d_str, note, dkm) = fmt_fault(nf);
        let act = nf.get("recentlyActive").and_then(|v| v.as_bool()).unwrap_or(false);
        let lvl = if act { if dkm < 3.0 { "bad" } else if dkm < 15.0 { "watch" } else { "good" } } else if dkm < 1.0 { "watch" } else { "good" };
        add(lvl, format!("Nearest mapped fault: {full}, {d_str}; it {note}."));
    }

    if let Some(ca) = data.pointer("/earthquake/california").filter(|v| !v.is_null()) {
        let ap = ca.pointer("/alquistPriolo/inZone").and_then(|v| v.as_bool());
        let liq = ca.pointer("/liquefaction/inZone").and_then(|v| v.as_bool());
        let ls = ca.pointer("/landslide/inZone").and_then(|v| v.as_bool());
        let mut inside = Vec::new();
        if ap == Some(true) { inside.push("an Alquist-Priolo Earthquake Fault Zone"); }
        if liq == Some(true) { inside.push("a state Liquefaction Zone"); }
        if ls == Some(true) { inside.push("a state Earthquake-Induced Landslide Zone"); }
        if !inside.is_empty() {
            add("bad", format!("Inside {}; this is disclosed on the Natural Hazard Disclosure and new construction/additions need a site-specific geologic or geotechnical report.", inside.join(" and ")));
        } else if ap == Some(false) && liq == Some(false) && ls == Some(false) {
            let mut near = Vec::new();
            if let Some(d) = ca.pointer("/liquefaction/nearestDistM").and_then(|v| v.as_f64()).filter(|&d| d <= 500.0) {
                near.push(format!("a liquefaction zone is {} away", fmt_mi(d)));
            }
            if let Some(d) = ca.pointer("/landslide/nearestDistM").and_then(|v| v.as_f64()).filter(|&d| d <= 500.0) {
                near.push(format!("a landslide zone is {} away", fmt_mi(d)));
            }
            add(if near.is_empty() { "good" } else { "watch" }, format!("Not in a state Alquist-Priolo fault zone, liquefaction zone or earthquake-induced landslide zone{}.", if near.is_empty() { String::new() } else { format!("; however {}", near.join(" and ")) }));
        }
    }

    if let Some(caf) = data.pointer("/wildfire/california").filter(|v| !v.is_null()) {
        if let Some(cls) = caf.get("fhszClass").and_then(|v| v.as_i64()) {
            let fhsz = caf.get("fhsz").and_then(|v| v.as_str()).unwrap_or("Moderate");
            if cls >= 1 {
                add(if cls >= 2 { "bad" } else { "watch" }, format!("In a CAL FIRE {fhsz} Fire Hazard Severity Zone. Expect defensible-space (100 ft) and ember-resistant construction rules, a disclosure, and possibly limited/expensive fire insurance."));
            } else if let Some(n) = caf.get("nearestHighOrVeryHigh").filter(|v| !v.is_null()) {
                let dm = n.get("distM").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let nc = n.get("class").and_then(|v| v.as_str()).unwrap_or("High");
                let dir = n.get("direction").and_then(|v| v.as_str()).unwrap_or("nearby");
                add(if dm <= 1000.0 { "watch" } else { "good" }, format!("Not in a CAL FIRE Fire Hazard Severity Zone; the nearest {nc} zone is {} to the {dir}.", fmt_dist(dm)));
            } else {
                add("good", "Not in a CAL FIRE Fire Hazard Severity Zone and no High/Very High zone within 3.1 mi.".into());
            }
        }
    } else if let Some(whp) = data.pointer("/wildfire/whp").filter(|v| !v.is_null()) {
        let c = whp.get("class").and_then(|v| v.as_i64()).unwrap_or(0);
        let lbl = whp.get("label").and_then(|v| v.as_str()).unwrap_or("Low");
        add(if (4..=5).contains(&c) { "bad" } else if c == 3 { "watch" } else { "good" }, format!("USFS Wildfire Hazard Potential at this spot: {lbl}."));
    }

    if let Some(nri) = data.get("nri").filter(|v| !v.is_null()) {
        if let Some(rt) = nri.pointer("/risk/rating").and_then(|v| v.as_str()) {
            let sc = nri.pointer("/risk/score").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let lvl = if rt == "Very High" || rt == "Relatively High" { "bad" } else if rt == "Relatively Moderate" { "watch" } else { "good" };
            let top: Vec<String> = nri.get("topHazards").and_then(|v| v.as_array()).map(|a| {
                a.iter().take(3).filter_map(|h| Some(format!("{} ({})", h.get("name")?.as_str()?, h.get("rating")?.as_str()?.to_lowercase()))).collect()
            }).unwrap_or_default();
            add(lvl, format!("FEMA National Risk Index for this census tract: {} overall risk (score {sc}/100){}.", rt.to_lowercase(), if top.is_empty() { String::new() } else { format!("; leading hazards: {}", top.join(", ")) }));
        }
    }

    if let Some(rz) = data.get("radon") {
        if let Some(z) = rz.get("epaZone").and_then(|v| v.as_i64()) {
            let pot = match z { 1 => "highest", 2 => "moderate", _ => "low" };
            let lvl = radon_concern_level(rz).unwrap_or("good");
            add(lvl, format!("Radon: EPA radon Zone {z} ({pot} potential). EPA recommends mitigation at >= 4.0 pCi/L; a short-term test (~$15-30) is the only way to know."));
        }
    }

    out
}

pub fn build_nhd(data: &Value) -> Value {
    let yn = |v: Option<bool>| match v { Some(true) => "yes", Some(false) => "no", None => "unknown" };
    let sfha = data.pointer("/flood/sfha").and_then(|v| v.as_bool());
    let dam = data.pointer("/other/damInundation/inArea").and_then(|v| v.as_bool());
    let fhsz_cls = data.pointer("/wildfire/california/fhszClass").and_then(|v| v.as_i64());
    let fhsz_str = data.pointer("/wildfire/california/fhsz").and_then(|v| v.as_str()).unwrap_or("unknown");
    let resp = data.pointer("/wildfire/california/responsibilityArea").and_then(|v| v.as_str());
    let ap = data.pointer("/earthquake/california/alquistPriolo/inZone").and_then(|v| v.as_bool());
    let liq = data.pointer("/earthquake/california/liquefaction/inZone").and_then(|v| v.as_bool());
    let ls = data.pointer("/earthquake/california/landslide/inZone").and_then(|v| v.as_bool());
    let liq_mapped = data.pointer("/earthquake/california/liquefaction/mapped").and_then(|v| v.as_bool()).unwrap_or(false);
    let shz = if liq == Some(true) || ls == Some(true) {
        "yes"
    } else if liq == Some(false) && ls == Some(false) {
        if liq_mapped { "no" } else { "no (not mapped)" }
    } else {
        "unknown"
    };
    json!({
        "note": "Indicative only, from public GIS layers; the official NHD report comes from a licensed NHD provider.",
        "specialFloodHazardArea": yn(sfha),
        "damInundationArea": yn(dam),
        "veryHighFireHazardZone": yn(fhsz_cls.map(|c| c == 3)),
        "fireHazardSeverityZone": fhsz_str,
        "stateResponsibilityArea": yn(resp.map(|r| r == "SRA")),
        "earthquakeFaultZone": yn(ap),
        "seismicHazardZone": shz,
    })
}

fn fault_age_rank(age: &str) -> u8 {
    let l = age.to_ascii_lowercase();
    if l.contains("historic") {
        5
    } else if l.contains("latest quaternary") {
        4
    } else if l.contains("late quaternary") {
        3
    } else if l.contains("middle and late") {
        2
    } else if l.contains("quaternary") {
        1
    } else {
        0
    }
}

/// Group USGS Quaternary fault segments by fault & section, retaining the youngest/most-recent age
/// for each fault zone, tracking the closest active (`historic`/`latest Quaternary`) segment for
/// `nearestRecentlyActive`, and producing clipped/simplified GeoJSON polylines without segment truncation.
pub fn summarize_fault_features(
    lat: f64,
    lon: f64,
    feats: &[Value],
) -> (Value, Vec<Value>, Vec<Value>) {
    let is_active = |age: &str| fault_age_rank(age) >= 4;
    let age_note = |age: &str| -> Option<&'static str> {
        match fault_age_rank(age) {
            5 => Some("has ruptured in historic time (~last 150 years)"),
            4 => Some("has moved within the last ~15,000 years"),
            3 => Some("last moved within ~130,000 years"),
            2 => Some("last moved within ~750,000 years"),
            1 => Some("last moved sometime in the past ~1.6 million years"),
            _ => None,
        }
    };

    struct FaultGroup {
        attr: Value,
        age_rank: u8,
        age_dist_m: f64,
        dist_m: f64,
        nlat: f64,
        nlon: f64,
        active_best: Option<(f64, f64, f64, Value)>,
        geoms: Vec<Value>,
    }

    let mut groups: HashMap<String, FaultGroup> = HashMap::new();
    for f in feats {
        let Some(a) = f.get("attributes") else { continue };
        let Some(geom) = f.get("geometry") else { continue };
        let paths = parse_coord_lists(geom.get("paths"));
        let Some((dm, nlat, nlon)) = paths_distance(lat, lon, &paths) else { continue };
        let fnm = val_str(a.get("fault_name")).unwrap_or("Unnamed fault");
        let snm = val_str(a.get("section_name")).unwrap_or("");
        let seg_age = val_str(a.get("age")).unwrap_or("");
        let seg_rank = fault_age_rank(seg_age);
        let key = format!("{fnm}|{snm}");
        match groups.get_mut(&key) {
            Some(g) => {
                if dm < g.dist_m {
                    g.dist_m = dm;
                    g.nlat = nlat;
                    g.nlon = nlon;
                }
                if seg_rank > g.age_rank || (seg_rank == g.age_rank && dm < g.age_dist_m) {
                    g.attr = a.clone();
                    g.age_rank = seg_rank;
                    g.age_dist_m = dm;
                }
                if seg_rank >= 4
                    && g.active_best
                        .as_ref()
                        .map(|ab| dm < ab.0)
                        .unwrap_or(true)
                {
                    g.active_best = Some((dm, nlat, nlon, a.clone()));
                }
                g.geoms.push(geom.clone());
            }
            None => {
                let active_best = if seg_rank >= 4 {
                    Some((dm, nlat, nlon, a.clone()))
                } else {
                    None
                };
                groups.insert(
                    key,
                    FaultGroup {
                        attr: a.clone(),
                        age_rank: seg_rank,
                        age_dist_m: dm,
                        dist_m: dm,
                        nlat,
                        nlon,
                        active_best,
                        geoms: vec![geom.clone()],
                    },
                );
            }
        }
    }

    let mut sorted: Vec<FaultGroup> = groups.into_values().collect();
    sorted.sort_by(|a, b| a.dist_m.partial_cmp(&b.dist_m).unwrap_or(std::cmp::Ordering::Equal));

    let format_row = |a: &Value, dist_m: f64, nlat: f64, nlon: f64| -> Value {
        let age = val_str(a.get("age")).unwrap_or("");
        let fid = val_str(a.get("fault_id")).unwrap_or("");
        let sid = val_str(a.get("section_id")).unwrap_or("");
        let dir = if dist_m >= 1.0 {
            Some(compass8(bearing_deg(lat, lon, nlat, nlon)))
        } else {
            None
        };
        json!({
            "name": val_str(a.get("fault_name")).unwrap_or("Unnamed fault"),
            "section": val_str(a.get("section_name")),
            "distM": dist_m.round() as i64,
            "distKm": round_n(dist_m / 1000.0, 1),
            "distMi": round_n(dist_m / MILE_M, 2),
            "direction": dir,
            "age": if age.is_empty() { None } else { Some(age) },
            "ageNote": age_note(age),
            "slipRate": val_str(a.get("slip_rate")),
            "slipSense": val_str(a.get("slip_sense")),
            "class": val_str(a.get("class")),
            "recentlyActive": is_active(age),
            "url": if !fid.is_empty() && fid != "0" {
                Some(format!("https://earthquake.usgs.gov/cfusion/qfault/show_report_AB_archive.cfm?fault_id={fid}&section_id={sid}"))
            } else { None },
        })
    };

    let nearest_arr: Vec<Value> = sorted
        .iter()
        .take(6)
        .map(|g| format_row(&g.attr, g.dist_m, g.nlat, g.nlon))
        .collect();
    let nearest_act = sorted
        .iter()
        .filter_map(|g| g.active_best.as_ref())
        .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(dm, nlat, nlon, a)| format_row(a, *dm, *nlat, *nlon))
        .unwrap_or(Value::Null);

    let mut fault_gj_feats = Vec::new();
    for g in sorted.iter().take(24) {
        let row = format_row(&g.attr, g.dist_m, g.nlat, g.nlon);
        for gm in &g.geoms {
            if let Some(gj) = clip_and_simplify_arcgis_geometry(gm, lat, lon, 32_000.0, 0.00012, 240) {
                fault_gj_feats.push(json!({
                    "type": "Feature",
                    "geometry": gj,
                    "properties": {
                        "name": row.get("name"),
                        "age": row.get("age"),
                        "recentlyActive": row.get("recentlyActive"),
                        "distKm": row.get("distKm"),
                    }
                }));
            }
        }
    }

    (
        json!({
            "searchRadiusKm": 30,
            "countWithinRadius": sorted.len(),
            "nearest": nearest_arr,
            "nearestRecentlyActive": nearest_act,
        }),
        nearest_arr,
        fault_gj_feats,
    )
}

pub async fn run_hazards(
    http: &HttpClient,
    env: &EnvConfig,
    lat: f64,
    lon: f64,
    geo: &Value,
) -> Result<ModuleOutput> {
    let hub_db = crate::extensions::ExtensionHub::global().active_db();
    run_hazards_with_db(http, env, hub_db.as_ref(), lat, lon, geo).await
}

pub async fn run_hazards_with_db(
    http: &HttpClient,
    env: &EnvConfig,
    db: Option<&crate::db::DbStore>,
    lat: f64,
    lon: f64,
    geo: &Value,
) -> Result<ModuleOutput> {
    if !lat.is_finite() || !lon.is_finite() || lat.abs() > 90.0 || lon.abs() > 180.0 {
        return Err(anyhow!("hazards: lat/lon must be valid WGS84 coordinates"));
    }
    let _ = env.get("OUTBOUND_NO_PROXY");
    let state = val_str(geo.get("state")).unwrap_or("").to_uppercase();
    let county_fips = val_str(geo.get("countyFips")).unwrap_or("");
    let state_fips = val_str(geo.get("stateFips")).unwrap_or("");
    let is_ca = state == "CA" || state_fips == "06" || county_fips.starts_with("06");

    let tract11 = extract_tract11_from_geo(geo);

    let mut warnings = Vec::new();
    let mut sources = Vec::new();
    let mut map_layers: Vec<Value> = Vec::new();
    let mut layers_map = serde_json::Map::new();

    let fault_extra = [("geometryPrecision", "5".to_string()), ("maxAllowableOffset", "0.0003".to_string())];

    let cgs_ap = format!("{AGO_CGS}/CGS_Alquist_Priolo_Fault_Zones/FeatureServer/0");
    let cgs_liq = format!("{AGO_CGS}/CGS_Liquefaction_Zones/FeatureServer/0");
    let cgs_ls = format!("{AGO_CGS}/CGS_Landslide_Zones/FeatureServer/0");
    let cgs_uneval = format!("{AGO_CGS}/CGS_SHZ_Unevaluated_Areas/FeatureServer/0");
    let cgs_quads = format!("{AGO_CGS}/IW_Regulatory/FeatureServer/0");
    let cgs_radon = "https://gis.conservation.ca.gov/server/rest/services/CGS/RadonPotentialZones/MapServer/0";
    let cgs_ls_susc = "https://gis.conservation.ca.gov/server/rest/services/CGS/MS58_LandslideSusceptibility_Classes/MapServer";
    let cgs_tsunami = "https://gis.conservation.ca.gov/server/rest/services/CGS/IW_Tsunami_Hazard_Area/FeatureServer/0";
    let fhsz_sra = format!("{CALFIRE}/FHSZSRA_23_3/FeatureServer/0");
    let fhsz_lra = format!("{CALFIRE}/FHSALRA25_v1_All/FeatureServer/0");
    let calfire_sra = format!("{CALFIRE}/SRA25_2/FeatureServer/0");
    let oes_slr = format!("{CALOES}/4");
    let oes_levee = format!("{CALOES}/6");
    let oes_dam = format!("{CALOES}/7");

    let nat_fut = Box::pin(async {
        let g1 = Box::pin(async {
            tokio::join!(
                fetch_flood_with_db(http, db, lat, lon, geo),
                q_nri_with_db(http, db, lat, lon, tract11.as_deref(), geo),
            )
        });
        let g2 = Box::pin(async {
            tokio::join!(
                q_asce(http, lat, lon),
                q_comcat_count(http, lat, lon),
                q_comcat_list(http, lat, lon, 100, "4.0", 10),
                q_comcat_list(http, lat, lon, 50, "3.5", 500),
            )
        });
        let g3 = Box::pin(async {
            tokio::join!(
                q_faults_tile_cached(http, db, lat, lon, geo, "OBJECTID,fault_name,section_name,fault_id,section_id,age,slip_rate,slip_sense,class", &fault_extra),
                q_whp(http, lat, lon),
                http.arcgis_point_query(URL_GRP, lat, lon, "AREANAME,GRP,RI,STATE", &[], REQ_TIMEOUT_MS),
                q_elevation(http, lat, lon),
            )
        });
        let ((flood, a3), (a4, a5, a6, a7), (a8, a9, a10, a11)) = tokio::join!(g1, g2, g3);
        (flood.point_rows, flood.near_features, flood.panel_rows, a3, a4, a5, a6, a7, a8, a9, a10, a11)
    });

    let ap_off = [("maxAllowableOffset", "0.0001".to_string())];
    let fhsz_extra = [("where", "FHSZ >= 2".to_string()), ("maxAllowableOffset", "0.0002".to_string())];
    let dam_geom_ex = [
        ("geometryPrecision", "5".to_string()),
        ("maxAllowableOffset", "0.0001".to_string()),
    ];

    let ca_fut = Box::pin(async {
        if !is_ca {
            return None;
        }
        let c1 = Box::pin(async {
            tokio::join!(
                http.arcgis_point_query(&cgs_ap, lat, lon, "QUAD_NAME,ZN_RELEASED,ZN_REVISED", &[], REQ_TIMEOUT_MS),
                arcgis_radius_tile_cached(http, db, lat, lon, geo, "ca_ap", &cgs_ap, "QUAD_NAME", 5000.0, &ap_off, "California Geological Survey (Alquist-Priolo)"),
                http.arcgis_point_query(&cgs_liq, lat, lon, "QUAD_NAME,RELEASED,REVISED", &[], REQ_TIMEOUT_MS),
                arcgis_radius_tile_cached(http, db, lat, lon, geo, "ca_liq", &cgs_liq, "QUAD_NAME", 2000.0, &ap_off, "California Geological Survey (Liquefaction)"),
                http.arcgis_point_query(&cgs_ls, lat, lon, "QUAD_NAME,RELEASED,REVISED", &[], REQ_TIMEOUT_MS),
            )
        });
        let c2 = Box::pin(async {
            tokio::join!(
                arcgis_radius_tile_cached(http, db, lat, lon, geo, "ca_ls", &cgs_ls, "QUAD_NAME", 2000.0, &ap_off, "California Geological Survey (Landslide)"),
                http.arcgis_point_query(&cgs_uneval, lat, lon, "QUAD_NAME,LABEL", &[], REQ_TIMEOUT_MS),
                http.arcgis_point_query(&cgs_quads, lat, lon, "CELL_NAME,ZoneType,PopupInfo", &[], REQ_TIMEOUT_MS),
                http.arcgis_point_query(&fhsz_sra, lat, lon, "SRA,FHSZ,FHSZ_Description", &[], REQ_TIMEOUT_MS),
                http.arcgis_point_query(&fhsz_lra, lat, lon, "SRA,FHSZ,FHSZ_Description", &[], REQ_TIMEOUT_MS),
            )
        });
        let c3 = Box::pin(async {
            tokio::join!(
                arcgis_radius_tile_cached(http, db, lat, lon, geo, "ca_fhsz_sra", &fhsz_sra, "SRA,FHSZ,FHSZ_Description", 5000.0, &fhsz_extra, "CAL FIRE FHSZ SRA"),
                arcgis_radius_tile_cached(http, db, lat, lon, geo, "ca_fhsz_lra", &fhsz_lra, "SRA,FHSZ,FHSZ_Description", 5000.0, &fhsz_extra, "CAL FIRE FHSZ LRA"),
                http.arcgis_point_query(&calfire_sra, lat, lon, "SRA", &[], REQ_TIMEOUT_MS),
                http.arcgis_point_query(cgs_radon, lat, lon, "Rn_ZONE_POTENTIAL,DESCRIPTION_RN_ZONE,CGS_REPORT,REPORT_NAME,LINK_REPORT,STUDY_AREA", &[], REQ_TIMEOUT_MS),
                q_mapserver_identify(http, cgs_ls_susc, lat, lon),
            )
        });
        let c4 = Box::pin(async {
            tokio::join!(
                http.arcgis_point_query(cgs_tsunami, lat, lon, "Evacuate,County,Map_Link", &[], REQ_TIMEOUT_MS),
                http.arcgis_point_query(&oes_dam, lat, lon, "DamName,NID,HazardCl,LoadingScn,FailedStr", &[], REQ_TIMEOUT_MS),
                arcgis_radius_tile_cached(http, db, lat, lon, geo, "ca_dam", &oes_dam, "DamName,NID,HazardCl", 1000.0, &dam_geom_ex, "Cal OES Dam Inundation"),
                http.arcgis_point_query(&oes_slr, lat, lon, "hazard_description,source_agency,source_title", &[], REQ_TIMEOUT_MS),
                http.arcgis_point_query(&oes_levee, lat, lon, "Flood_Type,Depth_Rang,hazard_description", &[], REQ_TIMEOUT_MS),
            )
        });
        let (
            (b0, b1, b2, b3, b4),
            (b5, b6, b7, b8, b9),
            (b10, b11, b12, b13, b14),
            (b15, b16, b17, b18, b19),
        ) = tokio::join!(c1, c2, c3, c4);
        Some((
            b0, b1, b2, b3, b4, b5, b6, b7, b8, b9,
            b10, b11, b12, b13, b14, b15, b16, b17, b18, b19,
        ))
    });

    let (
        (r_fl_pt, r_fl_near, r_panel, r_nri, r_asce, r_qcnt, r_qtop, r_qchart, r_faults, r_whp, r_grp, r_elev),
        ca_res,
    ) = tokio::join!(nat_fut, ca_fut);

    let (elev_m, elev_ft, elev_source) = match r_elev {
        Ok(el) => {
            sources.push(SourceItem::new(el.source, el.source_url));
            (
                Some(round_n(el.meters, 1)),
                Some((el.meters / FOOT_M).round() as i64),
                Some(el.source),
            )
        }
        Err(e) => {
            tracing::warn!(error = %e, "hazards: site elevation unavailable from every source");
            warnings.push(
                "Site elevation could not be retrieved (the USGS 3DEP and Copernicus DEM elevation services did not respond)."
                    .to_string(),
            );
            (None, None, None)
        }
    };

    // 1. FLOOD
    let fl_pt_checked = r_fl_pt.is_ok();
    let fl_near_checked = r_fl_near.is_ok();
    let fl_mapped = match &r_fl_pt {
        Ok((rows, _)) => Some(!rows.is_empty()),
        Err(_) => None,
    };
    let mut fl_zone: Option<String> = None;
    let mut fl_sub: Option<String> = None;
    let mut fl_sfha: Option<bool> = None;
    let mut fl_risk: Option<&str> = None;
    let mut fl_desc: Option<String> = None;
    let mut fl_bfe: Option<f64> = None;
    let mut fl_depth: Option<f64> = None;
    let mut fl_dfirm: Option<String> = None;
    match r_fl_pt {
        Ok((rows, src)) => {
            sources.push(SourceItem::new(src.name, &src.url));
            let row = rows.iter().find(|a| val_str(a.get("SFHA_TF")) == Some("T")).or_else(|| rows.first());
            if let Some(r) = row {
                let z = val_str(r.get("FLD_ZONE")).unwrap_or("").to_string();
                let sub = val_str(r.get("ZONE_SUBTY")).map(|s| s.to_string());
                let (is_s, rk, lbl) = flood_zone_info(&z, sub.as_deref().unwrap_or(""));
                fl_sfha = Some(val_str(r.get("SFHA_TF")) == Some("T") || is_s);
                fl_zone = if z.is_empty() { None } else { Some(z) };
                fl_sub = sub;
                fl_risk = Some(rk);
                fl_desc = Some(lbl);
                fl_bfe = val_num(r.get("STATIC_BFE"));
                fl_depth = val_num(r.get("DEPTH"));
                fl_dfirm = val_str(r.get("DFIRM_ID")).map(|s| s.to_string());
            }
        }
        Err(e) => warnings.push(format!("FEMA flood zone (NFHL) unavailable: {e}")),
    }
    let (firm_panel, firm_eff) = match r_panel {
        Ok((rows, _src)) => match rows.first() {
            Some(p) => (val_str(p.get("FIRM_PAN")).map(|s| s.to_string()), iso_date_ms(p.get("EFF_DATE"))),
            None => (None, None),
        },
        Err(_) => (None, None),
    };
    let mut nearest_sfha = Value::Null;
    let mut sfha_2km: Vec<Value> = Vec::new();
    match r_fl_near {
        Ok((feats, src)) => {
            if !sources.iter().any(|s| s.url == src.url) {
                sources.push(SourceItem::new(src.name, &src.url));
            }
            let mut hits: Vec<(f64, f64, f64, String, Value)> = Vec::new();
            for f in feats {
                let z = f.pointer("/attributes/FLD_ZONE").and_then(|v| v.as_str()).unwrap_or("A").to_string();
                if let Some(geom) = f.get("geometry") {
                    if let Some((dm, nlat, nlon)) = polygon_distance(lat, lon, geom) {
                        hits.push((dm, nlat, nlon, z, geom.clone()));
                    }
                }
            }
            hits.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            if let Some((dm, nlat, nlon, z, _)) = hits.first() {
                let dir = if *dm > 0.5 { Some(compass8(bearing_deg(lat, lon, *nlat, *nlon))) } else { None };
                nearest_sfha = json!({
                    "zone": z,
                    "distM": dm.round() as i64,
                    "distMi": round_n(*dm / MILE_M, 2),
                    "direction": dir,
                });
                let mut by_zone: HashMap<String, f64> = HashMap::new();
                for (d, _, _, zn, _) in &hits {
                    let e = by_zone.entry(zn.clone()).or_insert(*d);
                    if *d < *e { *e = *d; }
                }
                let mut zlist: Vec<(String, i64)> = by_zone.into_iter().map(|(k, v)| (k, v.round() as i64)).collect();
                zlist.sort_by_key(|x| x.1);
                sfha_2km = zlist.into_iter().map(|(zn, dm)| json!({ "zone": zn, "distM": dm })).collect();

                let gj_feats: Vec<Value> = hits.iter().take(20).filter_map(|(d, _, _, zn, g)| {
                    let gj = clip_and_simplify_arcgis_geometry(g, lat, lon, 6_000.0, 0.00007, 180)?;
                    Some(json!({
                        "type": "Feature",
                        "geometry": gj,
                        "properties": { "name": format!("FEMA Zone {zn}"), "zone": zn, "distM": d.round() as i64 }
                    }))
                }).collect();
                if !gj_feats.is_empty() {
                    let lyr = json!({ "id": "fema-sfha", "title": "FEMA high-risk flood zones (SFHA)", "kind": "polygons", "color": "#1f78b4", "features": gj_feats });
                    layers_map.insert("flood".into(), lyr.clone());
                    map_layers.push(lyr);
                }
            }
        }
        Err(e) => warnings.push(format!("Nearby FEMA SFHA query unavailable: {e}")),
    }

    let flood_val = json!({
        "checked": fl_pt_checked,
        "nearChecked": fl_near_checked,
        "mapped": fl_mapped,
        "zone": fl_zone,
        "zoneSubtype": fl_sub,
        "sfha": fl_sfha,
        "risk": fl_risk,
        "description": fl_desc,
        "bfe": fl_bfe,
        "baseFloodElevationFt": fl_bfe,
        "depthFt": fl_depth,
        "dfirmId": fl_dfirm,
        "firmPanel": firm_panel,
        "firmEffective": firm_eff,
        "insuranceRequired": fl_sfha,
        "nearestSfha": nearest_sfha,
        "sfhaWithin2km": sfha_2km,
    });

    // 2. NRI
    let nri_checked = r_nri.is_ok();
    let mut nri_all: HashMap<String, Value> = HashMap::new();
    let nri_val = match r_nri {
        Ok(Some(a)) => {
            sources.push(SourceItem::new("FEMA National Risk Index - Census Tracts", URL_NRI));
            let mut rated: Vec<Value> = Vec::new();
            for &(code, name) in NRI_HAZ {
                let sc = val_num(a.get(&format!("{code}_RISKS"))).map(|v| round_n(v, 1));
                let rt = val_str(a.get(&format!("{code}_RISKR"))).map(|s| s.to_string());
                let eal = val_num(a.get(&format!("{code}_EALT"))).map(|v| v.round() as i64);
                let item = json!({ "code": code, "name": name, "score": sc, "rating": rt, "ealUSD": eal });
                if rt.as_deref().map(is_rated_nri).unwrap_or(false) && sc.unwrap_or(0.0) > 0.0 {
                    rated.push(item.clone());
                }
                nri_all.insert(code.to_string(), item);
            }
            rated.sort_by(|x, y| {
                y.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0)
                    .partial_cmp(&x.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let r_sc = val_num(a.get("RISK_SCORE")).map(|v| round_n(v, 1));
            let r_rt = val_str(a.get("RISK_RATNG"));
            let sovi = json!({ "score": val_num(a.get("SOVI_SCORE")).map(|v| round_n(v, 1)), "rating": val_str(a.get("SOVI_RATNG")) });
            json!({
                "tract": val_str(a.get("TRACTFIPS")),
                "version": val_str(a.get("NRI_VER")),
                "population": val_num(a.get("POPULATION")).map(|v| v.round() as i64),
                "buildingValueUSD": val_num(a.get("BUILDVALUE")).map(|v| v.round() as i64),
                "score": r_sc,
                "rating": r_rt,
                "sow": sovi.clone(),
                "risk": { "score": r_sc, "rating": r_rt, "statePercentile": val_num(a.get("RISK_SPCTL")).map(|v| round_n(v, 1)) },
                "expectedAnnualLoss": {
                    "totalUSD": val_num(a.get("EAL_VALT")).map(|v| v.round() as i64),
                    "score": val_num(a.get("EAL_SCORE")).map(|v| round_n(v, 1)),
                    "rating": val_str(a.get("EAL_RATNG")),
                },
                "socialVulnerability": sovi,
                "communityResilience": { "score": val_num(a.get("RESL_SCORE")).map(|v| round_n(v, 1)), "rating": val_str(a.get("RESL_RATNG")) },
                "topHazards": rated,
            })
        }
        Ok(None) => Value::Null,
        Err(e) => { warnings.push(format!("FEMA NRI unavailable: {e}")); Value::Null }
    };

    // 3. EARTHQUAKE
    let asce_checked = r_asce.is_ok();
    let design_maps = match r_asce {
        Ok(j) => {
            sources.push(SourceItem::new("USGS Seismic Design Web Services (ASCE 7-22)", URL_ASCE722));
            let d = j.pointer("/response/data").unwrap_or(&Value::Null);
            let m = j.pointer("/response/metadata").unwrap_or(&Value::Null);
            let sc = j.pointer("/request/parameters/siteClass").and_then(|v| v.as_str()).unwrap_or("D");
            let pgam = val_num(d.get("pgam")).or_else(|| val_num(d.get("pga")));
            let pga = val_num(d.get("pga")).or(pgam);
            json!({
                "standard": "ASCE 7-22", "riskCategory": "II", "siteClass": sc,
                "pga": pga, "pgaM": pgam, "ss": val_num(d.get("ss")), "s1": val_num(d.get("s1")),
                "sms": val_num(d.get("sms")), "sm1": val_num(d.get("sm1")),
                "sds": val_num(d.get("sds")), "sd1": val_num(d.get("sd1")),
                "sdc": val_str(d.get("sdc")), "vs30": val_num(m.get("vs30")),
                "modelVersion": val_str(m.get("modelVersion")),
            })
        }
        Err(e) => { warnings.push(format!("USGS ASCE 7-22 unavailable: {e}")); Value::Null }
    };

    let mut quake_layer_feats = Vec::new();
    let recent_quakes: Vec<Value> = match r_qtop {
        Ok(feats) => {
            sources.push(SourceItem::new("USGS ANSS Comprehensive Earthquake Catalog (ComCat)", URL_COMCAT_QUERY));
            feats.iter().filter_map(|f| {
                let coords = f.pointer("/geometry/coordinates")?.as_array()?;
                let qlon = coords.first()?.as_f64()?;
                let qlat = coords.get(1)?.as_f64()?;
                let depth = coords.get(2).and_then(|v| v.as_f64()).map(|d| round_n(d, 1));
                let p = f.get("properties")?;
                let mag = val_num(p.get("mag"));
                let place = val_str(p.get("place"));
                let dt = iso_date_ms(p.get("time"));
                let d_mi = round_n(haversine_mi(lat, lon, qlat, qlon), 1);
                let d_km = round_n(haversine_m(lat, lon, qlat, qlon) / 1000.0, 1);
                quake_layer_feats.push(json!({
                    "type": "Feature",
                    "geometry": { "type": "Point", "coordinates": [round_n(qlon, 5), round_n(qlat, 5)] },
                    "properties": { "name": format!("M{} {}", mag.unwrap_or(0.0), dt.as_deref().unwrap_or("")), "mag": mag, "distMi": d_mi }
                }));
                Some(json!({ "mag": mag, "place": place, "date": dt, "distKm": d_km, "distMi": d_mi, "depthKm": depth, "url": val_str(p.get("url")) }))
            }).collect()
        }
        Err(e) => { warnings.push(format!("USGS ComCat recent quakes unavailable: {e}")); Vec::new() }
    };
    if !quake_layer_feats.is_empty() {
        let lyr = json!({ "id": "largest-earthquakes", "title": "Largest earthquakes within 100 km since 1900", "kind": "points", "color": "#6a3d9a", "features": quake_layer_feats });
        layers_map.insert("quakes".into(), lyr.clone());
        map_layers.push(lyr);
    }

    let count_m4 = r_qcnt.ok();
    let (chart_m35, count_m35, chart_trunc) = match r_qchart {
        Ok(feats) => {
            let trunc = feats.len() >= 500;
            let mut items: Vec<(i64, Value)> = feats.iter().filter_map(|f| {
                let c = f.pointer("/geometry/coordinates")?.as_array()?;
                let qlon = c.first()?.as_f64()?;
                let qlat = c.get(1)?.as_f64()?;
                let p = f.get("properties")?;
                let ms = p.get("time")?.as_i64()?;
                let dt = iso_date_ms(p.get("time"))?;
                let d_km = round_n(haversine_m(lat, lon, qlat, qlon) / 1000.0, 1);
                Some((ms, json!({ "t": dt, "mag": val_num(p.get("mag")), "distKm": d_km })))
            }).collect();
            items.sort_by_key(|x| x.0);
            let cnt = if trunc { None } else { Some(items.len() as i64) };
            (items.into_iter().map(|x| x.1).collect::<Vec<_>>(), cnt, trunc)
        }
        Err(_) => (Vec::new(), None, false),
    };
    let eq_history = json!({
        "since": "1900-01-01",
        "countM4Within50km": count_m4,
        "countM35Within50km": count_m35,
        "largestWithin100km": recent_quakes,
        "chartM35Within50km": chart_m35,
        "chartTruncated": chart_trunc,
    });

    let faults_checked = r_faults.is_ok();
    let (faults_obj, nearest_faults_arr) = match r_faults {
        Ok(feats) => {
            sources.push(SourceItem::new("USGS Quaternary Fault and Fold Database", URL_QFAULTS));
            let (fobj, nf, fault_gj_feats) = summarize_fault_features(lat, lon, &feats);
            if !fault_gj_feats.is_empty() {
                let lyr = json!({ "id": "quaternary-faults", "title": "Quaternary faults (USGS)", "kind": "lines", "color": "#d7301f", "features": fault_gj_feats });
                layers_map.insert("faults".into(), lyr.clone());
                map_layers.push(lyr);
            }
            (fobj, nf)
        }
        Err(e) => {
            warnings.push(format!("USGS Quaternary faults unavailable: {e}"));
            (Value::Null, Vec::new())
        }
    };

    // 4. WILDFIRE + CA ADD-ONS
    let whp_checked = r_whp.is_ok();
    let whp_val = match r_whp {
        Ok(Some(c)) => {
            sources.push(SourceItem::new("USDA Forest Service - Wildfire Hazard Potential (2023)", URL_WHP));
            let lbl = match c { 1 => "Very Low", 2 => "Low", 3 => "Moderate", 4 => "High", 5 => "Very High", 6 => "Non-burnable", 7 => "Water", _ => "Unknown" };
            json!({ "class": c, "label": lbl, "dataset": "USFS Wildfire Hazard Potential 2023 (270 m)" })
        }
        Ok(None) => Value::Null,
        Err(e) => { warnings.push(format!("USFS WHP unavailable: {e}")); Value::Null }
    };
    let wf_nri = nri_all.get("WFIR").map(|w| json!({ "score": w.get("score"), "rating": w.get("rating") })).unwrap_or(Value::Null);

    let mut eq_ca = Value::Null;
    let mut wf_ca = Value::Null;
    let mut radon_ca = Value::Null;
    let mut other_val = json!({});
    let mut ca_eq_checked = false;
    let mut ca_fhsz_checked = false;
    let mut ca_dam_checked = false;

    if let Some((
        r_ap_pt, r_ap_nr, r_liq_pt, r_liq_nr, r_ls_pt, r_ls_nr, r_uneval, r_quad,
        r_sra_pt, r_lra_pt, r_sra_nr, r_lra_nr, r_resp, r_cgs_rn, r_ls_susc,
        r_tsun, r_dam_pt, r_dam_nr, r_slr, r_levee,
    )) = ca_res {
        let quad_row = r_quad.ok().and_then(|v| v.into_iter().next());
        let zt = quad_row.as_ref().and_then(|q| val_str(q.get("ZoneType"))).unwrap_or("");
        let shz_mapped = zt == "all" || zt == "ll";
        let ap_mapped = zt == "all" || zt == "ap";
        let uneval = r_uneval.as_ref().map(|v| !v.is_empty()).ok();
        let quad_json = match &quad_row {
            Some(q) => {
                let mut links = Vec::new();
                if let Some(pop) = val_str(q.get("PopupInfo")) {
                    for cap in HREF_RE.captures_iter(pop) {
                        links.push(json!({ "url": &cap[1], "title": cap[2].trim() }));
                    }
                }
                json!({ "name": val_str(q.get("CELL_NAME")), "seismicHazardZonesMapped": shz_mapped, "faultZonesMapped": ap_mapped, "links": links })
            }
            None => Value::Null,
        };

        let mut build_zone = |pt: Result<Vec<Value>>, nr: Result<Vec<Value>>, mapped: bool, map_id: &str, layer_key: &str, title: &str, color: &str, radius_m: i64| -> Value {
            let (in_z, qn, rel, checked) = match pt {
                Ok(rows) => {
                    let r0 = rows.first();
                    (
                        Some(!rows.is_empty()),
                        r0.and_then(|r| val_str(r.get("QUAD_NAME"))).map(|s| s.to_string()),
                        r0.and_then(|r| val_str(r.get("RELEASED")).or_else(|| val_str(r.get("ZN_RELEASED")))).map(|s| s.to_string()),
                        true,
                    )
                }
                Err(e) => {
                    warnings.push(format!("{title} point query unavailable: {e}"));
                    (None, None, None, false)
                }
            };
            let mut best = f64::INFINITY;
            let mut gj_feats = Vec::new();
            if let Ok(feats) = nr {
                for f in feats.iter().take(20) {
                    if let Some(geom) = f.get("geometry") {
                        if let Some((dm, _, _)) = polygon_distance(lat, lon, geom) {
                            if dm < best { best = dm; }
                            if let Some(gj) = clip_and_simplify_arcgis_geometry(geom, lat, lon, 6_000.0, 0.00007, 180) {
                                gj_feats.push(json!({ "type": "Feature", "geometry": gj, "properties": { "name": title, "distM": dm.round() as i64 } }));
                            }
                        }
                    }
                }
            }
            if !gj_feats.is_empty() {
                let lyr = json!({ "id": map_id, "title": title, "kind": "polygons", "color": color, "features": gj_feats });
                layers_map.insert(layer_key.into(), lyr.clone());
                map_layers.push(lyr);
            }
            let near_m = if in_z == Some(true) { Some(0) } else if best.is_finite() { Some(best.round() as i64) } else { None };
            json!({ "checked": checked, "inZone": in_z, "mapped": mapped, "nearestDistM": near_m, "nearestWithinM": radius_m, "quad": qn, "released": rel })
        };

        let ap_z = build_zone(r_ap_pt, r_ap_nr, ap_mapped, "ca-ap-zones", "alquistPriolo", "CA Alquist-Priolo Earthquake Fault Zones (CGS)", "#e7298a", 5000);
        let liq_z = build_zone(r_liq_pt, r_liq_nr, shz_mapped && uneval != Some(true), "ca-liquefaction", "liquefaction", "CA Liquefaction Zones (CGS)", "#ff7f00", 2000);
        let ls_z = build_zone(r_ls_pt, r_ls_nr, shz_mapped && uneval != Some(true), "ca-eq-landslide", "landslide", "CA Earthquake-Induced Landslide Zones (CGS)", "#8c510a", 2000);
        ca_eq_checked = ap_z.get("checked").and_then(|v| v.as_bool()) == Some(true)
            || liq_z.get("checked").and_then(|v| v.as_bool()) == Some(true)
            || ls_z.get("checked").and_then(|v| v.as_bool()) == Some(true);
        if ca_eq_checked {
            sources.push(SourceItem::new("California Geological Survey - Earthquake Zones of Required Investigation", "https://maps.conservation.ca.gov/cgs/EQZApp/app/"));
        }

        let ls_susc_json = r_ls_susc.ok().and_then(|res| {
            let a = res.first()?.get("attributes")?;
            let cls = val_num(a.get("Raster.Value")).or_else(|| val_num(a.get("UniqueValue.Pixel Value")))? as i64;
            let lbl = if cls >= 9 { "very high" } else if cls >= 7 { "high" } else if cls >= 5 { "moderate" } else { "low" };
            Some(json!({ "class": cls, "scale": "0-10 (CGS Map Sheet 58, deep-seated landslides)", "label": lbl }))
        }).unwrap_or(Value::Null);

        let (tsun_in, tsun_url) = match r_tsun {
            Ok(rows) => {
                sources.push(SourceItem::new("California Geological Survey - Tsunami Hazard Areas", cgs_tsunami));
                let t0 = rows.first();
                let ev = t0.and_then(|t| val_str(t.get("Evacuate"))).map(|s| s.to_ascii_lowercase().starts_with("yes")).unwrap_or(false);
                let u = t0.and_then(|t| val_str(t.get("Map_Link"))).map(|s| s.to_string());
                (Some(ev), u)
            }
            Err(_) => (None, None),
        };

        ca_dam_checked = r_dam_pt.is_ok();
        let pt_dams: Vec<String> = match &r_dam_pt {
            Ok(rows) => {
                let mut seen = HashSet::new();
                rows.iter().filter_map(|d| val_str(d.get("DamName"))).filter(|s| seen.insert(*s)).take(8).map(|s| s.to_string()).collect()
            }
            Err(_) => Vec::new(),
        };
        if let Ok(feats) = &r_dam_nr {
            let mut dam_gj = Vec::new();
            let mut seen_layer = HashSet::new();
            for f in feats.iter().take(12) {
                let attrs = f.get("attributes");
                let name = attrs.and_then(|a| val_str(a.get("DamName"))).unwrap_or("Dam inundation area");
                let nid = attrs.and_then(|a| val_str(a.get("NID")));
                let hcls = attrs.and_then(|a| val_str(a.get("HazardCl")));
                let key = format!("{name}|{}", nid.unwrap_or(""));
                if !seen_layer.insert(key) {
                    continue;
                }
                if let Some(geom) = f.get("geometry") {
                    let dm = polygon_distance(lat, lon, geom).map(|(d, _, _)| d).unwrap_or(0.0);
                    let in_area = dm <= 0.5 || pt_dams.iter().any(|d| d == name);
                    if let Some(gj) = clip_and_simplify_arcgis_geometry(geom, lat, lon, 6_000.0, 0.00007, 200) {
                        dam_gj.push(json!({
                            "type": "Feature",
                            "geometry": gj,
                            "properties": {
                                "name": name,
                                "nid": nid,
                                "hazardClass": hcls,
                                "inArea": in_area,
                                "distM": dm.round() as i64,
                            }
                        }));
                    }
                }
            }
            if !dam_gj.is_empty() {
                let lyr = json!({
                    "id": "dam-inundation",
                    "title": "CA Dam Inundation Areas (Cal OES / DWR)",
                    "kind": "polygons",
                    "color": "#0284c7",
                    "features": dam_gj,
                });
                layers_map.insert("damInundation".into(), lyr.clone());
                map_layers.push(lyr);
            }
        }
        let dam_json = match r_dam_pt {
            Ok(rows) => {
                sources.push(SourceItem::new("CA DWR Division of Safety of Dams Inundation Maps (Cal OES)", &oes_dam));
                let mut near_seen = HashSet::new();
                let near_dams: Vec<String> = if rows.is_empty() {
                    r_dam_nr
                        .as_ref()
                        .map(|feats| {
                            feats
                                .iter()
                                .filter_map(|f| val_str(f.pointer("/attributes/DamName")))
                                .filter(|s| near_seen.insert(*s))
                                .take(8)
                                .map(|s| s.to_string())
                                .collect()
                        })
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                json!({
                    "checked": true,
                    "inArea": !rows.is_empty(),
                    "dams": pt_dams,
                    "hazardClass": rows.first().and_then(|d| val_str(d.get("HazardCl"))),
                    "nearbyWithin1km": near_dams,
                })
            }
            Err(e) => {
                warnings.push(format!("CA dam-inundation lookup unavailable: {e}"));
                Value::Null
            }
        };

        let slr_json = match r_slr {
            Ok(rows) => {
                let mut fts: Vec<i64> = rows.iter().filter_map(|x| val_str(x.get("hazard_description"))).filter_map(|s| FEET_RE.captures(s)?.get(1)?.as_str().parse::<i64>().ok()).collect();
                fts.sort();
                json!({ "exposed": !rows.is_empty(), "firstExposedAtFt": fts.first().copied(), "source": rows.first().and_then(|r| val_str(r.get("source_title"))).unwrap_or("USGS CoSMoS / Point Blue OCOF") })
            }
            Err(_) => Value::Null,
        };
        let levee_prot = r_levee.ok().map(|r| !r.is_empty());

        other_val = json!({
            "checked": ca_dam_checked || tsun_in.is_some(),
            "tsunamiHazardArea": tsun_in,
            "tsunamiMapUrl": tsun_url,
            "damInundation": dam_json,
            "seaLevelRise": slr_json,
            "leveeProtected": levee_prot,
        });

        eq_ca = json!({
            "checked": ca_eq_checked,
            "quadrangle": quad_json,
            "alquistPriolo": ap_z,
            "liquefaction": liq_z,
            "landslide": ls_z,
            "unevaluatedArea": uneval,
            "landslideSusceptibility": ls_susc_json,
            "tsunami": { "inZone": tsun_in, "mapUrl": tsun_url },
            "damInundation": dam_json,
        });

        // CAL FIRE FHSZ
        let resp_area = r_resp.ok().and_then(|v| v.first().and_then(|r| val_str(r.get("SRA"))).map(|s| s.to_string()));
        ca_fhsz_checked = r_sra_pt.is_ok() || r_lra_pt.is_ok();
        let mut fhsz_pts: Vec<(String, Value)> = Vec::new();
        if let Ok(v) = r_sra_pt { for a in v { fhsz_pts.push(("SRA".into(), a)); } }
        if let Ok(v) = r_lra_pt { for a in v { fhsz_pts.push(("LRA".into(), a)); } }
        if ca_fhsz_checked {
            sources.push(SourceItem::new("CAL FIRE Fire Hazard Severity Zones (SRA 2024 & LRA 2025)", &fhsz_sra));
        } else {
            warnings.push("CAL FIRE Fire Hazard Severity Zone lookup unavailable.".into());
        }

        let mut zoned: Vec<&(String, Value)> = fhsz_pts.iter().filter(|(_, a)| val_num(a.get("FHSZ")).unwrap_or(0.0) >= 1.0).collect();
        zoned.sort_by_key(|(_, a)| -(val_num(a.get("FHSZ")).unwrap_or(0.0) as i64));
        let (fhsz_desc, fhsz_cls, in_fhsz, fhsz_src): (Option<String>, Option<i64>, Option<bool>, Option<&str>) = if !ca_fhsz_checked {
            (None, None, None, None)
        } else if let Some((src, z)) = zoned.first() {
            (
                Some(val_str(z.get("FHSZ_Description")).unwrap_or("Moderate").to_string()),
                Some(val_num(z.get("FHSZ")).unwrap_or(1.0) as i64),
                Some(true),
                Some(if src == "SRA" { "SRA map effective 2024-04-01" } else { "LRA map dated 2025-03-24" }),
            )
        } else {
            (Some("Not in a mapped Fire Hazard Severity Zone".to_string()), Some(0), Some(false), None)
        };

        let mut near_fhsz = Vec::new();
        if let Ok(v) = r_sra_nr { near_fhsz.extend(v); }
        if let Ok(v) = r_lra_nr { near_fhsz.extend(v); }
        let mut best_fhsz: Option<(f64, f64, f64, Value)> = None;
        let mut fhsz_gj = Vec::new();
        for f in near_fhsz.iter().take(24) {
            if let (Some(a), Some(geom)) = (f.get("attributes"), f.get("geometry")) {
                if let Some((dm, nlat, nlon)) = polygon_distance(lat, lon, geom) {
                    if best_fhsz.as_ref().map(|b| dm < b.0).unwrap_or(true) {
                        best_fhsz = Some((dm, nlat, nlon, a.clone()));
                    }
                    if let Some(gj) = clip_and_simplify_arcgis_geometry(geom, lat, lon, 6_000.0, 0.00007, 180) {
                        fhsz_gj.push(json!({
                            "type": "Feature",
                            "geometry": gj,
                            "properties": { "name": val_str(a.get("FHSZ_Description")).unwrap_or("High"), "fhsz": val_num(a.get("FHSZ")) }
                        }));
                    }
                }
            }
        }
        if !fhsz_gj.is_empty() {
            let lyr = json!({ "id": "ca-fhsz", "title": "CAL FIRE High / Very High Fire Hazard Severity Zones", "kind": "polygons", "color": "#f03b20", "features": fhsz_gj });
            layers_map.insert("wildfire".into(), lyr.clone());
            map_layers.push(lyr);
        }
        let nearest_hvh = best_fhsz.map(|(dm, nlat, nlon, a)| json!({
            "class": val_str(a.get("FHSZ_Description")),
            "responsibility": val_str(a.get("SRA")),
            "distM": dm.round() as i64,
            "distMi": round_n(dm / MILE_M, 2),
            "direction": if dm > 0.5 { Some(compass8(bearing_deg(lat, lon, nlat, nlon))) } else { None },
        })).unwrap_or(Value::Null);

        wf_ca = json!({
            "checked": ca_fhsz_checked,
            "responsibilityArea": resp_area,
            "fhsz": fhsz_desc,
            "fhszClass": fhsz_cls,
            "inFhsz": in_fhsz,
            "source": fhsz_src,
            "nearestHighOrVeryHigh": nearest_hvh,
            "searchRadiusM": 5000,
        });

        if let Ok(rows) = r_cgs_rn {
            radon_ca = match rows.first() {
                Some(c) => json!({
                    "potential": val_str(c.get("Rn_ZONE_POTENTIAL")),
                    "description": val_str(c.get("DESCRIPTION_RN_ZONE")).or_else(|| val_str(c.get("DESCRIPTION_Rn_ZONE"))),
                    "study": val_str(c.get("STUDY_AREA")),
                    "report": val_str(c.get("REPORT_NAME")),
                    "reportUrl": val_str(c.get("LINK_REPORT")),
                }),
                None => json!({ "potential": null, "description": "Outside CGS indoor-radon study areas" }),
            };
        }
    }

    // 5. RADON
    let epa_z = epa_radon_zone(county_fips);
    let epa_lbl = epa_z.map(epa_radon_label);
    if epa_z.is_some() {
        sources.push(SourceItem::new("US EPA Map of Radon Zones (1993)", "https://www.epa.gov/radon/epa-map-radon-zones"));
    }
    let geo_radon = match r_grp {
        Ok(rows) => rows.first().map(|g| {
            let grp_n = val_num(g.get("GRP")).map(|v| v.round() as i64);
            let pot = match grp_n { Some(1) => Some("High"), Some(2) => Some("Moderate"), Some(3) => Some("Low"), _ => None };
            json!({ "potential": pot, "grp": grp_n, "radonIndex": val_num(g.get("RI")), "province": val_str(g.get("AREANAME")) })
        }).unwrap_or(Value::Null),
        Err(_) => Value::Null,
    };
    let radon_checked = epa_z.is_some() || !geo_radon.is_null() || !radon_ca.is_null();
    let radon_val = json!({
        "checked": radon_checked,
        "epaZone": epa_z,
        "label": epa_lbl,
        "epaZoneLabel": epa_lbl,
        "actionLevelPciL": 4.0,
        "county": val_str(geo.get("county")),
        "geologic": geo_radon,
        "california": radon_ca,
    });

    let mut data = json!({
        "location": {
            "lat": round_n(lat, 6),
            "lon": round_n(lon, 6),
            "elevationM": elev_m,
            "elevationFt": elev_ft,
            "elevationSource": elev_source,
        },
        "checks": {
            "flood": fl_pt_checked,
            "floodNear": fl_near_checked,
            "nri": nri_checked,
            "asce": asce_checked,
            "faults": faults_checked,
            "whp": whp_checked,
            "elevation": elev_m.is_some(),
            "radon": radon_checked,
            "caEqZones": if is_ca { json!(ca_eq_checked) } else { Value::Null },
            "caFhsz": if is_ca { json!(ca_fhsz_checked) } else { Value::Null },
            "caDam": if is_ca { json!(ca_dam_checked) } else { Value::Null },
        },
        "flood": flood_val,
        "earthquake": {
            "checked": asce_checked || faults_checked || count_m4.is_some() || ca_eq_checked,
            "designMaps": design_maps,
            "recentQuakes": recent_quakes,
            "history": eq_history,
            "nearestFaults": nearest_faults_arr,
            "faults": faults_obj,
            "california": eq_ca,
        },
        "wildfire": {
            "checked": ca_fhsz_checked || whp_checked || !wf_nri.is_null(),
            "california": wf_ca,
            "whp": whp_val,
            "nri": wf_nri,
        },
        "nri": nri_val,
        "radon": radon_val,
        "other": other_val,
    });

    let (scores, score_basis) = compute_scores(&data, &nri_all, is_ca);
    let highlights = build_highlights(&data, is_ca);
    if let Some(obj) = data.as_object_mut() {
        obj.insert("scores".into(), scores);
        obj.insert("scoreBasis".into(), score_basis);
        obj.insert("highlights".into(), Value::Array(highlights));
        if is_ca {
            let nhd = build_nhd(&Value::Object(obj.clone()));
            obj.insert("caNaturalHazardDisclosure".into(), nhd.clone());
            obj.insert("californiaNhd".into(), nhd);
        }
        layers_map.insert("list".into(), Value::Array(map_layers.clone()));
        obj.insert("mapLayers".into(), Value::Array(map_layers));
        obj.insert("layers".into(), Value::Object(layers_map));
    }

    Ok(ModuleOutput {
        data,
        sources,
        warnings,
    })
}
