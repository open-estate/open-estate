// Centralized brand configuration for the browser extension (self-hosted personal research edition).
// Keep all brand strings and theme tokens here so custom theme settings stay centralized.
(function initBrandConfig(root) {
  'use strict';

  const BRAND_CONFIG = Object.freeze({
    id: 'open-estate',
    name: 'Open Estate',
    nameZh: 'Open Estate',
    nameEs: 'Open Estate',
    domain: 'localhost:8080',
    logoGlyph: 'O',
    // Brand mark ("Approval Stamp") shown for the default brand; custom brands fall back to `logoGlyph`.
    logoSvg: '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"> <rect width="64" height="64" rx="14" fill="#ffffff"/> <g transform="rotate(-8 32 32)"> <rect x="11" y="11" width="42" height="42" rx="8.5" fill="none" stroke="#0f766e" stroke-width="4.2"/> <rect x="16.5" y="16.5" width="31" height="31" rx="4.5" fill="none" stroke="#0f766e" stroke-width="1.8"/> <path d="M32 20.5 42.5 29.2V41a2.4 2.4 0 0 1-2.4 2.4H23.9A2.4 2.4 0 0 1 21.5 41V29.2z" fill="#0f766e"/> <path d="M27 35.6l3.6 3.6 6.8-7.2" fill="none" stroke="#ffffff" stroke-width="3" stroke-linecap="round" stroke-linejoin="round"/> </g> </svg>',
    tagline: 'US Residential & CRE Due-Diligence Intelligence',
    taglineZh: '全美住宅与商业地产多维尽调智库',
    taglineEs: 'Inteligencia de diligencia debida residencial y comercial (CRE) en EE. UU.',
    defaultServerUrl: 'http://localhost:8080',
    devFallbackServerUrl: 'http://127.0.0.1:8080',
    accentColor: '#0f766e',
    accentColorSecondary: '#14b8a6',
    accentSoftBg: '#f0fdfa',
    clientIdHeader: 'X-OpenEstate-Client-Id',
    clientIdPrefix: 'oex-',
    storagePrefix: 'open_estate_ext_v1',
    overlayHostId: 'open-estate-intelligence-overlay-host',
    maxDealRoomItems: 10,
    cacheTtlMs: 30 * 60 * 1000,
    aiPendingCacheTtlMs: 20 * 1000,
  });

  function resolveBrandConfig(overrides) {
    if (!overrides || typeof overrides !== 'object') {
      return BRAND_CONFIG;
    }
    const cleanName = typeof overrides.brandName === 'string' && overrides.brandName.trim()
      ? overrides.brandName.trim().slice(0, 40)
      : BRAND_CONFIG.name;
    const cleanNameZh = typeof overrides.brandNameZh === 'string' && overrides.brandNameZh.trim()
      ? overrides.brandNameZh.trim().slice(0, 40)
      : (cleanName !== BRAND_CONFIG.name ? cleanName : BRAND_CONFIG.nameZh);
    const cleanNameEs = typeof overrides.brandNameEs === 'string' && overrides.brandNameEs.trim()
      ? overrides.brandNameEs.trim().slice(0, 40)
      : (cleanName !== BRAND_CONFIG.name ? cleanName : BRAND_CONFIG.nameEs);
    const cleanGlyph = typeof overrides.logoGlyph === 'string' && overrides.logoGlyph.trim()
      ? Array.from(overrides.logoGlyph.trim())[0]
      : (cleanName !== BRAND_CONFIG.name ? Array.from(cleanName)[0].toUpperCase() : BRAND_CONFIG.logoGlyph);
    const cleanAccent = typeof overrides.accentColor === 'string' && /^#[0-9a-fA-F]{6}$/.test(overrides.accentColor.trim())
      ? overrides.accentColor.trim()
      : BRAND_CONFIG.accentColor;

    const hasCustomGlyph = typeof overrides.logoGlyph === 'string' && overrides.logoGlyph.trim() !== '';
    return Object.assign({}, BRAND_CONFIG, {
      name: cleanName,
      nameZh: cleanNameZh,
      nameEs: cleanNameEs,
      logoGlyph: cleanGlyph,
      logoSvg: cleanName === BRAND_CONFIG.name && !hasCustomGlyph ? BRAND_CONFIG.logoSvg : '',
      accentColor: cleanAccent,
    });
  }

  root.BRAND_CONFIG = BRAND_CONFIG;
  root.resolveBrandConfig = resolveBrandConfig;
})(typeof globalThis !== 'undefined' ? globalThis : self);
