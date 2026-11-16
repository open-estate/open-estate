# Facts schema (`report.facts`, also the shape of researched `given_facts`; all fields optional unless noted; cite sources)

| Field | Type | Example | Notes |
|---|---|---|---|
| propertyType | string | "Single-family (detached)" | |
| beds, baths | number | 4, 2 | baths may be fractional (2.5) |
| sqft | number | 1956 | official living area; put unpermitted/extra space in `sqftNote` |
| lotSqft | number | 6300 | |
| lotFrontageFt, lotRearFt, lotDepthFt | number | 70, 57, 95 | from parcel geometry; used by feng shui lot-shape check |
| yearBuilt | number | 1975 | drives flying-star period & age-related diligence |
| stories, garage, hoaMonthly, apn, legal, zoning, foundation, roof, heating, cooling | various | | |
| features | string[] | | |
| virtualTours | [{url, label?}] | [{"url":"https://my.matterport.com/show/?m=…"}] | the listing's published 3D / video tour links (Matterport, Zillow 3D Home, iGUIDE, YouTube …). The report's 3D section plays them through the tour host's own embed; record the link only, never copy tour content. Licensed feeds fill `mlsListing.virtualTourUrl` automatically. |
| lastSale | {date, price} | {"date":"2025-03-05","price":2320000} | |
| saleHistory | [{date, event, price, note?}] | | events: Listed, Price cut, Pending, Sold |
| avms | [{source, value, date, note?}] | Redfin, Movoto, RentCast | date-stamp them |
| rentEstimate, rentNote | number, string | 4589 | monthly |
| taxRate, taxNote | fraction, string | 0.012592 | parcel's tax-rate-area total |
| insuranceAnnual, earthquakeInsuranceAnnual | number | | real quotes override estimates |
| walkScore | {walk, bike, transit, source} | | 0–100 Walk Score values |
| schools | {note, assigned:[{name, grades, distanceMi, ela?, math?, source?, year?, url?}]} | | Assigned schools: name, grades, distance and official state or federal data only (e.g. CAASPP % meeting standard in `ela`/`math`, with `source` and `year`). Never record GreatSchools or Niche ratings; a link to those sites in `url` is fine. Mark "verify with district". |
| localNotes | {noise[], environment[], safety} | | city GIS buffers, advisories, flight tracks |
| permitNotes | string | | summary of municipal building permit status |
| permits | [{number?, date?, type, status?, valuation?, description?}] | | structured building/electrical/roof/ADU permits |
| taxHistory | [{year, tax?, assessed?, land?, improvements?, landPerLotSqft?}] | | County Assessor multi-year roll (for tax administration, not market value) |
| rentalComps | [{address, listedDate?, rent, beds?, baths?, sqft?, distanceMi?, status?, source?}] | | nearby rental/lease comparables; never invented |
| nearbyListings | [{address, status, price, beds?, baths?, sqft?, daysOnMarket?, distanceMi?, source?}] | | active / coming-soon nearby for-sale listings (asking prices) |
| nearbyRentals | [{address, status, rent, beds?, baths?, sqft?, daysOnMarket?, distanceMi?, source?}] | | active nearby for-rent listings |
| buildingRegister | object | computed in `src/property/facts.rs` | APN, legal, assetClass, zoning, footprintSqft, lotCoveragePct (BCR), far (FAR), stories, heightFt, parking, permits, permitPortalUrl |
| officialAssessment | object | computed in `src/property/facts.rs` | latestYear, assessedTotal, assessedLand, assessedImprovements, landPerLotSqft, effectiveTaxRatePct, history[], disclaimer |
| titleAndLiens | object | computed in `src/property/facts.rs` | deedChainCount, ownerOccupied, vestingType, recordedMortgages[], involuntaryLiens[], easementsAndRestrictions[], recorderPortalUrl, disclaimer |
| foreclosure | object | computed in `src/property/facts.rs` | status, inForeclosure, caseNumber, auctionDate, openingBid, nearbyDistressed[], officialSearchLinks[] |
| diligenceExtra | [{cat, title, why}] | | appended to the checklist |
| comps | [{address, soldDate, price, listPrice?, dealType?, assetClass?, beds, baths, sqft, lotSqft, yearBuilt, distanceMi, note?}] | | ≥ 6 recent, same type, ≤ ~0.5 mi; never invented |
| sources | [{name, url, note?}] | | shown in the report |

Write `*Note` fields for conflicts ("MLS 6,300 vs public record 6,299 vs GIS 6,019"). Never fabricate permits, liens, foreclosure filings, assessments, or comps.
