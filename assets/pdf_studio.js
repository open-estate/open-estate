/* PDF Studio (`window.OpenEstatePdfStudio`)
 * Zero-server-storage local viewer (self-hosted fflate + PDF.js 4.4.168 + pure-JS text & scanned JPEG fallback),
 * strict client-side .zip bomb & path traversal guards, normalized [0..1] annotation paint overlay
 * (pan/circle/arrow/rect/pen/pin + PNG export), AI automatic citation pointer & pulsing callout ring,
 * and role-synced /api/ask side chat. Strictly honors AGENTS.md frosted light studio chrome & Data Honesty.
 */
(function () {
  'use strict';

  if (typeof window === 'undefined') return;
  if (window.OpenEstatePdfStudio && window.OpenEstatePdfStudio.__v === 1) return;

  var PDFJS_URL = '/vendor/pdf.min.mjs';
  var PDFJS_WORKER_URL = '/vendor/pdf.worker.min.mjs';
  var PDFJS_VIEWER_CSS_URL = '/vendor/pdf_viewer.min.css';
  var FFLATE_URL = '/vendor/fflate.min.js';
  var SUPPORTED_EXT_RE = /\.(pdf|txt|md|html?|png|jpe?g|webp)$/i;
  var ZIP_EXT_RE = /\.zip$/i;
  var NESTED_ARCHIVE_EXT_RE = /\.(zip|tar|tgz|gz|bz2|xz|7z|rar)$/i;
  var ZIP_MAX_FILES = 40;
  var ZIP_MAX_UNCOMPRESSED_BYTES = 100 * 1024 * 1024; // 100 MB
  var ZIP_MAX_COMPRESSION_RATIO = 50; // 50:1 max compression ratio

  function resolveVendorUrl(relPath) {
    var clean = String(relPath || '');
    if (!clean || /^https?:\/\//i.test(clean) || /^file:\/\//i.test(clean)) return clean;
    try {
      if (typeof window !== 'undefined' && window.location && window.location.origin && /^https?:\/\//i.test(window.location.origin)) {
        return window.location.origin.replace(/\/+$/, '') + (clean.charAt(0) === '/' ? clean : '/' + clean);
      }
    } catch (_) {}
    return clean;
  }
  var STOP_WORDS = {
    the: 1, and: 1, for: 1, with: 1, from: 1, this: 1, that: 1, into: 1, over: 1,
    under: 1, report: 1, section: 1, page: 1, item: 1, issue: 1, defect: 1, review: 1,
    critical: 1, major: 1, moderate: 1, minor: 1, info: 1, informational: 1, disclosure: 1,
    conflict: 1, summary: 1, flagged: 1, detected: 1, note: 1, verify: 1, check: 1,
    are: 1, was: 1, were: 1, has: 1, have: 1, had: 1, not: 1, all: 1, per: 1
  };
  var COLORS = [
    { hex: '#ef4444', en: 'Red', zh: '红色', es: 'Rojo' },
    { hex: '#f59e0b', en: 'Amber', zh: '琥珀', es: 'Ámbar' },
    { hex: '#0f766e', en: 'Teal', zh: '青绿', es: 'Verde azulado' },
    { hex: '#2563eb', en: 'Blue', zh: '蓝色', es: 'Azul' }
  ];

  /* -------------------- Language & Formatting Helpers -------------------- */
  function normLang(raw) {
    var l = String(raw || '').trim().toLowerCase();
    if (!l) return '';
    if (l.indexOf('zh') === 0 || l === 'cn') return 'zh';
    if (l.indexOf('es') === 0) return 'es';
    if (l.indexOf('en') === 0) return 'en';
    return '';
  }

  function curLang() {
    try {
      if (window.OpenEstateCore && typeof window.OpenEstateCore.langCode === 'function') {
        var coreL = normLang(window.OpenEstateCore.langCode());
        if (coreL) return coreL;
      }
    } catch (_) {}
    try {
      var onBtn = document.querySelector('#langSwitch button.on[data-lang]');
      if (onBtn) {
        var btnL = normLang(onBtn.getAttribute('data-lang'));
        if (btnL) return btnL;
      }
    } catch (_) {}
    try {
      var docL = normLang(document.documentElement.getAttribute('lang') || document.documentElement.lang);
      if (docL) return docL;
    } catch (_) {}
    try {
      var lsL = normLang(localStorage.getItem('open_estate_lang'));
      if (lsL) return lsL;
    } catch (_) {}
    return 'en';
  }

  function L(en, zh, es) {
    var lg = curLang();
    if (lg === 'zh') return zh || en;
    if (lg === 'es') return es || en;
    return en;
  }

  function brandName() {
    try {
      if (window.__OPEN_ESTATE_BRAND__ && window.__OPEN_ESTATE_BRAND__.name) {
        var bn = String(window.__OPEN_ESTATE_BRAND__.name).trim();
        if (bn) return bn;
      }
      if (window.OPEN_ESTATE_BRAND && window.OPEN_ESTATE_BRAND.name) {
        var bn2 = String(window.OPEN_ESTATE_BRAND.name).trim();
        if (bn2) return bn2;
      }
    } catch (_) {}
    return 'Open' + ' Estate';
  }

  function esc(val) {
    return String(val == null ? '' : val)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;')
      .replace(/'/g, '&#39;');
  }

  function fmtUsd(v) {
    var n = Number(v);
    if (v == null || v === '' || !isFinite(n)) return '';
    return '$' + Math.round(n).toLocaleString('en-US');
  }

  function getClientId() {
    try {
      var k = 'openestate_client_id';
      var v = localStorage.getItem(k);
      if (!v || v.length < 8) {
        v = 'oe_' + Math.random().toString(36).slice(2, 12) + Date.now().toString(36);
        localStorage.setItem(k, v);
      }
      return v;
    } catch (_) {
      return 'oe_session_ephemeral';
    }
  }

  function getActiveRole() {
    try {
      if (window.OpenEstateRole && typeof window.OpenEstateRole.getRole === 'function') {
        var wr = String(window.OpenEstateRole.getRole() || '').trim().toLowerCase();
        if (wr === 'buyer' || wr === 'seller' || wr === 'agent') return wr;
      }
    } catch (_) {}
    try {
      var lr = String(localStorage.getItem('openestate:role_lens:v1') || '').trim().toLowerCase();
      if (lr === 'buyer' || lr === 'seller' || lr === 'agent') return lr;
    } catch (_) {}
    return 'all';
  }

  function setActiveRole(nextRole) {
    var r = (nextRole === 'buyer' || nextRole === 'seller' || nextRole === 'agent') ? nextRole : 'all';
    try {
      if (window.OpenEstateRole && typeof window.OpenEstateRole.setRole === 'function') {
        window.OpenEstateRole.setRole(r);
        return r;
      }
    } catch (_) {}
    try {
      localStorage.setItem('openestate:role_lens:v1', r);
      document.documentElement.setAttribute('data-role', r);
      if (typeof CustomEvent === 'function') {
        window.dispatchEvent(new CustomEvent('openestate:rolechange', { detail: { role: r } }));
      }
    } catch (_) {}
    return r;
  }

  function roleDisplayLabel(r) {
    if (r === 'buyer') return L('Buyer', '买家', 'Comprador');
    if (r === 'seller') return L('Seller', '卖家', 'Vendedor');
    if (r === 'agent') return L('Agent', '经纪人', 'Agente');
    return L('Choose Role', '选择视角', 'Elegir Rol');
  }

  function mimeFromName(name) {
    var low = String(name || '').toLowerCase();
    if (/\.pdf$/.test(low)) return 'application/pdf';
    if (/\.md$/.test(low)) return 'text/markdown';
    if (/\.html?$/.test(low)) return 'text/html';
    if (/\.png$/.test(low)) return 'image/png';
    if (/\.jpe?g$/.test(low)) return 'image/jpeg';
    if (/\.webp$/.test(low)) return 'image/webp';
    return 'text/plain';
  }

  function baseName(path) {
    var s = String(path || '').replace(/\\/g, '/');
    var idx = s.lastIndexOf('/');
    return idx >= 0 ? s.slice(idx + 1) : s;
  }

  function bytesToText(u8) {
    try {
      return new TextDecoder('utf-8', { fatal: false }).decode(u8);
    } catch (_) {
      var out = '';
      for (var i = 0; i < u8.length; i++) out += String.fromCharCode(u8[i]);
      return out;
    }
  }

  function base64ToBytes(b64) {
    var bin = atob(String(b64 || ''));
    var out = new Uint8Array(bin.length);
    for (var i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
    return out;
  }

  function toUint8Array(raw) {
    if (!raw) return new Uint8Array(0);
    if (raw instanceof Uint8Array) return raw;
    if (raw instanceof ArrayBuffer) return new Uint8Array(raw);
    if (ArrayBuffer.isView(raw)) return new Uint8Array(raw.buffer, raw.byteOffset, raw.byteLength);
    if (Array.isArray(raw)) return new Uint8Array(raw);
    return new Uint8Array(0);
  }

  /* -------------------- Pure-JS In-Browser PKZIP Reader -------------------- */
  function concatChunks(chunks, totalLen) {
    var out = new Uint8Array(totalLen);
    var offset = 0;
    for (var i = 0; i < chunks.length; i++) {
      out.set(chunks[i], offset);
      offset += chunks[i].length;
    }
    return out;
  }

  function makeZipSafetyError(code, message) {
    var err = new Error(message);
    err.code = code;
    err.isZipSafetyError = true;
    return err;
  }

  async function decompressWithStream(compressed, format, maxBytes) {
    if (typeof DecompressionStream === 'undefined') {
      throw new Error('DecompressionStream unavailable');
    }
    var limit = (typeof maxBytes === 'number' && maxBytes > 0) ? maxBytes : ZIP_MAX_UNCOMPRESSED_BYTES;
    var ds = new DecompressionStream(format);
    var writer = ds.writable.getWriter();
    var reader = ds.readable.getReader();
    var chunks = [];
    var total = 0;
    var readPromise = (async function () {
      while (true) {
        var step = await reader.read();
        if (step.done) break;
        if (step.value && step.value.length) {
          total += step.value.length;
          if (total > limit) {
            try { reader.cancel(); } catch (_) {}
            throw makeZipSafetyError(
              'ZIP_BOMB_DETECTED',
              L(
                'Archive rejected: decompressed content exceeds safety size or 50:1 compression ratio limit.',
                '压缩包已被拒绝：解压后数据超出安全体积或 50:1 压缩比上限（疑似压缩炸弹）。',
                'Archivo rechazado: el contenido descomprimido supera el límite de tamaño o proporción 50:1.'
              )
            );
          }
          chunks.push(step.value);
        }
      }
      return concatChunks(chunks, total);
    })();
    writer.write(compressed).catch(function () {});
    writer.close().catch(function () {});
    return await readPromise;
  }

  function decompressWithFflateBounded(fflate, compressed, maxBytes) {
    var limit = (typeof maxBytes === 'number' && maxBytes > 0) ? maxBytes : ZIP_MAX_UNCOMPRESSED_BYTES;
    if (fflate && typeof fflate.Inflate === 'function') {
      var chunks = [];
      var total = 0;
      var inflater = new fflate.Inflate();
      inflater.ondata = function (chunk) {
        if (!chunk || !chunk.length) return;
        total += chunk.length;
        if (total > limit) {
          throw makeZipSafetyError(
            'ZIP_BOMB_DETECTED',
            L(
              'Archive rejected: decompressed content exceeds safety size or 50:1 compression ratio limit.',
              '压缩包已被拒绝：解压后数据超出安全体积或 50:1 压缩比上限（疑似压缩炸弹）。',
              'Archivo rechazado: el contenido descomprimido supera el límite de tamaño o proporción 50:1.'
            )
          );
        }
        chunks.push(chunk);
      };
      var stepSize = 4096;
      if (!compressed.length) {
        inflater.push(compressed, true);
      } else {
        for (var off = 0; off < compressed.length; off += stepSize) {
          var end = Math.min(compressed.length, off + stepSize);
          inflater.push(compressed.subarray(off, end), end >= compressed.length);
        }
      }
      return concatChunks(chunks, total);
    }
    if (fflate && typeof fflate.inflateSync === 'function') {
      var out = fflate.inflateSync(compressed);
      if (out.length > limit) {
        throw makeZipSafetyError(
          'ZIP_BOMB_DETECTED',
          L(
            'Archive rejected: decompressed content exceeds safety size or 50:1 compression ratio limit.',
            '压缩包已被拒绝：解压后数据超出安全体积或 50:1 压缩比上限（疑似压缩炸弹）。',
            'Archivo rechazado: el contenido descomprimido supera el límite de tamaño o proporción 50:1.'
          )
        );
      }
      return out;
    }
    return null;
  }

  function decodeZipFileName(nameBytes) {
    try {
      return new TextDecoder('utf-8', { fatal: false }).decode(nameBytes);
    } catch (_) {
      var s = '';
      for (var i = 0; i < nameBytes.length; i++) s += String.fromCharCode(nameBytes[i]);
      return s;
    }
  }

  function isIgnoredZipPath(path) {
    var norm = String(path || '').replace(/\\/g, '/');
    if (!norm || norm.charAt(norm.length - 1) === '/') return true;
    if (norm.indexOf('__MACOSX/') === 0 || norm.indexOf('/__MACOSX/') >= 0) return true;
    var bn = baseName(norm);
    if (!bn || bn.indexOf('._') === 0 || bn === '.DS_Store' || bn === 'Thumbs.db') return true;
    return false;
  }

  function validateZipEntryPath(rawPath) {
    var p = String(rawPath || '');
    if (!p) return;
    if (/[\x00-\x1f]/.test(p) || p.charAt(0) === '/' || p.charAt(0) === '\\' || /^[A-Za-z]:/.test(p)) {
      throw makeZipSafetyError(
        'UNSAFE_ARCHIVE_ENTRY',
        L(
          'Archive rejected: unsafe absolute or control-character file path inside .zip packet.',
          '压缩包已被拒绝：包含不安全的绝对路径或控制字符条目。',
          'Archivo rechazado: ruta absoluta o carácter de control inseguro en el paquete .zip.'
        )
      );
    }
    var segs = p.split(/[\\/]+/);
    for (var i = 0; i < segs.length; i++) {
      if (segs[i] === '..') {
        throw makeZipSafetyError(
          'UNSAFE_ARCHIVE_ENTRY',
          L(
            'Archive rejected: path traversal ("..") entry detected inside .zip packet.',
            '压缩包已被拒绝：检测到不安全的路径穿越（“..”）条目。',
            'Archivo rechazado: se detectó salto de directorio ("..") en el paquete .zip.'
          )
        );
      }
    }
    if (!isIgnoredZipPath(p) && NESTED_ARCHIVE_EXT_RE.test(p)) {
      throw makeZipSafetyError(
        'NESTED_ARCHIVE_PROHIBITED',
        L(
          'Archive rejected: nested archives (.zip/.tar/.gz) inside .zip packets are prohibited.',
          '压缩包已被拒绝：禁止在 .zip 披露包内嵌套其他压缩文件（.zip/.tar/.gz）。',
          'Archivo rechazado: no se permiten archivos comprimidos anidados (.zip/.tar/.gz).'
        )
      );
    }
  }

  var _fflatePromise = null;
  var _fflateFailed = false;
  var _fflateLoaded = false;

  async function loadFflate() {
    try {
      if (window.__OPEN_ESTATE_FFLATE_OVERRIDE__) {
        _fflateLoaded = true;
        return window.__OPEN_ESTATE_FFLATE_OVERRIDE__;
      }
      if (window.fflate && (typeof window.fflate.unzipSync === 'function' || typeof window.fflate.Inflate === 'function')) {
        _fflateLoaded = true;
        return window.fflate;
      }
    } catch (_) {}
    if (_fflateFailed) return null;
    if (_fflatePromise) return _fflatePromise;
    _fflatePromise = new Promise(function (resolve) {
      var settled = false;
      var timer = setTimeout(function () {
        if (!settled) {
          settled = true;
          _fflateFailed = true;
          resolve(null);
        }
      }, 3500);
      import(resolveVendorUrl(FFLATE_URL))
        .then(function (mod) {
          if (settled) return;
          settled = true;
          clearTimeout(timer);
          var winFflate = null;
          try { winFflate = window.fflate || (typeof globalThis !== 'undefined' ? globalThis.fflate : null); } catch (_) {}
          var resolved = (mod && (typeof mod.unzipSync === 'function' || typeof mod.Inflate === 'function')) ? mod
            : (mod && mod.default && (typeof mod.default.unzipSync === 'function' || typeof mod.default.Inflate === 'function')) ? mod.default
            : (winFflate && (typeof winFflate.unzipSync === 'function' || typeof winFflate.Inflate === 'function')) ? winFflate
            : null;
          if (resolved) {
            _fflateLoaded = true;
            resolve(resolved);
          } else {
            _fflateFailed = true;
            resolve(null);
          }
        })
        .catch(function () {
          if (settled) return;
          settled = true;
          clearTimeout(timer);
          _fflateFailed = true;
          resolve(null);
        });
    });
    return _fflatePromise;
  }

  function scanAndValidatePkzipHeaders(bytes) {
    var len = bytes.length;
    var view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    var minOff = Math.max(0, len - 65557);
    var eocdOff = -1;
    for (var i = len - 22; i >= minOff; i--) {
      if (
        bytes[i] === 0x50 &&
        bytes[i + 1] === 0x4b &&
        bytes[i + 2] === 0x05 &&
        bytes[i + 3] === 0x06
      ) {
        eocdOff = i;
        break;
      }
    }

    var entries = [];
    var nonIgnoredCount = 0;
    var declaredUncompTotal = 0;

    function checkEntryLimits(fullPath, compSize, uncompSize) {
      validateZipEntryPath(fullPath);
      if (isIgnoredZipPath(fullPath)) return false;
      nonIgnoredCount += 1;
      if (nonIgnoredCount > ZIP_MAX_FILES) {
        throw makeZipSafetyError(
          'TOO_MANY_ARCHIVE_FILES',
          L(
            'Archive rejected: exceeds maximum limit of 40 files per .zip packet.',
            '压缩包已被拒绝：单次 .zip 包内文件数量超过 40 个安全上限。',
            'Archivo rechazado: supera el límite máximo de 40 archivos por paquete .zip.'
          )
        );
      }
      declaredUncompTotal += uncompSize;
      if (declaredUncompTotal > ZIP_MAX_UNCOMPRESSED_BYTES) {
        throw makeZipSafetyError(
          'ZIP_BOMB_DETECTED',
          L(
            'Archive rejected: total uncompressed size exceeds 100 MB safety limit.',
            '压缩包已被拒绝：解压后总体积超过 100 MB 安全上限。',
            'Archivo rechazado: el tamaño descomprimido total supera el límite de 100 MB.'
          )
        );
      }
      if (uncompSize > 1024 && (compSize === 0 || (uncompSize / compSize) > ZIP_MAX_COMPRESSION_RATIO)) {
        throw makeZipSafetyError(
          'ZIP_BOMB_DETECTED',
          L(
            'Archive rejected: compression ratio exceeds 50:1 zip-bomb safety limit.',
            '压缩包已被拒绝：压缩比超过 50:1 安全阈值（疑似压缩炸弹）。',
            'Archivo rechazado: la relación de compresión supera el límite de seguridad 50:1.'
          )
        );
      }
      return true;
    }

    if (eocdOff >= 0) {
      var totalEntries = view.getUint16(eocdOff + 10, true);
      var cdSize = view.getUint32(eocdOff + 12, true);
      var cdOffset = view.getUint32(eocdOff + 16, true);
      var pos = cdOffset;
      var cdEnd = Math.min(len, cdOffset + cdSize);
      for (var idx = 0; idx < totalEntries && pos + 46 <= cdEnd; idx++) {
        var sig = view.getUint32(pos, true);
        if (sig !== 0x02014b50) break;
        var gpFlag = view.getUint16(pos + 8, true);
        var method = view.getUint16(pos + 10, true);
        var compSize = view.getUint32(pos + 20, true);
        var uncompSize = view.getUint32(pos + 24, true);
        var fnameLen = view.getUint16(pos + 28, true);
        var extraLen = view.getUint16(pos + 30, true);
        var commentLen = view.getUint16(pos + 32, true);
        var lhOff = view.getUint32(pos + 42, true);
        var nameBytes = bytes.subarray(pos + 46, pos + 46 + fnameLen);
        var fullPath = decodeZipFileName(nameBytes);
        pos += 46 + fnameLen + extraLen + commentLen;

        if (!checkEntryLimits(fullPath, compSize, uncompSize)) continue;
        if (!SUPPORTED_EXT_RE.test(fullPath)) continue;
        if ((gpFlag & 0x0001) !== 0) continue;
        if (lhOff + 30 > len || view.getUint32(lhOff, true) !== 0x04034b50) continue;

        var lhNameLen = view.getUint16(lhOff + 26, true);
        var lhExtraLen = view.getUint16(lhOff + 28, true);
        var dataStart = lhOff + 30 + lhNameLen + lhExtraLen;
        var dataEnd = Math.min(len, dataStart + compSize);
        if (dataStart > len) continue;
        entries.push({
          fullPath: fullPath,
          method: method,
          compressed: bytes.subarray(dataStart, dataEnd),
          compSize: compSize,
          uncompSize: uncompSize
        });
      }
    } else {
      var p = 0;
      while (p + 30 <= len) {
        if (view.getUint32(p, true) !== 0x04034b50) break;
        var fGp = view.getUint16(p + 6, true);
        var fMethod = view.getUint16(p + 8, true);
        var fComp = view.getUint32(p + 18, true);
        var fUncomp = view.getUint32(p + 22, true);
        var fNameL = view.getUint16(p + 26, true);
        var fExtraL = view.getUint16(p + 28, true);
        var fName = decodeZipFileName(bytes.subarray(p + 30, p + 30 + fNameL));
        var dStart = p + 30 + fNameL + fExtraL;
        if (fComp === 0 && (fGp & 0x0008) !== 0) break;
        var dEnd = Math.min(len, dStart + fComp);
        var keep = checkEntryLimits(fName, fComp, fUncomp);
        if (keep && SUPPORTED_EXT_RE.test(fName) && (fGp & 0x0001) === 0) {
          entries.push({
            fullPath: fName,
            method: fMethod,
            compressed: bytes.subarray(dStart, dEnd),
            compSize: fComp,
            uncompSize: fUncomp
          });
        }
        p = dEnd;
      }
    }

    if (declaredUncompTotal > 1024 && (declaredUncompTotal / Math.max(1, len)) > ZIP_MAX_COMPRESSION_RATIO) {
      throw makeZipSafetyError(
        'ZIP_BOMB_DETECTED',
        L(
          'Archive rejected: overall compression ratio exceeds 50:1 zip-bomb safety limit.',
          '压缩包已被拒绝：整体压缩比超过 50:1 安全阈值（疑似压缩炸弹）。',
          'Archivo rechazado: la relación de compresión total supera el límite 50:1.'
        )
      );
    }

    return entries;
  }

  async function unpackZipBytes(zipBytes, zipName) {
    var bytes = toUint8Array(zipBytes);
    var len = bytes.length;
    if (len < 22) return [];

    // 1. Strictly validate path safety, nested archives, <=40 files, <=100MB, and <=50:1 ratio before decompressing
    var entries = scanAndValidatePkzipHeaders(bytes);
    var fflate = await loadFflate();

    // 2. Decompress each validated entry with streaming byte & ratio bounds (protects against spoofed ZIP headers)
    var unpacked = [];
    var actualUncompTotal = 0;
    for (var j = 0; j < entries.length; j++) {
      var ent = entries[j];
      var remainingBudget = ZIP_MAX_UNCOMPRESSED_BYTES - actualUncompTotal;
      if (remainingBudget <= 0) {
        throw makeZipSafetyError(
          'ZIP_BOMB_DETECTED',
          L(
            'Archive rejected: total uncompressed size exceeds 100 MB safety limit.',
            '压缩包已被拒绝：解压后总体积超过 100 MB 安全上限。',
            'Archivo rechazado: el tamaño descomprimido total supera el límite de 100 MB.'
          )
        );
      }
      var entryMaxBytes = Math.min(remainingBudget, Math.max(1024, Math.max(1, ent.compSize) * ZIP_MAX_COMPRESSION_RATIO));
      var rawBytes = null;

      if (ent.method === 0) {
        if (ent.compressed.length > entryMaxBytes) {
          throw makeZipSafetyError('ZIP_BOMB_DETECTED', 'Archive rejected: entry exceeds uncompressed size safety limit.');
        }
        rawBytes = ent.compressed.slice();
      } else if (ent.method === 8) {
        if (fflate && (typeof fflate.Inflate === 'function' || typeof fflate.inflateSync === 'function')) {
          rawBytes = decompressWithFflateBounded(fflate, ent.compressed, entryMaxBytes);
        } else {
          rawBytes = await decompressWithStream(ent.compressed, 'deflate-raw', entryMaxBytes);
        }
      } else {
        continue;
      }

      if (!rawBytes) continue;
      actualUncompTotal += rawBytes.length;
      if (actualUncompTotal > ZIP_MAX_UNCOMPRESSED_BYTES ||
          (actualUncompTotal > 1024 && (actualUncompTotal / Math.max(1, len)) > ZIP_MAX_COMPRESSION_RATIO)) {
        throw makeZipSafetyError(
          'ZIP_BOMB_DETECTED',
          L(
            'Archive rejected: decompressed content exceeds 100 MB or 50:1 compression ratio limit.',
            '压缩包已被拒绝：实际解压数据超出 100 MB 或 50:1 压缩比安全上限。',
            'Archivo rechazado: los datos descomprimidos superan el límite de 100 MB o proporción 50:1.'
          )
        );
      }

      var cleanName = baseName(ent.fullPath);
      unpacked.push({
        name: cleanName,
        fullPath: ent.fullPath,
        mime: mimeFromName(cleanName),
        bytes: rawBytes,
        size: rawBytes.length,
        sourceZipName: zipName || ''
      });
    }
    return unpacked;
  }

  /* -------------------- PDF.js Loader & Offline Fallback Parser -------------------- */
  var _pdfjsFailed = false;

  async function loadPdfJs() {
    try {
      if (window.__OPEN_ESTATE_PDFJS_OVERRIDE__) {
        return window.__OPEN_ESTATE_PDFJS_OVERRIDE__;
      }
    } catch (_) {}
    if (_pdfjsFailed) return null;
    var sharedPromise = null;
    try {
      sharedPromise = window.__openEstatePdfJsPromise || null;
    } catch (_) {}
    if (!sharedPromise) {
      var pdfJsSrc = resolveVendorUrl(PDFJS_URL);
      var workerSrc = resolveVendorUrl(PDFJS_WORKER_URL);
      sharedPromise = import(pdfJsSrc).then(function (mod) {
        var resolved = (mod && mod.getDocument) ? mod : (mod && mod.default && mod.default.getDocument ? mod.default : mod);
        if (resolved && resolved.GlobalWorkerOptions) {
          resolved.GlobalWorkerOptions.workerSrc = workerSrc;
        }
        return resolved && resolved.getDocument ? resolved : null;
      });
      try {
        window.__openEstatePdfJsPromise = sharedPromise;
      } catch (_) {}
    }

    return await new Promise(function (resolve) {
      var settled = false;
      var timer = setTimeout(function () {
        if (!settled) {
          settled = true;
          _pdfjsFailed = true;
          resolve(null);
        }
      }, 4500);
      Promise.resolve(sharedPromise)
        .then(function (resolved) {
          if (settled) return;
          settled = true;
          clearTimeout(timer);
          if (resolved && resolved.getDocument) {
            if (resolved.GlobalWorkerOptions && !resolved.GlobalWorkerOptions.workerSrc) {
              resolved.GlobalWorkerOptions.workerSrc = resolveVendorUrl(PDFJS_WORKER_URL);
            }
            resolve(resolved);
          } else {
            _pdfjsFailed = true;
            resolve(null);
          }
        })
        .catch(function () {
          if (settled) return;
          settled = true;
          clearTimeout(timer);
          _pdfjsFailed = true;
          resolve(null);
        });
    });
  }

  function decodePdfLiteralString(raw) {
    var out = '';
    for (var i = 0; i < raw.length; i++) {
      var ch = raw.charAt(i);
      if (ch === '\\' && i + 1 < raw.length) {
        var nx = raw.charAt(i + 1);
        if (nx === 'n') { out += '\n'; i++; }
        else if (nx === 'r') { out += ' '; i++; }
        else if (nx === 't') { out += ' '; i++; }
        else if (nx === '\r') {
          if (i + 2 < raw.length && raw.charAt(i + 2) === '\n') i += 2;
          else i += 1;
        }
        else if (nx === '\n') { i += 1; }
        else if (nx === 'b' || nx === 'f') { i++; }
        else if (nx === '(' || nx === ')' || nx === '\\') { out += nx; i++; }
        else if (/[0-7]/.test(nx)) {
          var oct = raw.slice(i + 1, i + 4).match(/^[0-7]{1,3}/)[0];
          out += String.fromCharCode(parseInt(oct, 8));
          i += oct.length;
        } else {
          out += nx;
          i++;
        }
      } else {
        out += ch;
      }
    }
    return out;
  }

  function decodePdfHexString(hexRaw) {
    var clean = String(hexRaw || '').replace(/\s+/g, '');
    if (!clean) return '';
    if (clean.length % 2 === 1) clean += '0';
    if (/^feff/i.test(clean) && clean.length >= 8) {
      var u16 = '';
      for (var u = 4; u + 3 < clean.length; u += 4) {
        u16 += String.fromCharCode(parseInt(clean.slice(u, u + 4), 16));
      }
      return u16;
    }
    var out = '';
    for (var i = 0; i + 1 < clean.length; i += 2) {
      var code = parseInt(clean.slice(i, i + 2), 16);
      if (code >= 32 || code === 9 || code === 10) out += String.fromCharCode(code);
    }
    return out;
  }

  function appendPdfTextChunk(curLine, chunk) {
    var piece = String(chunk || '');
    if (!piece) return curLine;
    if (!curLine) return piece;
    if (/[\s\-(\[{/]$/.test(curLine) || /^[\s,.;:!?)\]}]/.test(piece)) {
      return curLine + piece;
    }
    return curLine + ' ' + piece;
  }

  function decodeTjArrayTokens(inner) {
    var tokenRe = /\((?:\\.|[^\\()])*\)|<([0-9A-Fa-f\s]+)>|(-?\d+(?:\.\d+)?)/g;
    var m;
    var out = '';
    while ((m = tokenRe.exec(inner)) !== null) {
      var tok = m[0];
      if (tok.charAt(0) === '(') {
        out += decodePdfLiteralString(tok.slice(1, -1));
      } else if (tok.charAt(0) === '<') {
        out += decodePdfHexString(m[1] || '');
      } else if (m[2] !== undefined) {
        var kern = parseFloat(m[2]);
        // In PDF TJ arrays, negative numbers advance rightward in thousandths of text space; <= -120 marks word space
        if (isFinite(kern) && kern <= -120 && out && !/\s$/.test(out)) {
          out += ' ';
        }
      }
    }
    return out;
  }

  function pushDecodedPdfLines(lines, rawLine) {
    var parts = String(rawLine || '').split(/\n+/);
    for (var i = 0; i < parts.length; i++) {
      var trimmed = parts[i].replace(/\s{2,}/g, ' ').trim();
      if (trimmed) lines.push(trimmed);
    }
  }

  function extractTextFromPdfStream(streamText) {
    var lines = [];
    var btRe = /BT([\s\S]*?)ET/g;
    var bm;
    var foundBt = false;
    while ((bm = btRe.exec(streamText)) !== null) {
      foundBt = true;
      var block = bm[1];
      // Match text-showing & text-positioning operators in exact stream order (note: T* has no trailing \b!)
      var tokRe = /\((?:\\.|[^\\()])*\)\s*(?:Tj|'|")|<([0-9A-Fa-f\s]+)>\s*Tj\b|\[((?:[^\[\]]|\((?:\\.|[^\\()])*\)|<[0-9A-Fa-f\s]*>)*)\]\s*TJ\b|(-?\d+(?:\.\d+)?)\s+(-?\d+(?:\.\d+)?)\s+T[dD]\b|(?:(-?\d+(?:\.\d+)?)\s+){5}(-?\d+(?:\.\d+)?)\s+Tm\b|T\*(?=$|[\s\[\]()<>\/])/g;
      var tm;
      var curLine = '';
      var lastTmY = null;
      while ((tm = tokRe.exec(block)) !== null) {
        var full = tm[0];
        if (/^T\*/.test(full)) {
          if (curLine.trim()) pushDecodedPdfLines(lines, curLine);
          curLine = '';
        } else if (/\sT[dD]$/.test(full)) {
          var tx = parseFloat(tm[3]);
          var ty = parseFloat(tm[4]);
          if (!isFinite(ty) || Math.abs(ty) > 0.5) {
            if (curLine.trim()) pushDecodedPdfLines(lines, curLine);
            curLine = '';
          } else if (isFinite(tx) && Math.abs(tx) > 0.5 && curLine && !/\s$/.test(curLine)) {
            curLine += ' ';
          }
        } else if (/\sTm$/.test(full)) {
          var tmY = parseFloat(tm[6]);
          if (lastTmY === null || !isFinite(tmY) || Math.abs(tmY - lastTmY) > 1.5) {
            if (curLine.trim()) pushDecodedPdfLines(lines, curLine);
            curLine = '';
          } else if (curLine && !/\s$/.test(curLine)) {
            curLine += ' ';
          }
          if (isFinite(tmY)) lastTmY = tmY;
        } else if (full.charAt(0) === '[') {
          var arrStr = decodeTjArrayTokens(tm[2] || '');
          curLine = appendPdfTextChunk(curLine, arrStr);
        } else if (full.charAt(0) === '<') {
          var hexStr = decodePdfHexString(tm[1] || '');
          curLine = appendPdfTextChunk(curLine, hexStr);
        } else {
          var isNewLineShow = /['"]$/.test(full);
          var lit = full.replace(/\)\s*(?:Tj|'|")$/, '').slice(1);
          var decoded = decodePdfLiteralString(lit);
          if (isNewLineShow) {
            if (curLine.trim()) pushDecodedPdfLines(lines, curLine);
            curLine = '';
            if (decoded.trim()) pushDecodedPdfLines(lines, decoded);
          } else {
            curLine = appendPdfTextChunk(curLine, decoded);
          }
        }
      }
      if (curLine.trim()) pushDecodedPdfLines(lines, curLine);
    }
    if (!foundBt) {
      var anyLitRe = /\((?:\\.|[^\\()]){2,}\)\s*(?:Tj|'|")/g;
      var am;
      while ((am = anyLitRe.exec(streamText)) !== null) {
        var cleaned = decodePdfLiteralString(am[0].replace(/\)\s*(?:Tj|'|")$/, '').slice(1)).trim();
        if (cleaned) pushDecodedPdfLines(lines, cleaned);
      }
    }
    return lines;
  }

  function wrapLineOnWords(rawLine, maxChars) {
    var limit = Math.max(24, maxChars || 88);
    var line = String(rawLine == null ? '' : rawLine).replace(/\r/g, '');
    if (!line.trim()) return [''];
    if (line.length <= limit) return [line];

    var words = line.trim().split(/\s+/);
    var wrapped = [];
    var cur = '';
    for (var i = 0; i < words.length; i++) {
      var w = words[i];
      if (!cur) {
        if (w.length <= limit) {
          cur = w;
        } else {
          for (var pos = 0; pos < w.length; pos += limit) {
            var slice = w.slice(pos, pos + limit);
            if (pos + limit < w.length) wrapped.push(slice);
            else cur = slice;
          }
        }
      } else if (cur.length + 1 + w.length <= limit) {
        cur += ' ' + w;
      } else {
        wrapped.push(cur);
        cur = '';
        i--; // re-process word `w` on fresh line
      }
    }
    if (cur) wrapped.push(cur);
    return wrapped.length ? wrapped : [''];
  }

  function loadImageFromBytes(imgBytes, mimeType) {
    return new Promise(function (resolve) {
      if (typeof Image === 'undefined' || typeof Blob === 'undefined' || typeof URL === 'undefined' || typeof URL.createObjectURL !== 'function') {
        resolve(null);
        return;
      }
      try {
        var blob = new Blob([imgBytes], { type: mimeType || 'image/jpeg' });
        var url = URL.createObjectURL(blob);
        var img = new Image();
        img.onload = function () {
          try { URL.revokeObjectURL(url); } catch (_) {}
          resolve(img);
        };
        img.onerror = function () {
          try { URL.revokeObjectURL(url); } catch (_) {}
          resolve(null);
        };
        img.src = url;
      } catch (_) {
        resolve(null);
      }
    });
  }

  async function parsePdfFallbackDetail(pdfBytes, docName) {
    var bytes = toUint8Array(pdfBytes);
    // Strict 1:1 byte-to-UTF16-code-unit string (0x00..0xFF) so stream byte offsets match `bytes`
    var latinChunks = [];
    for (var bi = 0; bi < bytes.length; bi += 16384) {
      latinChunks.push(String.fromCharCode.apply(null, bytes.subarray(bi, Math.min(bytes.length, bi + 16384))));
    }
    var latin = latinChunks.join('');

    var pageStreams = [];
    var embeddedJpegs = [];
    var streamRe = /stream\r?\n([\s\S]*?)\r?\n?endstream/g;
    var m;
    while ((m = streamRe.exec(latin)) !== null) {
      var dictHead = latin.slice(Math.max(0, m.index - 800), m.index);
      var rawStream = m[1];
      var headerEnd = m[0].indexOf('\n') + 1;
      var byteStart = m.index + headerEnd;
      var lenMatch = dictHead.match(/\/Length\s+(\d+)(?!\s+\d+\s+R)/);
      var declaredLen = lenMatch ? parseInt(lenMatch[1], 10) : -1;
      var byteEnd = (declaredLen >= 0 && byteStart + declaredLen <= bytes.length)
        ? (byteStart + declaredLen)
        : (byteStart + rawStream.length);
      var sb = bytes.subarray(byteStart, byteEnd);

      var isImageDict = /\/Subtype\s*\/Image\b/.test(dictHead);
      var hasDct = /\/DCTDecode\b/.test(dictHead);
      var hasFlate = /\/FlateDecode\b/.test(dictHead);

      if (hasDct || (isImageDict && sb.length > 4 && sb[0] === 0xff && sb[1] === 0xd8)) {
        var jpegBytes = sb;
        if (hasFlate && !(sb[0] === 0xff && sb[1] === 0xd8)) {
          try {
            jpegBytes = await decompressWithStream(sb, 'deflate', 32 * 1024 * 1024);
          } catch (_) {
            try {
              jpegBytes = await decompressWithStream(sb, 'deflate-raw', 32 * 1024 * 1024);
            } catch (__) {
              jpegBytes = null;
            }
          }
        }
        if (jpegBytes && jpegBytes.length > 4 && jpegBytes[0] === 0xff && jpegBytes[1] === 0xd8) {
          embeddedJpegs.push(jpegBytes.slice());
        }
        continue;
      }

      if (isImageDict || /\/Subtype\s*\/(?:Type1C|CIDFontType0C|OpenType)\b/.test(dictHead) || /\/Length1\b/.test(dictHead)) {
        continue;
      }

      var textChunk = rawStream;
      if (hasFlate) {
        var dec = null;
        try {
          dec = await decompressWithStream(sb, 'deflate', 32 * 1024 * 1024);
        } catch (_) {
          try {
            dec = await decompressWithStream(sb, 'deflate-raw', 32 * 1024 * 1024);
          } catch (__) {
            dec = null;
          }
        }
        textChunk = dec ? bytesToText(dec) : '';
      }
      var lines = extractTextFromPdfStream(textChunk);
      if (lines.length) pageStreams.push(lines);
    }

    var countMatch = latin.match(/\/Type\s*\/Pages[\s\S]{0,160}?\/Count\s+(\d+)/);
    var declaredPages = countMatch ? parseInt(countMatch[1], 10) : 0;

    // Scanned image-only PDF: 0 extracted text tokens across content streams + embedded /DCTDecode JPEG stream(s)
    if (!pageStreams.length && embeddedJpegs.length > 0) {
      var pageImages = [];
      var scannedPages = Object.create(null);
      var emptyTextPages = [];
      for (var ji = 0; ji < embeddedJpegs.length; ji++) {
        var imgEl = await loadImageFromBytes(embeddedJpegs[ji], 'image/jpeg');
        pageImages.push(imgEl);
        scannedPages[ji + 1] = true;
        emptyTextPages.push([]);
      }
      return {
        pages: emptyTextPages,
        pageImages: pageImages,
        embeddedJpegs: embeddedJpegs,
        scannedPages: scannedPages
      };
    }

    var scannedFallbackFlags = Object.create(null);
    if (!pageStreams.length) {
      scannedFallbackFlags[1] = true;
      pageStreams.push([
        L(
          'Scanned image-only PDF page — previewing in fallback mode. Use Circle, Arrow, or Pin tools to annotate clauses directly.',
          '扫描版纯图像 PDF 页面 —— 正在离线兼容模式下预览，可直接使用画圈、箭头或标记钉工具标注。',
          'Página PDF escaneada (solo imagen) — vista previa local. Use Círculo, Flecha o Pin para anotar.'
        )
      ]);
    }

    while (declaredPages > pageStreams.length && pageStreams.length < 60) {
      pageStreams.push([
        docName + ' — ' + L('Page', '第', 'Página') + ' ' + (pageStreams.length + 1)
      ]);
    }

    var wrappedPages = pageStreams.map(function (pgLines) {
      var wrapped = [];
      for (var li = 0; li < pgLines.length; li++) {
        var segs = wrapLineOnWords(pgLines[li], 88);
        for (var si = 0; si < segs.length; si++) wrapped.push(segs[si]);
      }
      return wrapped;
    });

    return {
      pages: wrappedPages,
      pageImages: embeddedJpegs.length ? await Promise.all(embeddedJpegs.map(function (jb) { return loadImageFromBytes(jb, 'image/jpeg'); })) : null,
      embeddedJpegs: embeddedJpegs,
      scannedPages: scannedFallbackFlags
    };
  }

  async function parsePdfFallbackPages(pdfBytes, docName) {
    var det = await parsePdfFallbackDetail(pdfBytes, docName);
    return det.pages;
  }

  function paginatePlainText(rawText) {
    var norm = String(rawText || '').replace(/\r\n/g, '\n');
    var pageMarkerSplit = norm.split(/\n*(?=\[Page\s+\d+\])/i).filter(function (s) { return s.trim(); });
    if (pageMarkerSplit.length > 1) {
      return pageMarkerSplit.map(function (chunk) {
        var out = [];
        chunk.split('\n').forEach(function (ln) {
          wrapLineOnWords(ln, 88).forEach(function (w) { out.push(w); });
        });
        return out;
      });
    }
    var allLines = norm.split('\n');
    var pages = [];
    var cur = [];
    for (var i = 0; i < allLines.length; i++) {
      var wrapped = wrapLineOnWords(allLines[i], 88);
      for (var w = 0; w < wrapped.length; w++) {
        cur.push(wrapped[w]);
        if (cur.length >= 42) {
          pages.push(cur);
          cur = [];
        }
      }
    }
    if (cur.length || !pages.length) pages.push(cur);
    return pages;
  }

  function stripHtmlToReadableLines(htmlStr) {
    try {
      var doc = new DOMParser().parseFromString(String(htmlStr || ''), 'text/html');
      if (doc && doc.body) {
        doc.querySelectorAll('script,style,noscript').forEach(function (n) { n.remove(); });
        var txt = (doc.body.innerText || doc.body.textContent || '').replace(/\n{3,}/g, '\n\n');
        return paginatePlainText(txt);
      }
    } catch (_) {}
    var plain = String(htmlStr || '').replace(/<[^>]+>/g, ' ').replace(/\s{2,}/g, ' ');
    return paginatePlainText(plain);
  }

  /* -------------------- Session File Store -------------------- */
  var sessionStore = {
    bySlug: Object.create(null), // slug -> { docs: [], pending: Promise|null, countHint: 0 }
    lastKey: '__default__'
  };

  function resolveBucketKey(slug) {
    var clean = String(slug || '').trim();
    return clean || '__default__';
  }

  function ensureBucket(key) {
    if (!sessionStore.bySlug[key]) {
      sessionStore.bySlug[key] = { docs: [], pending: null, countHint: 0 };
    }
    return sessionStore.bySlug[key];
  }

  function createDocRecord(item) {
    var name = baseName(item.name || item.fullPath || 'Document');
    var fullPath = item.fullPath || name;
    var mime = item.mime || mimeFromName(name);
    var bytes = toUint8Array(item.bytes);
    return {
      id: (fullPath + ':' + bytes.length).toLowerCase(),
      name: name,
      fullPath: fullPath,
      mime: mime,
      bytes: bytes,
      size: bytes.length,
      sourceZipName: item.sourceZipName || '',
      kind: mime === 'application/pdf' ? 'pdf'
        : mime.indexOf('image/') === 0 ? 'image'
        : mime === 'text/html' ? 'html'
        : 'text',
      pdfDoc: null,
      fallbackPages: null,
      fallbackPageImages: null,
      embeddedJpegs: null,
      scannedPages: Object.create(null),
      imageEl: null,
      numPages: 1,
      textBoxesByPage: Object.create(null)
    };
  }

  function mergeDocsIntoBucket(bucket, newDocs) {
    for (var i = 0; i < newDocs.length; i++) {
      var doc = newDocs[i];
      var replaced = false;
      for (var j = 0; j < bucket.docs.length; j++) {
        if (bucket.docs[j].fullPath.toLowerCase() === doc.fullPath.toLowerCase()) {
          bucket.docs[j] = doc;
          replaced = true;
          break;
        }
      }
      if (!replaced) bucket.docs.push(doc);
    }
    bucket.countHint = bucket.docs.length;
  }

  async function normalizeRawFileItem(entry) {
    if (!entry) return [];
    var name = '';
    var mime = '';
    var bytes = null;

    if (typeof File !== 'undefined' && entry instanceof File) {
      name = entry.name || 'Document.pdf';
      mime = entry.type || mimeFromName(name);
      bytes = new Uint8Array(await entry.arrayBuffer());
    } else if (typeof Blob !== 'undefined' && entry instanceof Blob) {
      name = entry.name || 'Document.pdf';
      mime = entry.type || mimeFromName(name);
      bytes = new Uint8Array(await entry.arrayBuffer());
    } else if (typeof entry === 'object') {
      name = entry.name || entry.fullPath || 'Document';
      mime = entry.mime || entry.type || mimeFromName(name);
      if (entry.bytes) {
        bytes = toUint8Array(entry.bytes);
      } else if (entry.buffer instanceof ArrayBuffer) {
        bytes = new Uint8Array(entry.buffer);
      } else if (entry.base64) {
        bytes = base64ToBytes(entry.base64);
      } else if (typeof entry.text === 'string') {
        bytes = new TextEncoder().encode(entry.text);
      } else if (typeof entry.arrayBuffer === 'function') {
        bytes = new Uint8Array(await entry.arrayBuffer());
      }
    }

    if (!bytes || !bytes.length) return [];

    var isZip = ZIP_EXT_RE.test(name) ||
      mime === 'application/zip' ||
      mime === 'application/x-zip-compressed' ||
      (bytes.length >= 4 && bytes[0] === 0x50 && bytes[1] === 0x4b && bytes[2] === 0x03 && bytes[3] === 0x04 && !/\.docx$|\.xlsx$/i.test(name));

    if (isZip) {
      var inner = await unpackZipBytes(bytes, name);
      return inner.map(createDocRecord);
    }
    if (NESTED_ARCHIVE_EXT_RE.test(name)) {
      throw makeZipSafetyError(
        'NESTED_ARCHIVE_PROHIBITED',
        L(
          'Archive rejected: only standard .zip packets (max 40 files, <= 100 MB, <= 50:1 ratio) are supported.',
          '压缩包已被拒绝：仅支持标准 .zip 披露包（最多 40 个文件、<= 100 MB、压缩比 <= 50:1）。',
          'Archivo rechazado: solo se admiten paquetes .zip estándar (máx. 40 archivos, <= 100 MB, <= 50:1).'
        )
      );
    }
    if (!SUPPORTED_EXT_RE.test(name) && mime !== 'application/pdf' && mime.indexOf('text/') !== 0 && mime.indexOf('image/') !== 0) {
      return [];
    }
    return [createDocRecord({
      name: name,
      fullPath: entry.fullPath || name,
      mime: mime,
      bytes: bytes
    })];
  }

  function ingestFiles(fileList, slug) {
    var list = !fileList ? []
      : (typeof fileList.length === 'number' && typeof fileList !== 'string') ? Array.prototype.slice.call(fileList)
      : [fileList];
    if (!list.length) return Promise.resolve([]);

    var key = resolveBucketKey(slug);
    var bucket = ensureBucket(key);
    sessionStore.lastKey = key;

    var prevPending = bucket.pending || Promise.resolve();
    var work = prevPending.then(async function () {
      var out = [];
      var batchError = null;
      for (var i = 0; i < list.length; i++) {
        try {
          var recs = await normalizeRawFileItem(list[i]);
          for (var j = 0; j < recs.length; j++) out.push(recs[j]);
        } catch (err) {
          batchError = err;
        }
      }
      if (batchError) {
        setStudioError(
          (batchError && batchError.code) || 'ARCHIVE_SAFETY_ERROR',
          (batchError && batchError.message) || String(batchError)
        );
      } else if (out.length > 0) {
        setStudioError('', '');
      }
      if (out.length > 0) {
        mergeDocsIntoBucket(bucket, out);
      } else {
        bucket.countHint = bucket.docs.length;
      }
      if (studioState.isOpen && (studioState.bucketKey === key || key === '__default__')) {
        if (studioState.docs.length === 0 || key === studioState.bucketKey) {
          studioState.docs = bucket.docs.slice();
          if (studioState.docIndex >= studioState.docs.length) studioState.docIndex = 0;
          syncFileSelect();
          if (studioState.docs.length) await loadAndRenderCurrentDoc();
          else updateErrorBanner();
        }
      }
      return bucket.docs.slice();
    }).catch(function (err) {
      bucket.countHint = bucket.docs.length;
      setStudioError((err && err.code) || 'ARCHIVE_SAFETY_ERROR', (err && err.message) || String(err));
      return bucket.docs.slice();
    });

    bucket.pending = work;
    return work;
  }

  function getBucketForSlug(slug) {
    var key = resolveBucketKey(slug);
    if (sessionStore.bySlug[key] && (sessionStore.bySlug[key].docs.length || sessionStore.bySlug[key].countHint)) {
      return { key: key, bucket: sessionStore.bySlug[key] };
    }
    if (key !== '__default__' && sessionStore.bySlug.__default__ &&
        (sessionStore.bySlug.__default__.docs.length || sessionStore.bySlug.__default__.countHint)) {
      return { key: '__default__', bucket: sessionStore.bySlug.__default__ };
    }
    return { key: key, bucket: ensureBucket(key) };
  }

  function hasLocalFiles(slug) {
    if (slug) {
      var b = sessionStore.bySlug[String(slug).trim()];
      if (b && (b.docs.length > 0 || b.countHint > 0)) return true;
      var hasAnySpecific = Object.keys(sessionStore.bySlug).some(function (k) {
        return k !== '__default__' && (sessionStore.bySlug[k].docs.length > 0 || sessionStore.bySlug[k].countHint > 0);
      });
      if (!hasAnySpecific && sessionStore.bySlug.__default__) {
        return sessionStore.bySlug.__default__.docs.length > 0 || sessionStore.bySlug.__default__.countHint > 0;
      }
      return false;
    }
    return Object.keys(sessionStore.bySlug).some(function (k) {
      var b2 = sessionStore.bySlug[k];
      return b2 && (b2.docs.length > 0 || b2.countHint > 0);
    });
  }

  /* -------------------- Analysis Normalizer (Data Honesty) -------------------- */
  function unwrapAnalysisPayload(raw) {
    if (!raw || typeof raw !== 'object') return null;
    if (raw.priorityReading || raw.findings || raw.defects || raw.deduplicatedDefects || raw.easementsAndExceptions || raw.exceptions || raw.hoaFlags || raw.contradictionMatrix || raw.contradictions) {
      return raw;
    }
    var candidates = [
      raw.privateUserAnalysis,
      raw.data,
      raw.analysis,
      raw.authorizedDisclosures && raw.authorizedDisclosures.data,
      raw.authorizedDisclosures,
      raw.sharedDealTeamAnalysis && raw.sharedDealTeamAnalysis.data,
      raw.sharedDealTeamAnalysis
    ];
    for (var i = 0; i < candidates.length; i++) {
      if (candidates[i] && typeof candidates[i] === 'object') {
        var nested = unwrapAnalysisPayload(candidates[i]);
        if (nested) return nested;
      }
    }
    return raw;
  }

  function parsePageNum(raw, fallback) {
    if (typeof raw === 'number' && isFinite(raw) && raw >= 1) return Math.round(raw);
    var m = String(raw || '').match(/\d+/);
    if (m) {
      var n = parseInt(m[0], 10);
      if (isFinite(n) && n >= 1) return n;
    }
    return fallback || 1;
  }

  function extractAnalysisViewModel(rawInput) {
    var root = unwrapAnalysisPayload(rawInput);
    if (!root || typeof root !== 'object') {
      return {
        headline: '',
        redFlags: [],
        prioritySections: [],
        contradictions: [],
        defects: [],
        exceptions: [],
        hoaFlags: [],
        disclaimer: ''
      };
    }

    var pr = root.priorityReading || {};
    var seen = Object.create(null);
    function makeItem(obj, defCat) {
      if (!obj || typeof obj !== 'object') return null;
      var title = String(obj.title || obj.item || obj.type || obj.topic || defCat || 'Finding').trim();
      var detail = String(obj.detail || obj.summary || obj.analysis || obj.excerpt || obj.quote || '').trim();
      var sev = String(obj.severity || obj.tier || 'Moderate').trim();
      var cit = obj.primaryCitation || obj.citation || (Array.isArray(obj.citations) && obj.citations[0]) || {};
      var sourceDoc = String(obj.sourceDoc || obj.contradictingDoc || obj.docName || cit.docName || cit.doc_name || '').trim();
      var sourcePage = parsePageNum(obj.sourcePage || obj.contradictingPage || obj.page || obj.pageRef || cit.page, 1);
      var quote = String(obj.quote || obj.contradictingQuote || obj.excerpt || cit.quote || '').trim();
      var costLow = obj.costLowUsd != null ? obj.costLowUsd : obj.costLow;
      var costHigh = obj.costHighUsd != null ? obj.costHighUsd : obj.costHigh;
      return {
        title: title,
        detail: detail,
        severity: sev,
        category: String(obj.category || obj.trade || obj.system || defCat || '').trim(),
        sourceDoc: sourceDoc,
        sourcePage: sourcePage,
        quote: quote,
        costLow: costLow,
        costHigh: costHigh,
        searchTerms: [quote, title, detail].filter(Boolean)
      };
    }

    function dedupKey(it) {
      return [it.title.toLowerCase(), it.sourceDoc.toLowerCase(), it.sourcePage].join('|');
    }

    var redFlags = [];
    (Array.isArray(pr.redFlags) ? pr.redFlags : []).forEach(function (rf) {
      var item = makeItem(rf, rf && rf.category ? rf.category : 'Red Flag');
      if (item) {
        seen['rf:' + dedupKey(item)] = 1;
        redFlags.push(item);
      }
    });

    var prioritySections = [];
    (Array.isArray(pr.prioritySections) ? pr.prioritySections : []).forEach(function (sec) {
      if (!sec || typeof sec !== 'object') return;
      var pItems = [];
      (Array.isArray(sec.problematicItems) ? sec.problematicItems : []).forEach(function (pi) {
        var item = makeItem(pi, sec.label);
        if (item) {
          seen['sec:' + dedupKey(item)] = 1;
          pItems.push(item);
        }
      });
      prioritySections.push({
        priority: sec.priority,
        key: sec.key || '',
        label: sec.label || '',
        present: Boolean(sec.present),
        summary: sec.summary || '',
        documents: Array.isArray(sec.documents) ? sec.documents : [],
        items: pItems
      });
    });

    var rawContras = Array.isArray(root.contradictionMatrix) ? root.contradictionMatrix
      : Array.isArray(root.contradictions) ? root.contradictions : [];
    var contradictions = [];
    rawContras.forEach(function (c) {
      var item = makeItem({
        title: (c && c.topic) ? c.topic : 'Disclosure Contradiction',
        detail: (c && (c.analysis || c.summary || c.detail)) || '',
        severity: (c && c.severity) || 'Major',
        sourceDoc: (c && (c.contradictingDoc || c.sourceDoc)) || '',
        sourcePage: (c && (c.contradictingPage || c.sourcePage)) || 1,
        quote: (c && (c.contradictingQuote || c.quote)) || ''
      }, 'Contradiction');
      if (item) contradictions.push(item);
    });

    var rawDefects = Array.isArray(root.defects) ? root.defects
      : Array.isArray(root.deduplicatedDefects) ? root.deduplicatedDefects
      : Array.isArray(root.findings) ? root.findings : [];
    var defects = [];
    rawDefects.forEach(function (d) {
      var item = makeItem(d, 'Defect');
      if (item && !seen['sec:' + dedupKey(item)]) defects.push(item);
    });

    var rawExceptions = Array.isArray(root.exceptions) ? root.exceptions
      : Array.isArray(root.easementsAndExceptions) ? root.easementsAndExceptions : [];
    var rawLiens = Array.isArray(root.involuntaryLiens) ? root.involuntaryLiens : [];
    var exceptions = [];
    rawLiens.forEach(function (l) {
      var item = makeItem({
        title: l.type || 'Lien / Notice',
        detail: l.excerpt || l.status || '',
        severity: l.status === 'active' ? 'Critical' : 'Moderate',
        sourceDoc: l.sourceDoc || 'Prelim_Title.pdf',
        sourcePage: l.page || l.sourcePage || 1,
        quote: l.excerpt || ''
      }, 'Title Lien');
      if (item) exceptions.push(item);
    });
    rawExceptions.forEach(function (e) {
      var item = makeItem({
        title: e.type || e.title || 'Schedule B Exception',
        detail: e.excerpt || e.detail || '',
        severity: /solar|ucc|encroach/i.test(String(e.type || '')) ? 'Major' : 'Moderate',
        sourceDoc: e.sourceDoc || 'Prelim_Title.pdf',
        sourcePage: e.page || e.sourcePage || 1,
        quote: e.excerpt || ''
      }, 'Schedule B Exception');
      if (item) exceptions.push(item);
    });

    var rawHoa = Array.isArray(root.hoaFlags) ? root.hoaFlags
      : Array.isArray(root.alerts) ? root.alerts
      : (root.warrantability ? (Array.isArray(root.redFlags) ? root.redFlags : []) : []);
    var hoaFlags = [];
    rawHoa.forEach(function (h) {
      var item = makeItem(h, 'HOA Flag');
      if (item) hoaFlags.push(item);
    });
    if (root.warrantability && Array.isArray(root.warrantability.checklist)) {
      root.warrantability.checklist.forEach(function (chk) {
        if (chk && chk.status && chk.status !== 'compliant' && chk.status !== 'pass') {
          var cit = chk.citation || {};
          var item = makeItem({
            title: chk.label || chk.code || 'HOA Warrantability Check',
            detail: chk.impact || chk.observed || '',
            severity: chk.status === 'non_warrantable' ? 'Critical' : 'Major',
            sourceDoc: cit.docName || 'HOA_Packet.pdf',
            sourcePage: cit.page || 1,
            quote: cit.quote || chk.observed || ''
          }, 'HOA Warrantability');
          if (item) hoaFlags.push(item);
        }
      });
    }

    var headline = String(
      root.executiveHeadline ||
      (root.docSummary && root.docSummary.headline) ||
      root.headline ||
      root.overallStatus ||
      ''
    ).trim();

    return {
      headline: headline,
      executiveSummary: String(root.executiveSummary || '').trim(),
      negotiationStrategy: String(root.negotiationStrategy || '').trim(),
      aiEnhanced: !!root.aiEnhanced,
      model: String(root.model || '').trim(),
      redFlags: redFlags,
      prioritySections: prioritySections,
      contradictions: contradictions,
      defects: defects,
      exceptions: exceptions,
      hoaFlags: hoaFlags,
      disclaimer: String(pr.standingDisclaimer || root.disclaimer || '').trim()
    };
  }

  /* -------------------- Studio State -------------------- */
  var studioState = {
    isOpen: false,
    bucketKey: '__default__',
    slug: '',
    address: '',
    role: getActiveRole(),
    onAskChat: null,
    docs: [],
    docIndex: 0,
    currentPage: 1,
    zoom: 1.0,
    fitWidth: true,
    railCollapsed: false,
    tool: 'pan', // 'pan' | 'circle' | 'arrow' | 'rect' | 'pen' | 'pin'
    color: '#ef4444',
    pinSeq: 0,
    annotations: Object.create(null), // key(docId::page) -> []
    activeAiPointer: null, // { key, title, severity, nx, ny, nw, nh, matched, startMs }
    pulseRaf: 0,
    renderSeq: 0,
    activeRenderTask: null,
    analysisRaw: null,
    analysisVm: extractAnalysisViewModel(null),
    activeSideTab: 'analysis', // 'analysis' | 'chat'
    chatHistory: [],
    chatSending: false,
    chatDraft: '',
    dragging: null,
    panStart: null,
    statusNotice: '',
    lastError: '',
    lastErrorCode: ''
  };

  function setStudioError(code, msg) {
    studioState.lastErrorCode = String(code || '');
    studioState.lastError = String(msg || '');
    updateErrorBanner();
    if (studioState.lastError && !studioState.isOpen) {
      try {
        var extBanners = document.querySelectorAll('#bcErrBanner');
        for (var i = 0; i < extBanners.length; i++) {
          extBanners[i].textContent = studioState.lastError;
          extBanners[i].classList.add('on');
        }
      } catch (_) {}
    }
  }

  function updateErrorBanner() {
    var errBanner = document.getElementById('psErrorBanner');
    var errTxt = document.getElementById('psErrorBannerText');
    if (!errBanner || !errTxt) return;
    if (studioState.lastError) {
      errTxt.textContent = '⚠️ ' + studioState.lastError;
      errBanner.style.display = 'flex';
    } else {
      errTxt.textContent = '';
      errBanner.style.display = 'none';
    }
  }

  function isCurrentDocPageScanned(doc, pageNum) {
    var d = doc || studioState.docs[studioState.docIndex];
    var p = pageNum || studioState.currentPage || 1;
    if (!d) return false;
    if (d.kind === 'image') return true;
    return Boolean(d.kind === 'pdf' && d.scannedPages && d.scannedPages[p]);
  }

  function updateScannedBannerForCurrentPage() {
    var scanBanner = document.getElementById('psScannedBanner');
    var scanTxt = document.getElementById('psScannedBannerText');
    if (!scanBanner || !scanTxt) return;
    var doc = studioState.docs[studioState.docIndex];
    if (doc && isCurrentDocPageScanned(doc, studioState.currentPage)) {
      scanTxt.textContent = '🖼 ' + L(
        'Scanned image-only page — page raster is displayed (no selectable text layer). Use Circle, Arrow, or Pin tools to mark clauses directly.',
        '扫描版纯图像页面 —— 已直接渲染页面原图（无可选文字层），可使用画圈、箭头或标记钉工具直接标注条款。',
        'Página escaneada (solo imagen) — imagen renderizada sin capa de texto. Use Círculo, Flecha o Pin para marcar cláusulas directamente.'
      );
      scanBanner.style.display = 'flex';
    } else {
      scanTxt.textContent = '';
      scanBanner.style.display = 'none';
    }
  }

  function docPageKey(doc, page) {
    var d = doc || studioState.docs[studioState.docIndex];
    var p = page || studioState.currentPage || 1;
    if (!d) return 'none::1';
    return d.id + '::' + p;
  }

  function getAnnotationsFor(doc, page) {
    var k = docPageKey(doc, page);
    if (!studioState.annotations[k]) studioState.annotations[k] = [];
    return studioState.annotations[k];
  }

  /* -------------------- Studio CSS & DOM Shell (Light & Dark Theme) -------------------- */
  function scopeDarkCss(rules) {
    function withPrefix(pfx) {
      return rules.map(function (rule) {
        var bi = rule.indexOf('{');
        if (bi < 0) return rule;
        var sels = rule.slice(0, bi).split(',').map(function (s) {
          var t = s.trim();
          return t ? (pfx + ' ' + t) : t;
        }).join(',');
        return sels + rule.slice(bi);
      }).join('\n');
    }
    return '@media (prefers-color-scheme: dark){\n' + withPrefix('html:not([data-theme="light"])') + '\n}\n' + withPrefix('html[data-theme="dark"]');
  }

  function injectStyles() {
    if (!document.getElementById('oe-pdfjs-viewer-css')) {
      try {
        var lnk = document.createElement('link');
        lnk.id = 'oe-pdfjs-viewer-css';
        lnk.rel = 'stylesheet';
        lnk.href = resolveVendorUrl(PDFJS_VIEWER_CSS_URL);
        document.head.appendChild(lnk);
      } catch (_) {}
    }
    if (document.getElementById('oe-pdf-studio-styles')) return;
    var darkRules = [
      '#oePdfStudioBackdrop{background:rgba(15,23,42,.82)}',
      '.ps-studio{background:var(--surface,#111827);border-color:var(--line-strong,#334155);box-shadow:0 24px 60px rgba(0,0,0,.65);color:var(--ink,#f1f5f9)}',
      '.ps-topbar{background:var(--surface,#111827);border-bottom-color:var(--line-strong,#334155)}',
      '.ps-title{color:var(--ink,#f8fafc)}',
      '.ps-addr-chip{color:var(--ink2,#cbd5e1);background:var(--surface-2,#1e293b);border-color:var(--line-strong,#334155)}',
      '.ps-privacy-pill{color:#a7f3d0;background:rgba(16,185,129,.16);border-color:rgba(52,211,153,.38)}',
      '.ps-toolbar{background:var(--surface-2,#1e293b);border-bottom-color:var(--line-strong,#334155)}',
      '.ps-grp{border-right-color:var(--line-strong,#334155)}',
      '.ps-select{border-color:var(--line-strong,#334155);color:var(--ink,#f1f5f9);background:var(--surface,#0f172a)}',
      '.ps-badge{background:var(--surface,#0f172a);color:var(--ink2,#cbd5e1);border:1px solid var(--line-strong,#334155)}',
      '.ps-btn{border-color:var(--line-strong,#334155);background:var(--surface,#0f172a);color:var(--ink2,#e2e8f0)}',
      '.ps-btn:not(.act):not(.pri):not(.danger):hover:not(:disabled){background:#293548;border-color:#475569;color:#f8fafc}',
      '.ps-btn.act,.ps-btn.pri{background:#0f766e;color:#ffffff;border-color:#14b8a6}',
      '.ps-btn.pri:hover:not(:disabled){background:#14b8a6;color:#042f2e}',
      '.ps-btn.danger{color:#fca5a5;border-color:rgba(248,113,113,.42);background:rgba(220,38,38,.18)}',
      '.ps-btn.danger:hover:not(:disabled){background:rgba(220,38,38,.28)}',
      '.ps-page-inp{border-color:var(--line-strong,#334155);color:var(--ink,#f1f5f9);background:var(--surface,#0f172a)}',
      '.ps-page-tot{color:var(--muted,#94a3b8)}',
      '.ps-zoom-pct{color:var(--ink2,#cbd5e1)}',
      '.ps-swatch{border-color:#1e293b;box-shadow:0 0 0 1px #475569}',
      '.ps-swatch.act{box-shadow:0 0 0 2px #f8fafc}',
      '.ps-main{background:#0b1120}',
      '.ps-rail{background:var(--surface-2,#1e293b);border-right-color:var(--line-strong,#334155)}',
      '.ps-rail-item{border-color:var(--line-strong,#334155);background:var(--surface,#111827);color:var(--ink2,#cbd5e1)}',
      '.ps-rail-item:hover{border-color:#2dd4bf}',
      '.ps-rail-item.act{border-color:#14b8a6;background:rgba(20,184,166,.18);color:#5eead4}',
      '.ps-rail-dot{color:#5eead4}',
      '.ps-ai-banner{background:var(--surface-2,#1e293b);color:var(--ink,#f1f5f9);box-shadow:0 6px 20px rgba(0,0,0,.4)}',
      '.ps-ai-banner.crit{border-color:rgba(248,113,113,.5);background:rgba(127,29,29,.88);color:#fecaca}',
      '.ps-ai-banner.warn{border-color:rgba(251,191,36,.5);background:rgba(120,53,15,.88);color:#fde68a}',
      '.ps-ai-banner.info{border-color:rgba(45,212,191,.5);background:rgba(17,94,89,.88);color:#ccfbf1}',
      '.ps-page-stage{border-color:#334155;box-shadow:0 12px 34px rgba(0,0,0,.55)}',
      '.ps-empty-stage{background:var(--surface,#111827);border-color:var(--line-strong,#475569);color:var(--ink,#f1f5f9);box-shadow:0 10px 28px rgba(0,0,0,.4)}',
      '.ps-empty-title{color:var(--ink,#f8fafc)}',
      '.ps-empty-desc,.ps-chat-status,.ps-disclaimer-note{color:var(--muted,#94a3b8)}',
      '.ps-sum-strat{color:var(--ink2,#cbd5e1)}',
      '.ps-sidepanel{border-color:var(--line-strong,#334155);background:var(--surface,#111827)}',
      '.ps-tabs{background:var(--surface-2,#1e293b);border-bottom-color:var(--line-strong,#334155)}',
      '.ps-tab-btn{color:var(--muted,#94a3b8)}',
      '.ps-tab-btn.act{background:var(--surface,#111827);color:#5eead4;border-color:var(--line-strong,#334155)}',
      '.ps-sum-banner{background:var(--surface-2,#1e293b);border-color:var(--line-strong,#334155);color:var(--ink2,#cbd5e1)}',
      '.ps-sec-hdr{color:var(--muted,#94a3b8)}',
      '.ps-card{background:var(--surface-2,#1e293b);border-color:var(--line-strong,#334155)}',
      '.ps-card.missing{background:var(--soft,#0f172a)}',
      '.ps-sev.crit{background:rgba(220,38,38,.2);color:#fca5a5;border-color:rgba(248,113,113,.4)}',
      '.ps-sev.maj{background:rgba(217,119,6,.2);color:#fde68a;border-color:rgba(251,191,36,.4)}',
      '.ps-sev.mod{background:rgba(37,99,235,.22);color:#93c5fd;border-color:rgba(96,165,250,.4)}',
      '.ps-sev.good{background:rgba(16,185,129,.2);color:#a7f3d0;border-color:rgba(52,211,153,.4)}',
      '.ps-card-title{color:var(--ink,#f8fafc)}',
      '.ps-card-detail{color:var(--ink2,#cbd5e1)}',
      '.ps-cite-tag{color:var(--ink2,#cbd5e1);background:var(--surface,#0f172a);border-color:var(--line-strong,#334155)}',
      '.ps-locate-btn{border-color:rgba(45,212,191,.45);background:rgba(20,184,166,.16);color:#5eead4}',
      '.ps-locate-btn:hover{background:#0f766e;color:#ffffff;border-color:#14b8a6}',
      '.ps-role-bar{background:var(--surface-2,#1e293b);border-bottom-color:var(--line-strong,#334155);color:var(--ink2,#cbd5e1)}',
      '.ps-role-pill{border-color:var(--line-strong,#334155);background:var(--surface,#0f172a);color:var(--ink2,#cbd5e1)}',
      '.ps-role-pill.act{background:#0f766e;color:#ffffff;border-color:#14b8a6}',
      '#psChatLog{background:var(--surface,#111827)}',
      '.ps-msg.a{background:var(--surface-2,#1e293b);color:var(--ink,#f1f5f9);border-color:var(--line-strong,#334155)}',
      '.ps-chat-cite-btn{border-color:rgba(45,212,191,.42);background:rgba(20,184,166,.16);color:#5eead4}',
      '.ps-chat-cite-btn:hover{background:#0f766e;color:#ffffff}',
      '.ps-chat-foot{border-top-color:var(--line-strong,#334155);background:var(--surface-2,#1e293b)}',
      '#psChatInput{border-color:var(--line-strong,#334155);color:var(--ink,#f8fafc);background:var(--surface,#0f172a)}',
      '#psChatInput::placeholder{color:var(--muted,#94a3b8)}'
    ];

    var st = document.createElement('style');
    st.id = 'oe-pdf-studio-styles';
    st.textContent = [
      '#oePdfStudioBackdrop{position:fixed;inset:0;z-index:10050;background:rgba(248,250,252,.86);backdrop-filter:blur(8px);-webkit-backdrop-filter:blur(8px);display:none;align-items:stretch;justify-content:center;padding:12px;box-sizing:border-box}',
      '#oePdfStudioBackdrop.on{display:flex}',
      '.ps-studio{width:100%;max-width:1480px;height:100%;background:#ffffff;border:1px solid #cbd5e1;border-radius:18px;box-shadow:0 24px 60px rgba(15,23,42,.15);display:flex;flex-direction:column;overflow:hidden;color:#0f172a;font-family:Inter,system-ui,-apple-system,sans-serif}',
      '.ps-topbar{display:flex;align-items:center;justify-content:space-between;gap:10px;padding:10px 14px;background:#ffffff;border-bottom:1px solid #e2e8f0;flex-wrap:wrap}',
      '.ps-brand{display:flex;align-items:center;gap:10px;min-width:0;flex-wrap:wrap}',
      '.ps-title{font-size:14.5px;font-weight:800;color:#0f172a;letter-spacing:-.01em;white-space:nowrap}',
      '.ps-addr-chip{font-size:12px;font-weight:600;color:#475569;background:#f1f5f9;border:1px solid #e2e8f0;border-radius:999px;padding:3px 10px;max-width:300px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}',
      '.ps-privacy-pill{display:inline-flex;align-items:center;gap:6px;font-size:11.8px;font-weight:700;color:#065f46;background:#ecfdf5;border:1px solid #6ee7b7;border-radius:999px;padding:4px 11px;line-height:1.3}',
      '.ps-toolbar{display:flex;align-items:center;gap:6px;padding:7px 12px;background:#f8fafc;border-bottom:1px solid #e2e8f0;flex-wrap:wrap}',
      '.ps-grp{display:inline-flex;align-items:center;gap:4px;padding:2px 6px 2px 0;border-right:1px solid #e2e8f0}',
      '.ps-grp:last-child{border-right:0;padding-right:0}',
      '.ps-select{border:1px solid #cbd5e1;border-radius:8px;padding:5px 8px;font:600 12.5px Inter,sans-serif;color:#0f172a;background:#ffffff;max-width:230px}',
      '.ps-badge{display:inline-flex;align-items:center;padding:2px 8px;border-radius:999px;font-size:11.5px;font-weight:700;background:#e2e8f0;color:#334155}',
      '.ps-btn{border:1px solid #cbd5e1;background:#ffffff;color:#1e293b;border-radius:8px;padding:5px 9px;font:600 12px Inter,sans-serif;cursor:pointer;display:inline-flex;align-items:center;gap:4px;white-space:nowrap;transition:all .12s}',
      '.ps-btn:hover:not(:disabled){background:#f1f5f9;border-color:#94a3b8}',
      '.ps-btn:disabled{opacity:.48;cursor:not-allowed}',
      '.ps-btn.act{background:#0f766e;color:#ffffff;border-color:#0f766e}',
      '.ps-btn.pri{background:#0f766e;color:#ffffff;border-color:#0f766e}',
      '.ps-btn.pri:hover:not(:disabled){background:#0d9488}',
      '.ps-btn.danger{color:#b91c1c;border-color:#fecaca;background:#fef2f2}',
      '.ps-btn.danger:hover:not(:disabled){background:#fee2e2}',
      '.ps-page-inp{width:46px;border:1px solid #cbd5e1;border-radius:7px;padding:4px 6px;font:700 12px Inter,sans-serif;text-align:center;color:#0f172a;background:#ffffff}',
      '.ps-page-tot{font-size:12px;font-weight:600;color:#64748b;padding:0 3px}',
      '.ps-zoom-pct{font-size:12px;font-weight:700;color:#334155;min-width:42px;text-align:center;font-variant-numeric:tabular-nums}',
      '.ps-swatch{width:19px;height:19px;border-radius:999px;border:2px solid #ffffff;box-shadow:0 0 0 1px #cbd5e1;cursor:pointer;padding:0;transition:transform .12s}',
      '.ps-swatch.act{box-shadow:0 0 0 2px #0f172a;transform:scale(1.1)}',
      '.ps-body{flex:1;min-height:0;display:flex;overflow:hidden}',
      '.ps-main{flex:1;min-width:0;display:flex;overflow:hidden;background:#eef2f6;position:relative}',
      '.ps-rail{width:168px;flex:0 0 168px;background:#f8fafc;border-right:1px solid #e2e8f0;overflow-y:auto;padding:8px;display:flex;flex-direction:column;gap:6px}',
      '.ps-rail.collapsed{display:none}',
      '.ps-rail-item{border:1px solid #e2e8f0;background:#ffffff;border-radius:9px;padding:7px 9px;text-align:left;cursor:pointer;display:flex;justify-content:space-between;align-items:center;font:600 12px Inter,sans-serif;color:#334155}',
      '.ps-rail-item:hover{border-color:#0f766e}',
      '.ps-rail-item.act{border-color:#0f766e;background:#ecfdf5;color:#065f46;font-weight:800}',
      '.ps-rail-dot{font-size:10.5px;color:#0f766e;font-weight:700}',
      '#psViewport{flex:1;min-width:0;min-height:0;overflow:auto;padding:20px;display:flex;flex-direction:column;align-items:center;position:relative}',
      '.ps-ai-banner{position:relative;z-index:4;flex-shrink:0;max-width:760px;width:100%;margin-bottom:10px;background:#ffffff;border:1.5px solid #f59e0b;border-radius:11px;padding:8px 12px;box-shadow:0 6px 20px rgba(15,23,42,.08);display:flex;align-items:center;justify-content:space-between;gap:10px;font-size:12.5px;line-height:1.4}',
      '.ps-ai-banner.crit{border-color:#ef4444;background:#fef2f2;color:#7f1d1d}',
      '.ps-ai-banner.warn{border-color:#f59e0b;background:#fffbeb;color:#78350f}',
      '.ps-ai-banner.info{border-color:#0f766e;background:#f0fdfa;color:#134e4a}',
      '.ps-page-stage{position:relative;background:#ffffff;border:1px solid #cbd5e1;border-radius:4px;box-shadow:0 10px 30px rgba(15,23,42,.12);flex-shrink:0}',
      '#psPdfCanvas{position:relative;z-index:1;display:block}',
      '#psTextLayer{position:absolute;inset:0;z-index:2;overflow:hidden;line-height:1;opacity:1;color:transparent}',
      '#psAnnotCanvas{position:absolute;inset:0;z-index:3;display:block}',
      '.ps-empty-stage{max-width:540px;margin:auto;background:#ffffff;border:1.5px dashed #94a3b8;border-radius:16px;padding:28px 24px;text-align:center;box-shadow:0 6px 20px rgba(15,23,42,.05)}',
      '.ps-empty-title{font-size:15.5px;font-weight:800;color:#0f172a;margin-bottom:6px}',
      '.ps-empty-desc{font-size:13px;color:#475569;line-height:1.5;margin-bottom:14px}',
      '.ps-chat-status{font-size:11px;color:#64748b}',
      '.ps-sum-strat{margin-top:5px;color:#334155}',
      '.ps-disclaimer-note{font-size:11px;color:#64748b;margin-top:4px;line-height:1.4}',
      '.ps-sidepanel{width:400px;min-width:320px;max-width:44vw;border-left:1px solid #e2e8f0;background:#ffffff;display:flex;flex-direction:column;min-height:0}',
      '.ps-tabs{display:flex;border-bottom:1px solid #e2e8f0;background:#f8fafc;padding:6px 8px 0;gap:6px}',
      '.ps-tab-btn{flex:1;border:1px solid transparent;border-bottom:0;background:transparent;border-radius:9px 9px 0 0;padding:8px 10px;font:700 12.5px Inter,sans-serif;color:#475569;cursor:pointer}',
      '.ps-tab-btn.act{background:#ffffff;color:#0f766e;border-color:#e2e8f0;margin-bottom:-1px;padding-bottom:9px}',
      '.ps-tab-pane{flex:1;min-height:0;display:none;flex-direction:column;overflow:hidden}',
      '.ps-tab-pane.act{display:flex}',
      '#psTabAnalysis{overflow-y:auto;padding:12px 14px;gap:10px}',
      '.ps-sum-banner{background:#f8fafc;border:1px solid #e2e8f0;border-radius:11px;padding:10px 12px;font-size:12.5px;color:#334155;line-height:1.45}',
      '.ps-sec-hdr{font-size:11.5px;font-weight:800;text-transform:uppercase;letter-spacing:.04em;color:#64748b;margin:8px 0 4px}',
      '.ps-card{background:#ffffff;border:1px solid #e2e8f0;border-radius:11px;padding:10px 12px;display:flex;flex-direction:column;gap:6px}',
      '.ps-card.crit{border-left:4px solid #ef4444}',
      '.ps-card.maj{border-left:4px solid #f59e0b}',
      '.ps-card.mod{border-left:4px solid #2563eb}',
      '.ps-card.missing{background:#f8fafc;border-style:dashed;opacity:.85}',
      '.ps-card-top{display:flex;justify-content:space-between;align-items:flex-start;gap:8px}',
      '.ps-sev{display:inline-flex;align-items:center;padding:2px 7px;border-radius:999px;font-size:10.8px;font-weight:800;white-space:nowrap}',
      '.ps-sev.crit{background:#fef2f2;color:#b91c1c;border:1px solid #fecaca}',
      '.ps-sev.maj{background:#fffbeb;color:#b45309;border:1px solid #fde68a}',
      '.ps-sev.mod{background:#eff6ff;color:#1d4ed8;border:1px solid #bfdbfe}',
      '.ps-sev.good{background:#ecfdf5;color:#065f46;border:1px solid #6ee7b7}',
      '.ps-card-title{font-size:13px;font-weight:700;color:#0f172a;line-height:1.35}',
      '.ps-card-detail{font-size:12.2px;color:#475569;line-height:1.42}',
      '.ps-card-foot{display:flex;justify-content:space-between;align-items:center;gap:8px;flex-wrap:wrap;margin-top:2px}',
      '.ps-cite-tag{font-size:11px;font-weight:700;color:#334155;background:#f1f5f9;border:1px solid #cbd5e1;border-radius:6px;padding:2px 7px}',
      '.ps-locate-btn{border:1px solid #0f766e;background:#f0fdfa;color:#0f766e;border-radius:8px;padding:4px 9px;font:700 11.5px Inter,sans-serif;cursor:pointer;display:inline-flex;align-items:center;gap:4px}',
      '.ps-locate-btn:hover{background:#0f766e;color:#ffffff}',
      '.ps-role-bar{padding:8px 12px;background:#f8fafc;border-bottom:1px solid #e2e8f0;display:flex;align-items:center;justify-content:space-between;gap:8px;flex-wrap:wrap;font-size:12px}',
      '.ps-role-pills{display:inline-flex;gap:4px;flex-wrap:wrap}',
      '.ps-role-pill{border:1px solid #cbd5e1;background:#ffffff;color:#334155;border-radius:999px;padding:3px 9px;font:700 11.5px Inter,sans-serif;cursor:pointer}',
      '.ps-role-pill.act{background:#0f766e;color:#ffffff;border-color:#0f766e}',
      '#psChatLog{flex:1;min-height:0;overflow-y:auto;padding:12px;display:flex;flex-direction:column;gap:9px;background:#ffffff}',
      '.ps-msg{max-width:92%;border-radius:12px;padding:9px 11px;font-size:12.8px;line-height:1.48;word-break:break-word}',
      '.ps-msg.u{align-self:flex-end;background:#0f766e;color:#ffffff;border-bottom-right-radius:4px}',
      '.ps-msg.a{align-self:flex-start;background:#f8fafc;color:#0f172a;border:1px solid #e2e8f0;border-bottom-left-radius:4px}',
      '.ps-chat-cites{display:flex;flex-wrap:wrap;gap:5px;margin-top:7px}',
      '.ps-chat-cite-btn{border:1px solid #99f6e4;background:#f0fdfa;color:#0f766e;border-radius:999px;padding:3px 8px;font:700 11px Inter,sans-serif;cursor:pointer}',
      '.ps-chat-cite-btn:hover{background:#0f766e;color:#ffffff}',
      '.ps-chat-foot{border-top:1px solid #e2e8f0;padding:9px 11px;background:#f8fafc;display:flex;flex-direction:column;gap:7px}',
      '.ps-chat-form{display:flex;gap:6px;align-items:flex-end}',
      '#psChatInput{flex:1;border:1px solid #cbd5e1;border-radius:9px;padding:7px 10px;font:500 12.8px Inter,sans-serif;color:#0f172a;background:#ffffff;resize:none;min-height:38px;max-height:96px}',
      '@media(max-width:960px){.ps-body{flex-direction:column}.ps-sidepanel{width:100%;max-width:none;height:44%;border-left:0;border-top:1px solid #e2e8f0}.ps-rail{display:none}}',
      scopeDarkCss(darkRules)
    ].join('\n');
    document.head.appendChild(st);
  }

  function buildStudioModal() {
    injectStyles();
    var existing = document.getElementById('oePdfStudioBackdrop');
    if (existing) return existing;

    var bd = document.createElement('div');
    bd.id = 'oePdfStudioBackdrop';
    bd.setAttribute('role', 'dialog');
    bd.setAttribute('aria-modal', 'true');
    bd.setAttribute('aria-labelledby', 'psStudioTitle');
    bd.innerHTML = [
      '<div class="ps-studio">',
      '  <div class="ps-topbar">',
      '    <div class="ps-brand">',
      '      <span class="ps-title" id="psStudioTitle">📄 PDF Studio</span>',
      '      <span class="ps-addr-chip" id="psAddrChip" style="display:none"></span>',
      '      <span class="ps-privacy-pill" id="psPrivacyPill"></span>',
      '    </div>',
      '    <div style="display:flex;align-items:center;gap:6px">',
      '      <input type="file" id="psAddFilesInput" multiple accept=".pdf,.zip,.txt,.md,.html,.htm,.png,.jpg,.jpeg,.webp" style="display:none">',
      '      <button type="button" class="ps-btn" id="psAddFilesBtn"></button>',
      '      <button type="button" class="ps-btn" id="psCloseBtn"></button>',
      '    </div>',
      '  </div>',
      '  <div class="ps-toolbar">',
      '    <div class="ps-grp">',
      '      <button type="button" class="ps-btn" id="psToggleRailBtn" title="Pages"></button>',
      '      <select class="ps-select" id="psFileSelect" aria-label="Document"></select>',
      '      <span class="ps-badge" id="psFileCountBadge">0</span>',
      '    </div>',
      '    <div class="ps-grp">',
      '      <button type="button" class="ps-btn" id="psPrevPageBtn">◀</button>',
      '      <input type="number" class="ps-page-inp" id="psPageInput" min="1" value="1" aria-label="Page">',
      '      <span class="ps-page-tot" id="psPageCount">/ 1</span>',
      '      <button type="button" class="ps-btn" id="psNextPageBtn">▶</button>',
      '    </div>',
      '    <div class="ps-grp">',
      '      <button type="button" class="ps-btn" id="psZoomOutBtn">−</button>',
      '      <span class="ps-zoom-pct" id="psZoomPct">100%</span>',
      '      <button type="button" class="ps-btn" id="psZoomInBtn">+</button>',
      '      <button type="button" class="ps-btn act" id="psFitWidthBtn"></button>',
      '    </div>',
      '    <div class="ps-grp" role="toolbar" aria-label="Annotation Tools">',
      '      <button type="button" class="ps-btn act" data-ps-tool="pan"></button>',
      '      <button type="button" class="ps-btn" data-ps-tool="circle"></button>',
      '      <button type="button" class="ps-btn" data-ps-tool="arrow"></button>',
      '      <button type="button" class="ps-btn" data-ps-tool="rect"></button>',
      '      <button type="button" class="ps-btn" data-ps-tool="pen"></button>',
      '      <button type="button" class="ps-btn" data-ps-tool="pin"></button>',
      '    </div>',
      '    <div class="ps-grp" id="psColorSwatches"></div>',
      '    <div class="ps-grp">',
      '      <button type="button" class="ps-btn" id="psUndoBtn"></button>',
      '      <button type="button" class="ps-btn danger" id="psClearPageBtn"></button>',
      '      <button type="button" class="ps-btn pri" id="psExportPngBtn"></button>',
      '    </div>',
      '  </div>',
      '  <div class="ps-body">',
      '    <div class="ps-main">',
      '      <aside class="ps-rail" id="psThumbRail" aria-label="Pages"></aside>',
      '      <div id="psViewport">',
      '        <div class="ps-ai-banner crit" id="psErrorBanner" role="alert" style="display:none">',
      '          <span id="psErrorBannerText"></span>',
      '          <button type="button" class="ps-btn" id="psErrorBannerCloseBtn" style="padding:2px 7px">✕</button>',
      '        </div>',
      '        <div class="ps-ai-banner info" id="psScannedBanner" style="display:none">',
      '          <span id="psScannedBannerText"></span>',
      '        </div>',
      '        <div class="ps-ai-banner info" id="psAiBanner" style="display:none">',
      '          <span id="psAiBannerText"></span>',
      '          <button type="button" class="ps-btn" id="psAiBannerCloseBtn" style="padding:2px 7px">✕</button>',
      '        </div>',
      '        <div class="ps-page-stage" id="psPageStage" style="display:none">',
      '          <canvas id="psPdfCanvas"></canvas>',
      '          <div id="psTextLayer" class="textLayer"></div>',
      '          <canvas id="psAnnotCanvas"></canvas>',
      '        </div>',
      '        <div class="ps-empty-stage" id="psEmptyStage">',
      '          <div style="font-size:28px;margin-bottom:8px">📂</div>',
      '          <div class="ps-empty-title" id="psEmptyTitle"></div>',
      '          <div class="ps-empty-desc" id="psEmptyDesc"></div>',
      '          <button type="button" class="ps-btn pri" id="psEmptyLoadBtn" style="padding:8px 15px;font-size:13px"></button>',
      '        </div>',
      '      </div>',
      '    </div>',
      '    <aside class="ps-sidepanel">',
      '      <div class="ps-tabs" role="tablist">',
      '        <button type="button" role="tab" class="ps-tab-btn act" id="psTabBtnAnalysis" data-ps-tab="analysis"></button>',
      '        <button type="button" role="tab" class="ps-tab-btn" id="psTabBtnChat" data-ps-tab="chat"></button>',
      '      </div>',
      '      <div class="ps-tab-pane act" id="psTabAnalysis"></div>',
      '      <div class="ps-tab-pane" id="psTabChat">',
      '        <div class="ps-role-bar" id="psRoleBar"></div>',
      '        <div id="psChatLog"></div>',
      '        <div class="ps-chat-foot">',
      '          <div style="display:flex;justify-content:space-between;align-items:center;gap:6px;flex-wrap:wrap">',
      '            <button type="button" class="ps-btn" id="psRefCurrentBtn" style="font-size:11.5px"></button>',
      '            <span class="ps-chat-status" id="psChatStatus"></span>',
      '          </div>',
      '          <form class="ps-chat-form" id="psChatForm">',
      '            <textarea id="psChatInput" rows="2"></textarea>',
      '            <button type="submit" class="ps-btn pri" id="psChatSendBtn" style="padding:8px 13px"></button>',
      '          </form>',
      '        </div>',
      '      </div>',
      '    </aside>',
      '  </div>',
      '</div>'
    ].join('\n');

    document.body.appendChild(bd);
    bindStudioEvents(bd);
    refreshChromeLabels();
    return bd;
  }

  function refreshChromeLabels() {
    var bd = document.getElementById('oePdfStudioBackdrop');
    if (!bd) return;

    var titleEl = bd.querySelector('#psStudioTitle');
    if (titleEl) {
      var bn = brandName();
      titleEl.textContent = '📄 ' + L(bn + ' AI — PDF Studio', bn + ' AI — PDF 审阅工作室', bn + ' AI — Estudio PDF');
    }

    var addrEl = bd.querySelector('#psAddrChip');
    if (addrEl) {
      if (studioState.address) {
        addrEl.textContent = studioState.address;
        addrEl.style.display = '';
      } else {
        addrEl.style.display = 'none';
      }
    }

    var privEl = bd.querySelector('#psPrivacyPill');
    if (privEl) {
      privEl.textContent = '🔒 ' + L(
        'Local In-Browser Viewer · Original files stay in your browser memory and are never saved or distributed by our server.',
        '本地浏览器内预览 · 原始文件仅保留在您的浏览器内存中，绝不由服务器保存或向外分发。',
        'Visor local en navegador · Los archivos permanecen en su memoria y nunca se guardan ni distribuyen.'
      );
    }

    var addBtn = bd.querySelector('#psAddFilesBtn');
    if (addBtn) addBtn.textContent = '+ ' + L('Load PDF / .ZIP', '加载本地 PDF / .ZIP', 'Cargar PDF / .ZIP');

    var closeBtn = bd.querySelector('#psCloseBtn');
    if (closeBtn) closeBtn.textContent = '✕ ' + L('Close', '关闭', 'Cerrar');

    var railBtn = bd.querySelector('#psToggleRailBtn');
    if (railBtn) railBtn.textContent = L('Pages', '页码栏', 'Páginas');

    var fitBtn = bd.querySelector('#psFitWidthBtn');
    if (fitBtn) {
      fitBtn.textContent = L('Fit Width', '适应宽度', 'Ajustar');
      fitBtn.classList.toggle('act', !!studioState.fitWidth);
    }

    var toolLabels = {
      pan: L('✋ Pan', '✋ 拖动', '✋ Mover'),
      circle: L('⭕ Circle', '⭕ 画圈', '⭕ Círculo'),
      arrow: L('↗ Arrow', '↗ 箭头', '↗ Flecha'),
      rect: L('▭ Highlight', '▭ 高亮框', '▭ Resaltar'),
      pen: L('✎ Pen', '✎ 画笔', '✎ Lápiz'),
      pin: L('📌 Pin', '📌 标记钉', '📌 Pin')
    };
    bd.querySelectorAll('[data-ps-tool]').forEach(function (btn) {
      var t = btn.getAttribute('data-ps-tool');
      if (toolLabels[t]) btn.textContent = toolLabels[t];
      btn.classList.toggle('act', studioState.tool === t);
    });

    var swWrap = bd.querySelector('#psColorSwatches');
    if (swWrap) {
      swWrap.innerHTML = COLORS.map(function (c) {
        return '<button type="button" class="ps-swatch' + (studioState.color === c.hex ? ' act' : '') +
          '" data-ps-color="' + c.hex + '" style="background:' + c.hex + '" title="' + esc(L(c.en, c.zh, c.es)) + '" aria-label="' + esc(c.en) + '"></button>';
      }).join('');
      swWrap.querySelectorAll('[data-ps-color]').forEach(function (sw) {
        sw.addEventListener('click', function () {
          studioState.color = sw.getAttribute('data-ps-color') || '#ef4444';
          refreshChromeLabels();
        });
      });
    }

    var undoBtn = bd.querySelector('#psUndoBtn');
    if (undoBtn) undoBtn.textContent = '↶ ' + L('Undo', '撤销', 'Deshacer');

    var clearBtn = bd.querySelector('#psClearPageBtn');
    if (clearBtn) clearBtn.textContent = '🗑 ' + L('Clear Page', '清空本页', 'Limpiar');

    var expBtn = bd.querySelector('#psExportPngBtn');
    if (expBtn) expBtn.textContent = '⬇ ' + L('Export PNG', '导出标注 PNG', 'Exportar PNG');

    var tabA = bd.querySelector('#psTabBtnAnalysis');
    if (tabA) tabA.textContent = '🎯 ' + L('Analysis & Pointers', 'AI 风险与定位', 'Análisis y Punteros');

    var tabC = bd.querySelector('#psTabBtnChat');
    if (tabC) tabC.textContent = '💬 ' + L('AI Chat', 'AI 对话', 'Chat IA');

    var emptyTitle = bd.querySelector('#psEmptyTitle');
    if (emptyTitle) {
      emptyTitle.textContent = L(
        'Select Local PDFs or a .ZIP Disclosure Packet',
        '请选择本地 PDF 或 .ZIP 披露压缩包',
        'Seleccione PDFs locales o un paquete .ZIP'
      );
    }

    var emptyDesc = bd.querySelector('#psEmptyDesc');
    if (emptyDesc) {
      emptyDesc.textContent = L(
        'Because our server never retains original disclosure files, saved/shared summaries do not store raw PDFs. Pick your local files to view, draw circles/arrows, and jump directly from AI findings.',
        '由于服务器绝不留存原始披露文件，已保存或分享的摘要中不含原始 PDF。选择您电脑中的本地文件即可预览、画圈标注，并从右侧 AI 风险卡片一键精确定位。',
        'Nuestro servidor no guarda archivos originales. Seleccione sus PDF o .zip locales para verlos, anotarlos y saltar desde los hallazgos de IA.'
      );
    }

    var emptyBtn = bd.querySelector('#psEmptyLoadBtn');
    if (emptyBtn) emptyBtn.textContent = '📂 ' + L('Choose Local PDF or .ZIP', '选择本地 PDF / .ZIP 文件', 'Elegir PDF o .ZIP Local');

    var refBtn = bd.querySelector('#psRefCurrentBtn');
    if (refBtn) refBtn.textContent = '📎 ' + L('Reference Current Page / Annotations', '引用当前页与标注', 'Referenciar Página / Anotaciones');

    var chatInp = bd.querySelector('#psChatInput');
    if (chatInp) {
      chatInp.setAttribute('placeholder', L(
        'Ask about this clause, defect, or property…',
        '针对当前页条款、瑕疵或本房源提问…',
        'Pregunte sobre esta cláusula, defecto o propiedad…'
      ));
    }

    var sendBtn = bd.querySelector('#psChatSendBtn');
    if (sendBtn) sendBtn.textContent = L('Send', '发送', 'Enviar');

    updateErrorBanner();
    updateScannedBannerForCurrentPage();
    renderRoleBar();
    renderAnalysisPanel();
  }

  /* -------------------- Side Panel Rendering (Tab 1 & Tab 2) -------------------- */
  function sevBadgeClass(sev) {
    var s = String(sev || '').toLowerCase();
    if (s.indexOf('crit') >= 0 || s.indexOf('high') >= 0) return 'crit';
    if (s.indexOf('maj') >= 0 || s.indexOf('warn') >= 0) return 'maj';
    if (s.indexOf('good') >= 0 || s.indexOf('low') >= 0) return 'good';
    return 'mod';
  }

  function renderFindingCardHtml(item, idx, groupTag) {
    var sCls = sevBadgeClass(item.severity);
    var costText = (item.costLow != null || item.costHigh != null)
      ? (' · ' + [fmtUsd(item.costLow), fmtUsd(item.costHigh)].filter(Boolean).join('–'))
      : '';
    var citeLabel = item.sourceDoc
      ? (item.sourceDoc + ' · p.' + (item.sourcePage || 1))
      : ('p.' + (item.sourcePage || 1));
    return [
      '<div class="ps-card ' + sCls + '">',
      '  <div class="ps-card-top">',
      '    <div class="ps-card-title">' + esc(item.title) + '</div>',
      '    <span class="ps-sev ' + sCls + '">' + esc(item.severity || 'Info') + '</span>',
      '  </div>',
      (item.detail ? '  <div class="ps-card-detail">' + esc(item.detail) + (costText ? '<strong>' + esc(costText) + '</strong>' : '') + '</div>' : ''),
      '  <div class="ps-card-foot">',
      '    <span class="ps-cite-tag">' + esc(citeLabel) + '</span>',
      '    <button type="button" class="ps-locate-btn" data-ps-locate="' + esc(groupTag + ':' + idx) + '">🎯 ' + esc(L('Point & Circle in PDF', '在 PDF 中画圈定位', 'Señalar en PDF')) + '</button>',
      '  </div>',
      '</div>'
    ].join('');
  }

  function renderAnalysisPanel() {
    var el = document.getElementById('psTabAnalysis');
    if (!el) return;
    var vm = studioState.analysisVm;
    var locateIndex = Object.create(null);
    var parts = [];

    if (vm.headline || vm.executiveSummary || vm.negotiationStrategy) {
      parts.push(
        '<div class="ps-sum-banner" id="psAiSummaryBanner">' +
        '<div><strong>' + esc(L('Analysis Summary:', '分析概览：', 'Resumen:')) + '</strong> ' + esc(vm.headline || '') +
        (vm.aiEnhanced ? (' <span class="ps-sev good" style="margin-left:6px">✨ ' + esc(vm.model || 'AI') + '</span>') : '') + '</div>' +
        (vm.executiveSummary ? ('<div style="margin-top:5px;line-height:1.45">' + esc(vm.executiveSummary) + '</div>') : '') +
        (vm.negotiationStrategy ? ('<div class="ps-sum-strat"><strong>' + esc(L('Negotiation Strategy:', '谈判策略：', 'Estrategia:')) + '</strong> ' + esc(vm.negotiationStrategy) + '</div>') : '') +
        '</div>'
      );
    }

    if (vm.redFlags.length) {
      parts.push('<div class="ps-sec-hdr">🚩 ' + esc(L('Red Flags', '核心红旗风险', 'Banderas Rojas')) + ' (' + vm.redFlags.length + ')</div>');
      vm.redFlags.forEach(function (rf, i) {
        locateIndex['rf:' + i] = rf;
        parts.push(renderFindingCardHtml(rf, i, 'rf'));
      });
    }

    if (vm.contradictions.length) {
      parts.push('<div class="ps-sec-hdr">⚠️ ' + esc(L('Contradictions', '跨文档矛盾项', 'Contradicciones')) + ' (' + vm.contradictions.length + ')</div>');
      vm.contradictions.forEach(function (c, i) {
        locateIndex['cx:' + i] = c;
        parts.push(renderFindingCardHtml(c, i, 'cx'));
      });
    }

    if (vm.prioritySections.length) {
      parts.push('<div class="ps-sec-hdr">📋 ' + esc(L('Priority Sections (1 → 6)', '优先级审阅章节 (1 → 6)', 'Secciones Prioritarias (1 → 6)')) + '</div>');
      vm.prioritySections.forEach(function (sec, sIdx) {
        if (!sec.present) {
          parts.push(
            '<div class="ps-card missing">' +
            '  <div class="ps-card-top"><div class="ps-card-title">#' + esc(sec.priority) + ' · ' + esc(sec.label) + '</div><span class="ps-sev mod">' + esc(L('Skipped', '缺失', 'Omitido')) + '</span></div>' +
            (sec.summary ? '<div class="ps-card-detail">' + esc(sec.summary) + '</div>' : '') +
            '</div>'
          );
          return;
        }
        parts.push(
          '<div class="ps-card">' +
          '  <div class="ps-card-top"><div class="ps-card-title">#' + esc(sec.priority) + ' · ' + esc(sec.label) + '</div><span class="ps-sev good">' + esc(L('Reviewed', '已审阅', 'Revisado')) + '</span></div>' +
          (sec.summary ? '<div class="ps-card-detail">' + esc(sec.summary) + '</div>' : '') +
          '</div>'
        );
        sec.items.forEach(function (it, iIdx) {
          var key = 'sec_' + sIdx + ':' + iIdx;
          locateIndex[key] = it;
          parts.push(renderFindingCardHtml(it, iIdx, 'sec_' + sIdx));
        });
      });
    }

    if (vm.defects.length) {
      parts.push('<div class="ps-sec-hdr">🔧 ' + esc(L('Itemized Defects', '瑕疵与维修清单', 'Defectos')) + ' (' + vm.defects.length + ')</div>');
      vm.defects.forEach(function (d, i) {
        locateIndex['def:' + i] = d;
        parts.push(renderFindingCardHtml(d, i, 'def'));
      });
    }

    if (vm.exceptions.length) {
      parts.push('<div class="ps-sec-hdr">📜 ' + esc(L('Title Exceptions & Liens', '产权例外与留置权', 'Excepciones de Título')) + ' (' + vm.exceptions.length + ')</div>');
      vm.exceptions.forEach(function (ex, i) {
        locateIndex['exc:' + i] = ex;
        parts.push(renderFindingCardHtml(ex, i, 'exc'));
      });
    }

    if (vm.hoaFlags.length) {
      parts.push('<div class="ps-sec-hdr">🏢 ' + esc(L('HOA Flags', 'HOA 物业风险项', 'Alertas HOA')) + ' (' + vm.hoaFlags.length + ')</div>');
      vm.hoaFlags.forEach(function (h, i) {
        locateIndex['hoa:' + i] = h;
        parts.push(renderFindingCardHtml(h, i, 'hoa'));
      });
    }

    if (!parts.length) {
      parts.push(
        '<div class="ps-sum-banner">' +
        esc(L(
          'No structured findings loaded yet. Run an analysis in Disclosures / Docs & AI, or switch to AI Chat to ask questions about the open document.',
          '暂未加载结构化分析结果。您可先在披露精读或文档 AI 中完成解析，或直接切换到“AI 对话”页签针对当前文件提问。',
          'Aún no hay hallazgos cargados. Ejecute un análisis o cambie a Chat IA para consultar sobre este documento.'
        )) +
        '</div>'
      );
    } else if (vm.disclaimer) {
      parts.push('<div class="ps-disclaimer-note">' + esc(vm.disclaimer) + '</div>');
    }

    el.innerHTML = parts.join('');
    el.querySelectorAll('.ps-locate-btn').forEach(function (btn) {
      btn.addEventListener('click', function () {
        var key = btn.getAttribute('data-ps-locate');
        var item = locateIndex[key];
        if (item) jumpToCitation(item);
      });
    });
  }

  function renderRoleBar() {
    var bar = document.getElementById('psRoleBar');
    if (!bar) return;
    var r = studioState.role || getActiveRole();
    studioState.role = r;
    var roles = [
      { k: 'buyer', lbl: L('Buyer', '买家', 'Comprador') },
      { k: 'seller', lbl: L('Seller', '卖家', 'Vendedor') },
      { k: 'agent', lbl: L('Agent', '经纪人', 'Agente') }
    ];
    bar.innerHTML = [
      '<span>🧭 <strong>' + esc(L('Role Lens:', '当前视角：', 'Rol:')) + '</strong> ' + esc(roleDisplayLabel(r)) + '</span>',
      '<div class="ps-role-pills">',
      roles.map(function (it) {
        return '<button type="button" class="ps-role-pill' + (r === it.k ? ' act' : '') + '" data-ps-role="' + it.k + '">' + esc(it.lbl) + '</button>';
      }).join(''),
      '</div>'
    ].join('');
    bar.querySelectorAll('[data-ps-role]').forEach(function (btn) {
      btn.addEventListener('click', function () {
        var nextR = btn.getAttribute('data-ps-role');
        studioState.role = setActiveRole(nextR);
        renderRoleBar();
      });
    });
  }

  function extractPageCitationsFromAnswer(answerText, citationsArr) {
    var found = [];
    var seen = Object.create(null);
    function addCite(pageNum, docHint, contextSnippet) {
      var p = parseInt(pageNum, 10);
      if (!isFinite(p) || p < 1 || p > 999) return;
      var k = String(docHint || '') + ':' + p;
      if (seen[k]) return;
      seen[k] = 1;
      found.push({
        sourceDoc: docHint || (studioState.docs[studioState.docIndex] ? studioState.docs[studioState.docIndex].name : ''),
        sourcePage: p,
        title: (docHint ? docHint + ' ' : '') + 'p.' + p,
        searchTerms: contextSnippet ? [contextSnippet] : []
      });
    }

    var text = String(answerText || '');
    var pgRe = /(?:([A-Za-z0-9_.\-\/]+\.(?:pdf|txt|md))[\s,;:\-()]{0,16})?(?:\(\s*p\.?\s*(\d+)\s*\)|\[Page\s+(\d+)\]|\bPage\s+(\d+)\b|\bp\.\s*(\d+)\b)/gi;
    var m;
    while ((m = pgRe.exec(text)) !== null) {
      var pg = m[2] || m[3] || m[4] || m[5];
      var start = Math.max(0, m.index - 45);
      var end = Math.min(text.length, m.index + m[0].length + 45);
      addCite(pg, m[1] || '', text.slice(start, end));
    }
    (Array.isArray(citationsArr) ? citationsArr : []).forEach(function (c) {
      var s = String(c || '');
      var cm = s.match(/(?:([A-Za-z0-9_.\-\/]+\.(?:pdf|txt|md))[\s,:\-]{0,12})?(?:p\.?\s*(\d+)|Page\s+(\d+))/i);
      if (cm) addCite(cm[2] || cm[3], cm[1] || '', s);
    });
    return found.slice(0, 6);
  }

  function renderChatLog() {
    var log = document.getElementById('psChatLog');
    if (!log) return;
    var out = [];
    if (!studioState.chatHistory.length) {
      out.push(
        '<div class="ps-msg a">' +
        esc(L(
          'Ask questions grounded in this property’s verified report and your active PDF page. Click "Reference Current Page / Annotations" or drop a Pin (#1, #2) on the document first.',
          '欢迎针对本房源核实报告与当前浏览的 PDF 页面提问。您可点击下方“引用当前页与标注”或在文档上放置标记钉（#1、#2）。',
          'Haga preguntas basadas en el informe verificado y la página actual del PDF. Puede usar "Referenciar Página" o colocar Pines (#1, #2).'
        )) +
        '</div>'
      );
    }
    studioState.chatHistory.forEach(function (entry, eIdx) {
      if (entry.role === 'user') {
        out.push('<div class="ps-msg u">' + esc(entry.content) + '</div>');
        return;
      }
      var formatted = esc(entry.content || '')
        .replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>')
        .replace(/\n/g, '<br>');
      var cites = extractPageCitationsFromAnswer(entry.content, entry.citations);
      var citeHtml = '';
      if (cites.length) {
        citeHtml = '<div class="ps-chat-cites">' + cites.map(function (c, cIdx) {
          return '<button type="button" class="ps-chat-cite-btn" data-ps-chat-cite="' + eIdx + ':' + cIdx + '">🎯 ' +
            esc(L('Point on PDF', '定位到 PDF', 'Señalar en PDF')) + ' (p.' + esc(c.sourcePage) + ')</button>';
        }).join('') + '</div>';
      }
      out.push('<div class="ps-msg a"><div>' + formatted + '</div>' + citeHtml + '</div>');
    });
    log.innerHTML = out.join('');
    log.querySelectorAll('[data-ps-chat-cite]').forEach(function (btn) {
      btn.addEventListener('click', function () {
        var parts = String(btn.getAttribute('data-ps-chat-cite') || '').split(':');
        var ent = studioState.chatHistory[Number(parts[0])];
        if (!ent) return;
        var cites = extractPageCitationsFromAnswer(ent.content, ent.citations);
        var c = cites[Number(parts[1])];
        if (c) jumpToCitation(c);
      });
    });
    log.scrollTop = log.scrollHeight;
  }

  async function submitStudioChat(rawQuestion) {
    var q = String(rawQuestion || '').trim();
    if (!q || studioState.chatSending) return;
    var inp = document.getElementById('psChatInput');
    var sendBtn = document.getElementById('psChatSendBtn');
    var statusEl = document.getElementById('psChatStatus');
    var curDoc = studioState.docs[studioState.docIndex];

    studioState.chatSending = true;
    if (inp) inp.value = '';
    studioState.chatDraft = '';
    if (sendBtn) sendBtn.disabled = true;
    if (statusEl) statusEl.textContent = L('Checking records…', '正在核查档案…', 'Consultando…');

    var histPayload = studioState.chatHistory
      .filter(function (x) { return !x.refused && x.role && x.content; })
      .map(function (x) { return { role: x.role, content: x.content }; });

    studioState.chatHistory.push({ role: 'user', content: q });
    renderChatLog();

    try {
      var ansData = null;
      var activeRole = (studioState.role === 'buyer' || studioState.role === 'seller' || studioState.role === 'agent')
        ? studioState.role : undefined;

      if (typeof studioState.onAskChat === 'function') {
        ansData = await studioState.onAskChat({
          slug: studioState.slug,
          question: q,
          history: histPayload,
          role: activeRole,
          docName: curDoc ? curDoc.name : '',
          page: studioState.currentPage
        });
      } else {
        if (!studioState.slug) {
          throw new Error(L(
            'Look up or open a property report first to enable grounded /api/ask chat.',
            '请先查询房源或打开房产报告，以启用基于档案的 /api/ask 对话。',
            'Consulte o abra un informe de propiedad primero para activar el chat /api/ask.'
          ));
        }
        var res = await fetch('/api/ask', {
          method: 'POST',
          headers: {
            'Content-Type': 'application/json',
            'X-OpenEstate-Client-Id': getClientId()
          },
          body: JSON.stringify({
            slug: studioState.slug,
            question: q,
            history: histPayload,
            role: activeRole
          })
        });
        ansData = await res.json();
        if (!res.ok || !ansData || !ansData.ok) {
          throw new Error((ansData && ansData.error) || ('HTTP ' + res.status));
        }
      }

      var answerStr = typeof ansData === 'string'
        ? ansData
        : String((ansData && (ansData.answer || ansData.text)) || '');
      studioState.chatHistory.push({
        role: 'assistant',
        content: answerStr,
        citations: ansData && Array.isArray(ansData.citations) ? ansData.citations : [],
        refused: Boolean(ansData && ansData.refused)
      });
      if (statusEl) {
        statusEl.textContent = (ansData && ansData.ms != null)
          ? L('Answered', '已回复', 'Respondido') + ' (' + (Number(ansData.ms) / 1000).toFixed(1) + 's)'
          : '';
      }
    } catch (err) {
      studioState.chatHistory.push({
        role: 'assistant',
        content: L('Unable to complete AI chat request: ', 'AI 对话请求未完成：', 'No se pudo completar: ') + (err && err.message ? err.message : String(err)),
        refused: true
      });
      if (statusEl) statusEl.textContent = '';
    } finally {
      studioState.chatSending = false;
      if (sendBtn) sendBtn.disabled = false;
      renderChatLog();
    }
  }

  /* -------------------- PDF / Text / Image Rendering & Canvas Overlay -------------------- */
  function syncFileSelect() {
    var sel = document.getElementById('psFileSelect');
    var badge = document.getElementById('psFileCountBadge');
    if (badge) badge.textContent = String(studioState.docs.length);
    if (!sel) return;
    if (!studioState.docs.length) {
      sel.innerHTML = '<option value="">' + esc(L('No local files loaded', '暂未加载本地文件', 'Sin archivos locales')) + '</option>';
      sel.disabled = true;
      return;
    }
    sel.disabled = false;
    sel.innerHTML = studioState.docs.map(function (d, i) {
      return '<option value="' + i + '"' + (i === studioState.docIndex ? ' selected' : '') + '>' +
        esc(d.name + (d.sourceZipName ? ' (' + d.sourceZipName + ')' : '')) + '</option>';
    }).join('');
  }

  function syncPageControls() {
    var doc = studioState.docs[studioState.docIndex];
    var total = doc ? (doc.numPages || 1) : 1;
    var pgInp = document.getElementById('psPageInput');
    var pgTot = document.getElementById('psPageCount');
    var prevBtn = document.getElementById('psPrevPageBtn');
    var nextBtn = document.getElementById('psNextPageBtn');
    var zoomPct = document.getElementById('psZoomPct');
    if (pgInp) {
      pgInp.value = String(studioState.currentPage);
      pgInp.max = String(total);
      pgInp.disabled = !doc;
    }
    if (pgTot) pgTot.textContent = '/ ' + total;
    if (prevBtn) prevBtn.disabled = !doc || studioState.currentPage <= 1;
    if (nextBtn) nextBtn.disabled = !doc || studioState.currentPage >= total;
    if (zoomPct) zoomPct.textContent = Math.round(studioState.zoom * 100) + '%';
    renderThumbRail();
  }

  function renderThumbRail() {
    var rail = document.getElementById('psThumbRail');
    if (!rail) return;
    rail.classList.toggle('collapsed', !!studioState.railCollapsed);
    var doc = studioState.docs[studioState.docIndex];
    if (!doc) {
      rail.innerHTML = '';
      return;
    }
    var count = Math.min(doc.numPages || 1, 200);
    var html = [];
    for (var p = 1; p <= count; p++) {
      var annots = getAnnotationsFor(doc, p);
      var hasPtr = studioState.activeAiPointer && studioState.activeAiPointer.key === docPageKey(doc, p);
      var badges = [];
      if (hasPtr) badges.push('🎯');
      if (annots.length) badges.push('✎' + annots.length);
      html.push(
        '<button type="button" class="ps-rail-item' + (p === studioState.currentPage ? ' act' : '') +
        '" data-ps-rail-page="' + p + '"><span>' + esc(L('Page ', '第 ', 'Pág. ') + p) + '</span>' +
        (badges.length ? '<span class="ps-rail-dot">' + esc(badges.join(' ')) + '</span>' : '') +
        '</button>'
      );
    }
    rail.innerHTML = html.join('');
    rail.querySelectorAll('[data-ps-rail-page]').forEach(function (btn) {
      btn.addEventListener('click', function () {
        goToPage(Number(btn.getAttribute('data-ps-rail-page')) || 1);
      });
    });
  }

  async function ensureDocLoaded(doc) {
    if (!doc) return;
    if (doc.kind === 'pdf') {
      var hasOverride = false;
      try { hasOverride = Boolean(window.__OPEN_ESTATE_PDFJS_OVERRIDE__); } catch (_) {}
      if (doc.pdfDoc || (doc.fallbackPages && !hasOverride)) return;
      var pdfjs = await loadPdfJs();
      if (pdfjs && typeof pdfjs.getDocument === 'function') {
        try {
          var loadingTask = pdfjs.getDocument({
            data: doc.bytes.slice(),
            verbosity: 0,
            useSystemFonts: true,
            isEvalSupported: false
          });
          doc.pdfDoc = await loadingTask.promise;
          doc.numPages = doc.pdfDoc.numPages || 1;
          doc.fallbackPages = null;
          return;
        } catch (_) {}
      }
      if (!doc.fallbackPages) {
        var det = await parsePdfFallbackDetail(doc.bytes, doc.name);
        doc.fallbackPages = det.pages;
        doc.fallbackPageImages = det.pageImages || null;
        doc.embeddedJpegs = det.embeddedJpegs || null;
        doc.scannedPages = det.scannedPages || Object.create(null);
        doc.numPages = doc.fallbackPages.length || 1;
      }
    } else if (doc.kind === 'image') {
      if (doc.imageEl) return;
      doc.imageEl = await loadImageFromBytes(doc.bytes, doc.mime || 'image/png');
      doc.numPages = 1;
      doc.scannedPages[1] = true;
    } else if (doc.kind === 'html') {
      if (!doc.fallbackPages) {
        doc.fallbackPages = stripHtmlToReadableLines(bytesToText(doc.bytes));
        doc.numPages = doc.fallbackPages.length || 1;
      }
    } else {
      if (!doc.fallbackPages) {
        doc.fallbackPages = paginatePlainText(bytesToText(doc.bytes));
        doc.numPages = doc.fallbackPages.length || 1;
      }
    }
  }

  function wrapCanvasParagraphByWords(ctx, rawLine, maxPx) {
    var s = String(rawLine == null ? '' : rawLine).replace(/\r/g, '');
    if (!s.trim()) return [''];
    if (ctx.measureText(s).width <= maxPx) return [s];

    var words = s.trim().split(/\s+/);
    var out = [];
    var cur = '';
    for (var i = 0; i < words.length; i++) {
      var w = words[i];
      if (!cur) {
        if (ctx.measureText(w).width <= maxPx) {
          cur = w;
        } else {
          // Single unbroken token wider than maxPx: break by character only for that token
          var seg = '';
          for (var ci = 0; ci < w.length; ci++) {
            var ch = w.charAt(ci);
            if (seg && ctx.measureText(seg + ch).width > maxPx) {
              out.push(seg);
              seg = ch;
            } else {
              seg += ch;
            }
          }
          cur = seg;
        }
      } else {
        var candidate = cur + ' ' + w;
        if (ctx.measureText(candidate).width <= maxPx) {
          cur = candidate;
        } else {
          out.push(cur);
          cur = '';
          i--;
        }
      }
    }
    if (cur) out.push(cur);
    return out.length ? out : [''];
  }

  function renderTextLinesToCanvas(ctx, cssW, cssH, scale, doc, pageNum, lines) {
    ctx.fillStyle = '#ffffff';
    ctx.fillRect(0, 0, cssW, cssH);

    // Header band
    ctx.fillStyle = '#f8fafc';
    ctx.fillRect(0, 0, cssW, Math.round(44 * scale));
    ctx.strokeStyle = '#e2e8f0';
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(0, Math.round(44 * scale));
    ctx.lineTo(cssW, Math.round(44 * scale));
    ctx.stroke();

    ctx.fillStyle = '#0f766e';
    ctx.font = '700 ' + Math.round(11 * scale) + 'px Inter, system-ui, sans-serif';
    ctx.fillText(doc.name + ' — p.' + pageNum + ' / ' + doc.numPages, Math.round(32 * scale), Math.round(27 * scale));

    var startY = Math.round(72 * scale);
    var lineH = Math.round(17 * scale);
    var leftX = Math.round(32 * scale);
    var maxLinePx = Math.max(120, cssW - leftX * 2);
    var fontPx = Math.max(9, Math.round(11.5 * scale));
    ctx.font = '500 ' + fontPx + 'px ui-monospace, SFMono-Regular, Menlo, monospace';
    ctx.fillStyle = '#0f172a';

    var visualLines = [];
    var safeLines = Array.isArray(lines) ? lines : [];
    for (var li = 0; li < safeLines.length; li++) {
      var wrapped = wrapCanvasParagraphByWords(ctx, safeLines[li], maxLinePx);
      for (var wi = 0; wi < wrapped.length; wi++) {
        visualLines.push(wrapped[wi]);
      }
    }

    var boxes = [];
    for (var i = 0; i < visualLines.length; i++) {
      var line = String(visualLines[i] || '');
      var y = startY + i * lineH;
      if (y > cssH - Math.round(24 * scale)) break;
      ctx.fillText(line, leftX, y);
      if (line.trim()) {
        var tw = ctx.measureText(line).width;
        boxes.push({
          str: line,
          nx: leftX / cssW,
          ny: Math.max(0, (y - fontPx) / cssH),
          nw: Math.min(0.92, Math.max(0.08, tw / cssW)),
          nh: Math.max(0.015, lineH / cssH)
        });
      }
    }
    doc.textBoxesByPage[pageNum] = boxes;
  }

  function normalizeTextLayerDomSeparators(container) {
    if (!container || typeof document === 'undefined' || typeof document.createTextNode !== 'function') return;
    try {
      var kids = container.childNodes ? Array.prototype.slice.call(container.childNodes) : [];
      for (var i = 0; i < kids.length; i++) {
        var node = kids[i];
        var next = kids[i + 1];
        if (!node || !next) continue;
        var tag = String(node.nodeName || '').toUpperCase();
        var nextTag = String(next.nodeName || '').toUpperCase();
        if (tag === 'BR' && nextTag !== '#TEXT') {
          container.insertBefore(document.createTextNode('\n'), next);
        } else if (tag === 'SPAN' && nextTag === 'SPAN') {
          var aTxt = String(node.textContent || '');
          var bTxt = String(next.textContent || '');
          if (aTxt && bTxt && !/\s$/.test(aTxt) && !/^\s/.test(bTxt)) {
            container.insertBefore(document.createTextNode(' '), next);
          }
        }
      }
    } catch (_) {}
  }

  function canvasHasRenderedInk(ctx, w, h) {
    if (!ctx || typeof ctx.getImageData !== 'function' || w <= 0 || h <= 0) return true;
    try {
      var data = ctx.getImageData(0, 0, w, h).data;
      var stride = Math.max(4, Math.floor(data.length / 4096) * 4);
      for (var i = 0; i < data.length; i += stride) {
        if (data[i + 3] > 0 && (data[i] < 248 || data[i + 1] < 248 || data[i + 2] < 248)) {
          return true;
        }
      }
      return false;
    } catch (_) {
      return true;
    }
  }

  function extractPdfJsPageTextBoxes(textContent, viewport, scale) {
    var raw = [];
    var items = (textContent && Array.isArray(textContent.items)) ? textContent.items : [];
    var vpW = (viewport && viewport.width) || (612 * (scale || 1));
    var vpH = (viewport && viewport.height) || (792 * (scale || 1));
    var toVp = (viewport && typeof viewport.convertToViewportPoint === 'function')
      ? function (x, y) { return viewport.convertToViewportPoint(x, y); }
      : function (x, y) { var s = scale || 1; return [x * s, vpH - y * s]; };

    for (var i = 0; i < items.length; i++) {
      var it = items[i];
      if (!it || !it.str || !it.str.trim() || !Array.isArray(it.transform) || it.transform.length < 6) continue;
      var tr = it.transform;
      var fontH = Math.max(Math.hypot(tr[2], tr[3]), Math.hypot(tr[0], tr[1]), it.height || 10, 6);
      var itemW = (typeof it.width === 'number' && it.width > 0) ? it.width : (it.str.length * fontH * 0.5);
      var p1 = toVp(tr[4], tr[5]);
      var p2 = toVp(tr[4] + itemW, tr[5] + fontH);
      var left = Math.min(p1[0], p2[0]);
      var right = Math.max(p1[0], p2[0]);
      var top = Math.min(p1[1], p2[1]);
      var bottom = Math.max(p1[1], p2[1]);
      raw.push({
        str: it.str,
        vx: left,
        vy: p1[1],
        nx: Math.max(0, left / vpW),
        ny: Math.max(0, top / vpH),
        nw: Math.max(0.01, (right - left) / vpW),
        nh: Math.max(0.012, (bottom - top) / vpH)
      });
    }
    // Sort top-to-bottom, left-to-right for stable line grouping
    raw.sort(function (a, b) {
      if (Math.abs(a.vy - b.vy) > 6) return a.vy - b.vy;
      return a.vx - b.vx;
    });
    return raw;
  }

  async function renderCurrentPage() {
    var stage = document.getElementById('psPageStage');
    var empty = document.getElementById('psEmptyStage');
    var pdfCanvas = document.getElementById('psPdfCanvas');
    var annotCanvas = document.getElementById('psAnnotCanvas');
    var vp = document.getElementById('psViewport');
    updateErrorBanner();
    if (!stage || !empty || !pdfCanvas || !annotCanvas || !vp) return;

    var doc = studioState.docs[studioState.docIndex];
    if (!doc) {
      stage.style.display = 'none';
      empty.style.display = '';
      updateScannedBannerForCurrentPage();
      syncPageControls();
      return;
    }

    empty.style.display = 'none';
    stage.style.display = '';

    var mySeq = ++studioState.renderSeq;
    if (studioState.activeRenderTask && typeof studioState.activeRenderTask.cancel === 'function') {
      try { studioState.activeRenderTask.cancel(); } catch (_) {}
      studioState.activeRenderTask = null;
    }

    await ensureDocLoaded(doc);
    if (mySeq !== studioState.renderSeq) return;

    studioState.currentPage = Math.max(1, Math.min(doc.numPages || 1, studioState.currentPage || 1));
    var dpr = Math.min(window.devicePixelRatio || 1, 2);
    var availW = Math.max(320, (vp.clientWidth || 760) - 48);

    var baseW = 612;
    var baseH = 792;
    var pdfPage = null;
    var fallbackPageImg = (doc.fallbackPageImages && doc.fallbackPageImages[studioState.currentPage - 1]) || null;

    if (doc.kind === 'pdf' && doc.pdfDoc) {
      pdfPage = await doc.pdfDoc.getPage(studioState.currentPage);
      if (mySeq !== studioState.renderSeq) return;
      var vp1 = pdfPage.getViewport({ scale: 1.0 });
      baseW = vp1.width || 612;
      baseH = vp1.height || 792;
    } else if (doc.kind === 'image' && doc.imageEl) {
      baseW = doc.imageEl.naturalWidth || 800;
      baseH = doc.imageEl.naturalHeight || 600;
    } else if (fallbackPageImg && fallbackPageImg.naturalWidth > 0 && fallbackPageImg.naturalHeight > 0) {
      baseW = fallbackPageImg.naturalWidth;
      baseH = fallbackPageImg.naturalHeight;
    }

    if (studioState.fitWidth) {
      studioState.zoom = Math.max(0.4, Math.min(2.6, availW / baseW));
    }
    var scale = studioState.zoom;
    var cssW = Math.max(240, Math.round(baseW * scale));
    var cssH = Math.max(240, Math.round(baseH * scale));

    stage.style.width = cssW + 'px';
    stage.style.height = cssH + 'px';

    pdfCanvas.width = Math.round(cssW * dpr);
    pdfCanvas.height = Math.round(cssH * dpr);
    pdfCanvas.style.width = cssW + 'px';
    pdfCanvas.style.height = cssH + 'px';

    annotCanvas.width = Math.round(cssW * dpr);
    annotCanvas.height = Math.round(cssH * dpr);
    annotCanvas.style.width = cssW + 'px';
    annotCanvas.style.height = cssH + 'px';

    var textLayerEl = document.getElementById('psTextLayer');
    if (textLayerEl) {
      textLayerEl.innerHTML = '';
      textLayerEl.style.width = cssW + 'px';
      textLayerEl.style.height = cssH + 'px';
      if (textLayerEl.style && typeof textLayerEl.style.setProperty === 'function') {
        textLayerEl.style.setProperty('--scale-factor', String(scale));
      }
    }

    var ctx = pdfCanvas.getContext('2d');
    if (pdfPage) {
      ctx.setTransform(1, 0, 0, 1, 0, 0);
      ctx.fillStyle = '#ffffff';
      ctx.fillRect(0, 0, pdfCanvas.width, pdfCanvas.height);
      var renderVp = pdfPage.getViewport({ scale: scale * dpr });
      var task = pdfPage.render({ canvasContext: ctx, viewport: renderVp });
      studioState.activeRenderTask = task;
      try {
        await task.promise;
      } catch (err) {
        if (err && err.name === 'RenderingCancelledException') return;
      } finally {
        if (studioState.activeRenderTask === task) studioState.activeRenderTask = null;
      }
      if (mySeq !== studioState.renderSeq) return;
      try {
        var tc = await pdfPage.getTextContent();
        var cssVp = pdfPage.getViewport({ scale: scale });
        var boxes = extractPdfJsPageTextBoxes(tc, cssVp, scale);
        doc.textBoxesByPage[studioState.currentPage] = boxes;
        if (!doc.scannedPages) doc.scannedPages = Object.create(null);
        doc.scannedPages[studioState.currentPage] = (boxes.length === 0);

        if (boxes.length > 0) {
          var pdfjsLib = await loadPdfJs();
          if (textLayerEl && pdfjsLib && typeof pdfjsLib.TextLayer === 'function' && tc) {
            var tl = new pdfjsLib.TextLayer({
              textContentSource: tc,
              container: textLayerEl,
              viewport: cssVp
            });
            if (tl && typeof tl.render === 'function') {
              await tl.render();
              normalizeTextLayerDomSeparators(textLayerEl);
            }
          }
        } else if (!canvasHasRenderedInk(ctx, pdfCanvas.width, pdfCanvas.height)) {
          // Safety net if a scanned /DCTDecode PDF page rendered blank: decode embedded JPEG directly
          if (!doc.fallbackPageImages) {
            var scanDet = await parsePdfFallbackDetail(doc.bytes, doc.name);
            doc.fallbackPageImages = scanDet.pageImages || null;
          }
          var scanImg = doc.fallbackPageImages && doc.fallbackPageImages[studioState.currentPage - 1];
          if (scanImg) {
            ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
            ctx.drawImage(scanImg, 0, 0, cssW, cssH);
          }
        }
      } catch (_) {}
    } else if (doc.kind === 'image' && doc.imageEl) {
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      ctx.fillStyle = '#ffffff';
      ctx.fillRect(0, 0, cssW, cssH);
      ctx.drawImage(doc.imageEl, 0, 0, cssW, cssH);
      doc.textBoxesByPage[studioState.currentPage] = [];
      doc.scannedPages[studioState.currentPage] = true;
    } else if (fallbackPageImg) {
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      ctx.fillStyle = '#ffffff';
      ctx.fillRect(0, 0, cssW, cssH);
      ctx.drawImage(fallbackPageImg, 0, 0, cssW, cssH);
      doc.textBoxesByPage[studioState.currentPage] = [];
      if (!doc.scannedPages) doc.scannedPages = Object.create(null);
      doc.scannedPages[studioState.currentPage] = true;
    } else {
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      var lines = (doc.fallbackPages && doc.fallbackPages[studioState.currentPage - 1]) || [];
      renderTextLinesToCanvas(ctx, cssW, cssH, scale, doc, studioState.currentPage, lines);
    }

    studioState.renderedDocName = doc.name;
    studioState.renderedPage = studioState.currentPage;
    syncPageControls();
    updateCanvasCursor();
    redrawAnnotationCanvas();
    updateErrorBanner();
    updateScannedBannerForCurrentPage();
    updateAiBannerForCurrentPage();
  }

  async function loadAndRenderCurrentDoc() {
    studioState.currentPage = 1;
    await renderCurrentPage();
  }

  async function goToPage(pg) {
    var doc = studioState.docs[studioState.docIndex];
    if (!doc) return;
    await ensureDocLoaded(doc);
    var target = Math.max(1, Math.min(doc.numPages || 1, parseInt(pg, 10) || 1));
    studioState.currentPage = target;
    return renderCurrentPage();
  }

  /* -------------------- Annotation & AI Pointer Canvas Renderer -------------------- */
  function drawArrowHead(ctx, x1, y1, x2, y2, size) {
    var angle = Math.atan2(y2 - y1, x2 - x1);
    ctx.beginPath();
    ctx.moveTo(x2, y2);
    ctx.lineTo(x2 - size * Math.cos(angle - Math.PI / 7), y2 - size * Math.sin(angle - Math.PI / 7));
    ctx.lineTo(x2 - size * Math.cos(angle + Math.PI / 7), y2 - size * Math.sin(angle + Math.PI / 7));
    ctx.closePath();
    ctx.fill();
  }

  function drawSingleAnnotation(ctx, a, w, h) {
    var col = a.color || '#ef4444';
    ctx.save();
    ctx.strokeStyle = col;
    ctx.fillStyle = col;
    ctx.lineWidth = Math.max(2, Math.min(4, w * 0.004));
    ctx.lineCap = 'round';
    ctx.lineJoin = 'round';

    if (a.type === 'circle') {
      var cx = a.cx * w;
      var cy = a.cy * h;
      var rx = Math.max(8, Math.abs(a.rx) * w);
      var ry = Math.max(8, Math.abs(a.ry) * h);
      ctx.beginPath();
      ctx.ellipse(cx, cy, rx, ry, 0, 0, Math.PI * 2);
      ctx.stroke();
    } else if (a.type === 'arrow') {
      var ax1 = a.x1 * w;
      var ay1 = a.y1 * h;
      var ax2 = a.x2 * w;
      var ay2 = a.y2 * h;
      ctx.beginPath();
      ctx.moveTo(ax1, ay1);
      ctx.lineTo(ax2, ay2);
      ctx.stroke();
      drawArrowHead(ctx, ax1, ay1, ax2, ay2, Math.max(11, ctx.lineWidth * 4.2));
    } else if (a.type === 'rect') {
      var rx1 = a.x * w;
      var ry1 = a.y * h;
      var rw = a.w * w;
      var rh = a.h * h;
      ctx.save();
      ctx.globalAlpha = 0.20;
      ctx.fillRect(rx1, ry1, rw, rh);
      ctx.restore();
      ctx.strokeRect(rx1, ry1, rw, rh);
    } else if (a.type === 'pen') {
      var pts = a.points || [];
      if (pts.length === 1) {
        ctx.beginPath();
        ctx.arc(pts[0].x * w, pts[0].y * h, ctx.lineWidth, 0, Math.PI * 2);
        ctx.fill();
      } else if (pts.length > 1) {
        ctx.beginPath();
        ctx.moveTo(pts[0].x * w, pts[0].y * h);
        for (var i = 1; i < pts.length; i++) {
          ctx.lineTo(pts[i].x * w, pts[i].y * h);
        }
        ctx.stroke();
      }
    } else if (a.type === 'pin') {
      var px = a.x * w;
      var py = a.y * h;
      var r = Math.max(12, Math.min(16, w * 0.02));
      ctx.shadowColor = 'rgba(15,23,42,0.28)';
      ctx.shadowBlur = 6;
      ctx.beginPath();
      ctx.arc(px, py, r, 0, Math.PI * 2);
      ctx.fill();
      ctx.shadowBlur = 0;
      ctx.lineWidth = 2;
      ctx.strokeStyle = '#ffffff';
      ctx.stroke();

      ctx.fillStyle = '#ffffff';
      ctx.font = '800 ' + Math.round(r * 0.92) + 'px Inter, system-ui, sans-serif';
      ctx.textAlign = 'center';
      ctx.textBaseline = 'middle';
      ctx.fillText('#' + (a.pinNumber || 1), px, py + 0.5);
    }
    ctx.restore();
  }

  function drawAiPointerOverlay(ctx, ptr, w, h, nowMs) {
    if (!ptr || ptr.key !== docPageKey()) return;
    var isCrit = /crit|high/i.test(String(ptr.severity || ''));
    var color = isCrit ? '#ef4444' : (/maj/i.test(String(ptr.severity || '')) ? '#f59e0b' : '#0f766e');
    var elapsed = Math.max(0, nowMs - ptr.startMs);
    var pulseActive = elapsed < 4500;
    var wave = pulseActive ? (0.5 + 0.5 * Math.sin(elapsed * 0.008)) : 0.4;

    ctx.save();
    if (ptr.matched) {
      var cx = (ptr.nx + ptr.nw * 0.5) * w;
      var cy = (ptr.ny + ptr.nh * 0.5) * h;
      var rx = Math.max(26, ptr.nw * 0.56 * w);
      var ry = Math.max(16, ptr.nh * 0.95 * h);

      // Outer pulsing halo ring
      ctx.strokeStyle = color;
      ctx.globalAlpha = 0.25 + 0.35 * wave;
      ctx.lineWidth = 5 + 3 * wave;
      ctx.beginPath();
      ctx.ellipse(cx, cy, rx + 6 * wave, ry + 5 * wave, 0, 0, Math.PI * 2);
      ctx.stroke();

      // Inner crisp callout circle
      ctx.globalAlpha = 0.95;
      ctx.lineWidth = 2.8;
      ctx.beginPath();
      ctx.ellipse(cx, cy, rx, ry, 0, 0, Math.PI * 2);
      ctx.stroke();

      // Leader arrow + tag pill above/near circle
      var rawTitle = String(ptr.title || '').trim();
      var shortTitle = rawTitle.length > 34 ? (rawTitle.slice(0, 33).trim() + '…') : rawTitle;
      var tagLabel = 'AI Pointer: ' + shortTitle;
      ctx.font = '700 12px Inter, system-ui, sans-serif';
      var textW = ctx.measureText(tagLabel).width;
      var pillW = Math.min(w - 20, textW + 18);
      var pillH = 24;
      var pillX = Math.max(8, Math.min(w - pillW - 8, cx - pillW * 0.5));
      var aboveY = cy - ry - pillH - 14;
      var pillY = aboveY >= 8 ? aboveY : Math.min(h - pillH - 8, cy + ry + 14);

      var targetY = aboveY >= 8 ? (cy - ry - 2) : (cy + ry + 2);
      var startY = aboveY >= 8 ? (pillY + pillH) : pillY;
      ctx.strokeStyle = color;
      ctx.fillStyle = color;
      ctx.lineWidth = 2.2;
      ctx.beginPath();
      ctx.moveTo(cx, startY);
      ctx.lineTo(cx, targetY);
      ctx.stroke();
      drawArrowHead(ctx, cx, startY, cx, targetY, 9);

      ctx.fillStyle = color;
      ctx.beginPath();
      if (typeof ctx.roundRect === 'function') ctx.roundRect(pillX, pillY, pillW, pillH, 999);
      else ctx.rect(pillX, pillY, pillW, pillH);
      ctx.fill();

      ctx.fillStyle = '#ffffff';
      ctx.textBaseline = 'middle';
      ctx.fillText(tagLabel, pillX + 9, pillY + pillH / 2, pillW - 18);
    } else {
      // Scanned or unmatched text page: crisp top page pointer frame
      var bw = Math.min(w - 24, 420);
      var bx = (w - bw) / 2;
      var by = 14;
      ctx.globalAlpha = 0.92;
      ctx.fillStyle = color;
      ctx.beginPath();
      if (typeof ctx.roundRect === 'function') ctx.roundRect(bx, by, bw, 28, 999);
      else ctx.rect(bx, by, bw, 28);
      ctx.fill();
      ctx.fillStyle = '#ffffff';
      ctx.font = '700 12px Inter, system-ui, sans-serif';
      ctx.textAlign = 'center';
      ctx.textBaseline = 'middle';
      ctx.fillText('AI Pointer (p.' + studioState.currentPage + '): ' + String(ptr.title || '').slice(0, 44), w / 2, by + 14, bw - 16);
    }
    ctx.restore();
  }

  function redrawAnnotationCanvas(previewItem) {
    var annotCanvas = document.getElementById('psAnnotCanvas');
    if (!annotCanvas) return;
    var cssW = parseFloat(annotCanvas.style.width) || annotCanvas.clientWidth || 1;
    var cssH = parseFloat(annotCanvas.style.height) || annotCanvas.clientHeight || 1;
    var dpr = Math.min(window.devicePixelRatio || 1, 2);
    var ctx = annotCanvas.getContext('2d');
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, cssW, cssH);

    var list = getAnnotationsFor();
    for (var i = 0; i < list.length; i++) {
      drawSingleAnnotation(ctx, list[i], cssW, cssH);
    }
    if (previewItem) {
      drawSingleAnnotation(ctx, previewItem, cssW, cssH);
    }
    if (studioState.activeAiPointer && studioState.activeAiPointer.key === docPageKey()) {
      drawAiPointerOverlay(ctx, studioState.activeAiPointer, cssW, cssH, performance.now());
    }
  }

  function startPulseLoop() {
    if (studioState.pulseRaf) cancelAnimationFrame(studioState.pulseRaf);
    function tick() {
      var ptr = studioState.activeAiPointer;
      if (!studioState.isOpen || !ptr || ptr.key !== docPageKey()) {
        studioState.pulseRaf = 0;
        return;
      }
      redrawAnnotationCanvas();
      if (performance.now() - ptr.startMs < 4600) {
        studioState.pulseRaf = requestAnimationFrame(tick);
      } else {
        studioState.pulseRaf = 0;
      }
    }
    studioState.pulseRaf = requestAnimationFrame(tick);
  }

  function updateAiBannerForCurrentPage() {
    var banner = document.getElementById('psAiBanner');
    var txt = document.getElementById('psAiBannerText');
    if (!banner || !txt) return;
    var ptr = studioState.activeAiPointer;
    if (!ptr || ptr.key !== docPageKey()) {
      banner.style.display = 'none';
      return;
    }
    var isCrit = /crit|high/i.test(String(ptr.severity || ''));
    banner.className = 'ps-ai-banner ' + (isCrit ? 'crit' : (ptr.matched ? 'info' : 'warn'));
    if (ptr.matched) {
      txt.textContent = '🎯 ' + L('AI Pointer located on p.', 'AI 已定位到第 ', 'Puntero IA en pág. ') +
        studioState.currentPage + ': ' + ptr.title;
    } else {
      txt.textContent = '🎯 ' + L(
        'AI Page Pointer: Navigated to p.' + studioState.currentPage + ' (' + ptr.title + '). Scanned page or no exact text match — use Circle or Arrow tools to mark.',
        'AI 页面定位：已跳转至第 ' + studioState.currentPage + ' 页（' + ptr.title + '）。当前页为扫描件或无精确文本匹配，可使用画圈或箭头工具手动标记。',
        'Puntero IA en pág. ' + studioState.currentPage + ' (' + ptr.title + '). Página escaneada o sin texto coincidente — use Círculo o Flecha.'
      );
    }
    banner.style.display = 'flex';
  }

  /* -------------------- AI Automatic Pointer & Circle Engine -------------------- */
  function tokenizeSearchText(raw) {
    var s = String(raw || '').toLowerCase().replace(/[^a-z0-9\u4e00-\u9fff]+/g, ' ');
    return s.split(/\s+/).filter(function (tok) {
      return tok && (tok.length >= 3 || /[\u4e00-\u9fff]/.test(tok)) && !STOP_WORDS[tok];
    });
  }

  function findBestDocIndex(sourceDoc, title) {
    var docs = studioState.docs;
    if (!docs.length) return -1;
    if (!sourceDoc && !title) return studioState.docIndex;
    var target = String(sourceDoc || '').trim().toLowerCase();
    var targetBase = baseName(target);
    var targetStem = targetBase.replace(/\.[^.]+$/, '');
    var targetTokens = tokenizeSearchText((sourceDoc || '') + ' ' + (title || ''));
    var aliases = [
      ['inspection', 'home_inspection', 'roof', 'sewer', 'foundation', 'structural'],
      ['termite', 'wdo', 'pest', 'rot'],
      ['prelim', 'title', 'vesting', 'schedule'],
      ['nhd', 'hazard', 'natural', 'flood', 'fire', 'seismic'],
      ['hoa', 'reserve', 'ccr', 'condo', 'governance'],
      ['permit', 'municipal', 'code', 'building'],
      ['tds', 'spq', 'seller', 'disclosure']
    ];

    var bestIdx = studioState.docIndex;
    var bestScore = -1;
    for (var i = 0; i < docs.length; i++) {
      var dn = docs[i].name.toLowerCase();
      var dp = docs[i].fullPath.toLowerCase();
      var dStem = dn.replace(/\.[^.]+$/, '');
      var score = 0;
      if (target && (dn === targetBase || dp === target)) score += 120;
      else if (targetStem && dStem === targetStem) score += 100;
      else if (targetBase && (dn.indexOf(targetStem) >= 0 || targetStem.indexOf(dStem) >= 0)) score += 60;

      for (var t = 0; t < targetTokens.length; t++) {
        if (dn.indexOf(targetTokens[t]) >= 0 || dp.indexOf(targetTokens[t]) >= 0) score += 14;
      }
      for (var a = 0; a < aliases.length; a++) {
        var grp = aliases[a];
        var hitTarget = grp.some(function (w) { return target.indexOf(w) >= 0; });
        var hitDoc = grp.some(function (w) { return dn.indexOf(w) >= 0 || dp.indexOf(w) >= 0; });
        if (hitTarget && hitDoc) score += 25;
      }
      if (score > bestScore) {
        bestScore = score;
        bestIdx = i;
      }
    }
    return bestScore > 0 ? bestIdx : studioState.docIndex;
  }

  function findBestTextMatchRegion(boxes, citation) {
    if (!Array.isArray(boxes) || !boxes.length) return null;

    var phrases = [];
    if (citation.quote) phrases.push(String(citation.quote));
    if (citation.title) phrases.push(String(citation.title));
    if (Array.isArray(citation.searchTerms)) {
      citation.searchTerms.forEach(function (t) { if (t) phrases.push(String(t)); });
    } else if (typeof citation.searchTerms === 'string' && citation.searchTerms) {
      phrases.push(citation.searchTerms);
    }

    var tokenSet = Object.create(null);
    phrases.forEach(function (ph) {
      tokenizeSearchText(ph).forEach(function (tok) { tokenSet[tok] = 1; });
    });
    var tokens = Object.keys(tokenSet);
    if (!tokens.length && !phrases.length) return null;

    // Group items into visual lines by ny proximity (~0.012)
    var lines = [];
    for (var i = 0; i < boxes.length; i++) {
      var b = boxes[i];
      var last = lines[lines.length - 1];
      if (last && Math.abs(b.ny - last.ny) <= 0.014) {
        last.items.push(b);
        last.text += ' ' + b.str;
      } else {
        lines.push({ ny: b.ny, items: [b], text: b.str });
      }
    }

    var bestScore = 0;
    var bestItems = null;

    for (var li = 0; li < lines.length; li++) {
      var candidates = [
        { text: lines[li].text, items: lines[li].items }
      ];
      if (li + 1 < lines.length) {
        candidates.push({
          text: lines[li].text + ' ' + lines[li + 1].text,
          items: lines[li].items.concat(lines[li + 1].items)
        });
      }
      for (var c = 0; c < candidates.length; c++) {
        var cand = candidates[c];
        var low = cand.text.toLowerCase();
        var score = 0;
        for (var p = 0; p < phrases.length; p++) {
          var snippet = String(phrases[p] || '').trim().toLowerCase().slice(0, 42);
          if (snippet.length >= 6 && low.indexOf(snippet) >= 0) score += 100;
        }
        var matchedOnLine = [];
        cand.items.forEach(function (it) {
          var ilow = it.str.toLowerCase();
          var itemHit = 0;
          for (var tk = 0; tk < tokens.length; tk++) {
            if (ilow.indexOf(tokens[tk]) >= 0) itemHit++;
          }
          if (itemHit > 0) matchedOnLine.push(it);
        });
        for (var tk2 = 0; tk2 < tokens.length; tk2++) {
          if (low.indexOf(tokens[tk2]) >= 0) score += 10 + Math.min(8, tokens[tk2].length);
        }
        if (score > bestScore) {
          bestScore = score;
          bestItems = matchedOnLine.length ? matchedOnLine : cand.items;
        }
      }
    }

    if (bestScore <= 0 || !bestItems || !bestItems.length) return null;

    var minX = 1, minY = 1, maxX = 0, maxY = 0;
    for (var k = 0; k < bestItems.length; k++) {
      var bx = bestItems[k];
      minX = Math.min(minX, bx.nx);
      minY = Math.min(minY, bx.ny);
      maxX = Math.max(maxX, bx.nx + bx.nw);
      maxY = Math.max(maxY, bx.ny + bx.nh);
    }
    var padX = 0.022;
    var padY = 0.015;
    var nx = Math.max(0.02, minX - padX);
    var ny = Math.max(0.02, minY - padY);
    var nw = Math.min(0.96 - nx, Math.max(0.12, (maxX - minX) + padX * 2));
    var nh = Math.min(0.96 - ny, Math.max(0.035, (maxY - minY) + padY * 2));
    return { nx: nx, ny: ny, nw: nw, nh: nh };
  }

  async function jumpToCitation(citation) {
    if (!citation || typeof citation !== 'object') return;
    if (!studioState.isOpen) {
      await openStudio({ citation: citation });
      return;
    }

    var vp = document.getElementById('psViewport');
    if (!studioState.docs.length) {
      var banner = document.getElementById('psAiBanner');
      var txt = document.getElementById('psAiBannerText');
      if (banner && txt) {
        banner.className = 'ps-ai-banner warn';
        txt.textContent = '📂 ' + L(
          'Load your local PDF / .ZIP packet to jump directly to ',
          '请先加载本地 PDF / .ZIP 文件以直接定位到 ',
          'Cargue su PDF / .ZIP local para saltar a '
        ) + (citation.sourceDoc || 'document') + ' p.' + parsePageNum(citation.sourcePage, 1) + '.';
        banner.style.display = 'flex';
      }
      return;
    }

    var targetDocIdx = findBestDocIndex(citation.sourceDoc, citation.title);
    if (targetDocIdx >= 0 && targetDocIdx !== studioState.docIndex) {
      studioState.docIndex = targetDocIdx;
      syncFileSelect();
    }
    var doc = studioState.docs[studioState.docIndex];
    await ensureDocLoaded(doc);
    var targetPage = Math.max(1, Math.min(doc.numPages || 1, parsePageNum(citation.sourcePage, 1)));
    studioState.currentPage = targetPage;
    await renderCurrentPage();

    var boxes = (doc && doc.textBoxesByPage && doc.textBoxesByPage[targetPage]) || [];
    var region = findBestTextMatchRegion(boxes, citation);

    studioState.activeAiPointer = {
      key: docPageKey(doc, targetPage),
      title: String(citation.title || citation.quote || 'Citation').trim(),
      severity: citation.severity || 'Major',
      nx: region ? region.nx : 0.5,
      ny: region ? region.ny : 0.1,
      nw: region ? region.nw : 0.5,
      nh: region ? region.nh : 0.06,
      matched: Boolean(region),
      startMs: performance.now()
    };

    updateAiBannerForCurrentPage();
    renderThumbRail();
    redrawAnnotationCanvas();
    startPulseLoop();

    if (vp) {
      if (region) {
        var stage = document.getElementById('psPageStage');
        var stageH = stage ? stage.clientHeight : 792;
        var stageW = stage ? stage.clientWidth : 612;
        var targetTop = Math.max(0, (region.ny + region.nh * 0.5) * stageH - vp.clientHeight * 0.45);
        var targetLeft = Math.max(0, (region.nx + region.nw * 0.5) * stageW - vp.clientWidth * 0.5);
        if (typeof vp.scrollTo === 'function') {
          vp.scrollTo({ top: targetTop, left: targetLeft, behavior: 'smooth' });
        } else {
          vp.scrollTop = targetTop;
          vp.scrollLeft = targetLeft;
        }
      } else {
        vp.scrollTop = 0;
      }
    }
  }

  /* -------------------- Studio Pointer / Drawing Events -------------------- */
  function insertPinTokenIntoChat(pinNumber, docName, pageNum) {
    var token = '[Pin #' + pinNumber + ': ' + docName + ' p.' + pageNum + '] ';
    var inp = document.getElementById('psChatInput');
    if (inp) {
      var cur = inp.value || '';
      inp.value = cur && !/\s$/.test(cur) ? (cur + ' ' + token) : (cur + token);
      studioState.chatDraft = inp.value;
    } else {
      studioState.chatDraft = (studioState.chatDraft || '') + token;
    }
  }

  function bindStudioEvents(bd) {
    bd.addEventListener('click', function (ev) {
      if (ev.target === bd) closeStudio();
    });

    bd.querySelector('#psCloseBtn').addEventListener('click', closeStudio);

    var addInput = bd.querySelector('#psAddFilesInput');
    var triggerPick = function () { addInput && addInput.click(); };
    bd.querySelector('#psAddFilesBtn').addEventListener('click', triggerPick);
    bd.querySelector('#psEmptyLoadBtn').addEventListener('click', triggerPick);
    addInput.addEventListener('change', function () {
      if (addInput.files && addInput.files.length) {
        ingestFiles(addInput.files, studioState.slug);
      }
      addInput.value = '';
    });

    bd.querySelector('#psToggleRailBtn').addEventListener('click', function () {
      studioState.railCollapsed = !studioState.railCollapsed;
      renderThumbRail();
      if (studioState.fitWidth) renderCurrentPage();
    });

    bd.querySelector('#psFileSelect').addEventListener('change', function (ev) {
      var idx = parseInt(ev.target.value, 10);
      if (isFinite(idx) && idx >= 0 && idx < studioState.docs.length) {
        studioState.docIndex = idx;
        loadAndRenderCurrentDoc();
      }
    });

    bd.querySelector('#psPrevPageBtn').addEventListener('click', function () {
      goToPage(studioState.currentPage - 1);
    });
    bd.querySelector('#psNextPageBtn').addEventListener('click', function () {
      goToPage(studioState.currentPage + 1);
    });
    bd.querySelector('#psPageInput').addEventListener('change', function (ev) {
      goToPage(ev.target.value);
    });

    bd.querySelector('#psZoomOutBtn').addEventListener('click', function () {
      studioState.fitWidth = false;
      studioState.zoom = Math.max(0.4, Math.round((studioState.zoom - 0.2) * 100) / 100);
      refreshChromeLabels();
      renderCurrentPage();
    });
    bd.querySelector('#psZoomInBtn').addEventListener('click', function () {
      studioState.fitWidth = false;
      studioState.zoom = Math.min(3.0, Math.round((studioState.zoom + 0.2) * 100) / 100);
      refreshChromeLabels();
      renderCurrentPage();
    });
    bd.querySelector('#psFitWidthBtn').addEventListener('click', function () {
      studioState.fitWidth = true;
      refreshChromeLabels();
      renderCurrentPage();
    });

    bd.querySelectorAll('[data-ps-tool]').forEach(function (btn) {
      btn.addEventListener('click', function () {
        studioState.tool = btn.getAttribute('data-ps-tool') || 'pan';
        updateCanvasCursor();
        refreshChromeLabels();
      });
    });

    bd.querySelector('#psUndoBtn').addEventListener('click', function () {
      undoAnnotation();
    });

    bd.querySelector('#psClearPageBtn').addEventListener('click', function () {
      clearPageAnnotations();
    });

    bd.querySelector('#psExportPngBtn').addEventListener('click', function () {
      var pdfCanvas = document.getElementById('psPdfCanvas');
      var annotCanvas = document.getElementById('psAnnotCanvas');
      var doc = studioState.docs[studioState.docIndex];
      if (!pdfCanvas || !annotCanvas || !doc || !pdfCanvas.width) return;
      var merged = document.createElement('canvas');
      merged.width = pdfCanvas.width;
      merged.height = pdfCanvas.height;
      var mctx = merged.getContext('2d');
      mctx.drawImage(pdfCanvas, 0, 0);
      mctx.drawImage(annotCanvas, 0, 0);
      try {
        var dataUrl = merged.toDataURL('image/png');
        var a = document.createElement('a');
        var cleanBase = doc.name.replace(/\.[^.]+$/, '').replace(/[^A-Za-z0-9_-]+/g, '_');
        a.href = dataUrl;
        a.download = cleanBase + '_p' + studioState.currentPage + '_annotated.png';
        document.body.appendChild(a);
        a.click();
        a.remove();
      } catch (_) {}
    });

    var errClose = bd.querySelector('#psErrorBannerCloseBtn');
    if (errClose) {
      errClose.addEventListener('click', function () {
        setStudioError('', '');
      });
    }

    bd.querySelector('#psAiBannerCloseBtn').addEventListener('click', function () {
      studioState.activeAiPointer = null;
      updateAiBannerForCurrentPage();
      redrawAnnotationCanvas();
    });

    // Side panel tabs
    bd.querySelectorAll('[data-ps-tab]').forEach(function (tabBtn) {
      tabBtn.addEventListener('click', function () {
        var t = tabBtn.getAttribute('data-ps-tab') || 'analysis';
        studioState.activeSideTab = t;
        bd.querySelectorAll('[data-ps-tab]').forEach(function (b) {
          b.classList.toggle('act', b.getAttribute('data-ps-tab') === t);
        });
        bd.querySelector('#psTabAnalysis').classList.toggle('act', t === 'analysis');
        bd.querySelector('#psTabChat').classList.toggle('act', t === 'chat');
      });
    });

    // Chat Reference & Submit
    bd.querySelector('#psRefCurrentBtn').addEventListener('click', function () {
      var doc = studioState.docs[studioState.docIndex];
      var docName = doc ? doc.name : L('Uploaded Packet', '当前文档', 'Documento');
      var annots = getAnnotationsFor();
      var pins = annots.filter(function (a) { return a.type === 'pin'; }).map(function (a) { return '#' + a.pinNumber; });
      var metaParts = [docName + ' p.' + studioState.currentPage];
      if (annots.length) metaParts.push(annots.length + ' ' + L('annotation(s)', '处标注', 'anotación(es)'));
      if (pins.length) metaParts.push('Pins ' + pins.join(','));
      var prefix = '[Viewing ' + metaParts.join(' · ') + '] ';
      var inp = document.getElementById('psChatInput');
      if (inp) {
        if (inp.value.indexOf(prefix) !== 0) {
          inp.value = prefix + inp.value;
        }
        studioState.chatDraft = inp.value;
        inp.focus();
      }
    });

    bd.querySelector('#psChatForm').addEventListener('submit', function (ev) {
      ev.preventDefault();
      var inp = document.getElementById('psChatInput');
      submitStudioChat(inp ? inp.value : '');
    });

    // Annotation Canvas Pointer Drawing & Pan
    var annotCanvas = bd.querySelector('#psAnnotCanvas');
    var vp = bd.querySelector('#psViewport');

    function normPt(ev) {
      var rect = annotCanvas.getBoundingClientRect();
      var w = rect.width || 1;
      var h = rect.height || 1;
      return {
        x: Math.max(0, Math.min(1, (ev.clientX - rect.left) / w)),
        y: Math.max(0, Math.min(1, (ev.clientY - rect.top) / h))
      };
    }

    function buildShapePreview(drag) {
      if (!drag) return null;
      if (drag.tool === 'circle') {
        return {
          type: 'circle',
          color: studioState.color,
          cx: (drag.sx + drag.cx) * 0.5,
          cy: (drag.sy + drag.cy) * 0.5,
          rx: Math.abs(drag.cx - drag.sx) * 0.5,
          ry: Math.abs(drag.cy - drag.sy) * 0.5
        };
      }
      if (drag.tool === 'arrow') {
        return {
          type: 'arrow',
          color: studioState.color,
          x1: drag.sx,
          y1: drag.sy,
          x2: drag.cx,
          y2: drag.cy
        };
      }
      if (drag.tool === 'rect') {
        return {
          type: 'rect',
          color: studioState.color,
          x: Math.min(drag.sx, drag.cx),
          y: Math.min(drag.sy, drag.cy),
          w: Math.abs(drag.cx - drag.sx),
          h: Math.abs(drag.cy - drag.sy)
        };
      }
      if (drag.tool === 'pen') {
        return {
          type: 'pen',
          color: studioState.color,
          points: drag.points
        };
      }
      return null;
    }

    annotCanvas.addEventListener('pointerdown', function (ev) {
      if (ev.button !== 0 && ev.pointerType === 'mouse') return;
      if (studioState.tool === 'pan') {
        studioState.panStart = {
          clientX: ev.clientX,
          clientY: ev.clientY,
          scrollLeft: vp.scrollLeft,
          scrollTop: vp.scrollTop
        };
        annotCanvas.style.cursor = 'grabbing';
        try { annotCanvas.setPointerCapture(ev.pointerId); } catch (_) {}
        return;
      }

      var pt = normPt(ev);
      if (studioState.tool === 'pin') {
        var doc = studioState.docs[studioState.docIndex];
        var pinNum = ++studioState.pinSeq;
        var docName = doc ? doc.name : 'Document';
        getAnnotationsFor().push({
          type: 'pin',
          pinNumber: pinNum,
          color: studioState.color,
          x: pt.x,
          y: pt.y,
          docName: docName,
          page: studioState.currentPage
        });
        insertPinTokenIntoChat(pinNum, docName, studioState.currentPage);
        redrawAnnotationCanvas();
        renderThumbRail();
        return;
      }

      studioState.dragging = {
        tool: studioState.tool,
        sx: pt.x,
        sy: pt.y,
        cx: pt.x,
        cy: pt.y,
        points: [{ x: pt.x, y: pt.y }]
      };
      try { annotCanvas.setPointerCapture(ev.pointerId); } catch (_) {}
    });

    annotCanvas.addEventListener('pointermove', function (ev) {
      if (studioState.panStart) {
        vp.scrollLeft = studioState.panStart.scrollLeft - (ev.clientX - studioState.panStart.clientX);
        vp.scrollTop = studioState.panStart.scrollTop - (ev.clientY - studioState.panStart.clientY);
        return;
      }
      if (!studioState.dragging) return;
      var pt = normPt(ev);
      studioState.dragging.cx = pt.x;
      studioState.dragging.cy = pt.y;
      if (studioState.dragging.tool === 'pen') {
        var pts = studioState.dragging.points;
        var prev = pts[pts.length - 1];
        if (!prev || Math.hypot(pt.x - prev.x, pt.y - prev.y) > 0.002) {
          pts.push(pt);
        }
      }
      redrawAnnotationCanvas(buildShapePreview(studioState.dragging));
    });

    function finishDraw(ev) {
      if (studioState.panStart) {
        studioState.panStart = null;
        updateCanvasCursor();
        return;
      }
      if (!studioState.dragging) return;
      var drag = studioState.dragging;
      studioState.dragging = null;
      var shape = buildShapePreview(drag);
      if (shape) {
        var moved = Math.hypot(drag.cx - drag.sx, drag.cy - drag.sy);
        if (drag.tool === 'pen' || moved >= 0.008) {
          getAnnotationsFor().push(shape);
          renderThumbRail();
        }
      }
      redrawAnnotationCanvas();
      try { if (ev && ev.pointerId != null) annotCanvas.releasePointerCapture(ev.pointerId); } catch (_) {}
    }

    annotCanvas.addEventListener('pointerup', finishDraw);
    annotCanvas.addEventListener('pointercancel', finishDraw);

    if (!window.__oePdfStudioGlobalBound) {
      window.__oePdfStudioGlobalBound = true;
      window.addEventListener('keydown', function (ev) {
        if (ev.key === 'Escape' && studioState.isOpen) closeStudio();
      });
      window.addEventListener('openestate:rolechange', function (ev) {
        var r = (ev && ev.detail && ev.detail.role) || getActiveRole();
        studioState.role = (r === 'buyer' || r === 'seller' || r === 'agent') ? r : 'all';
        renderRoleBar();
      });
      document.addEventListener('click', function (ev) {
        if (ev.target && ev.target.closest && ev.target.closest('#langSwitch button')) {
          setTimeout(refreshChromeLabels, 30);
        }
      });
      if (typeof MutationObserver !== 'undefined') {
        try {
          new MutationObserver(function () {
            if (studioState.isOpen) refreshChromeLabels();
          }).observe(document.documentElement, { attributes: true, attributeFilter: ['lang'] });
        } catch (_) {}
      }
    }
    updateCanvasCursor();
  }

  function updateCanvasCursor() {
    var annotCanvas = document.getElementById('psAnnotCanvas');
    var textLayerEl = document.getElementById('psTextLayer');
    if (annotCanvas) {
      if (studioState.tool === 'pan') {
        annotCanvas.style.cursor = 'grab';
        annotCanvas.style.touchAction = 'auto';
        annotCanvas.style.pointerEvents = 'none';
      } else if (studioState.tool === 'pin') {
        annotCanvas.style.cursor = 'copy';
        annotCanvas.style.touchAction = 'none';
        annotCanvas.style.pointerEvents = 'auto';
      } else {
        annotCanvas.style.cursor = 'crosshair';
        annotCanvas.style.touchAction = 'none';
        annotCanvas.style.pointerEvents = 'auto';
      }
    }
    if (textLayerEl) {
      textLayerEl.style.pointerEvents = studioState.tool === 'pan' ? 'auto' : 'none';
    }
  }

  /* -------------------- Annotation & State Inspection Helpers -------------------- */
  function addAnnotation(pageAnnot) {
    if (!pageAnnot || typeof pageAnnot !== 'object') return null;
    var doc = studioState.docs[studioState.docIndex];
    if (pageAnnot.docName && studioState.docs.length) {
      var matchIdx = findBestDocIndex(pageAnnot.docName, '');
      if (matchIdx >= 0) doc = studioState.docs[matchIdx];
    }
    var targetPage = pageAnnot.page != null ? parsePageNum(pageAnnot.page, studioState.currentPage || 1) : (studioState.currentPage || 1);
    var annot = Object.assign({}, pageAnnot, {
      type: pageAnnot.type || 'circle',
      color: pageAnnot.color || studioState.color
    });
    if (annot.type === 'pin') {
      var pinNum = annot.pinNumber || ++studioState.pinSeq;
      if (pinNum > studioState.pinSeq) studioState.pinSeq = pinNum;
      var dName = annot.docName || (doc ? doc.name : 'Document');
      annot.pinNumber = pinNum;
      annot.docName = dName;
      annot.page = targetPage;
      insertPinTokenIntoChat(pinNum, dName, targetPage);
    }
    getAnnotationsFor(doc, targetPage).push(annot);
    redrawAnnotationCanvas();
    renderThumbRail();
    return Object.assign({}, annot);
  }

  function undoAnnotation() {
    var list = getAnnotationsFor();
    if (!list.length) return null;
    var popped = list.pop();
    redrawAnnotationCanvas();
    renderThumbRail();
    return popped;
  }

  function clearPageAnnotations() {
    var k = docPageKey();
    studioState.annotations[k] = [];
    if (studioState.activeAiPointer && studioState.activeAiPointer.key === k) {
      studioState.activeAiPointer = null;
      updateAiBannerForCurrentPage();
    }
    redrawAnnotationCanvas();
    renderThumbRail();
    return true;
  }

  function getState() {
    var curDoc = studioState.docs[studioState.docIndex] || null;
    var curAnnots = getAnnotationsFor().map(function (a) { return Object.assign({}, a); });
    return {
      isOpen: studioState.isOpen,
      slug: studioState.slug,
      address: studioState.address,
      role: studioState.role,
      bucketKey: studioState.bucketKey,
      docs: studioState.docs.map(function (d) {
        return {
          id: d.id,
          name: d.name,
          fullPath: d.fullPath,
          mime: d.mime,
          size: d.size,
          kind: d.kind,
          numPages: d.numPages
        };
      }),
      docIndex: studioState.docIndex,
      currentDocName: curDoc ? curDoc.name : null,
      currentPage: studioState.currentPage,
      renderedDocName: studioState.renderedDocName || null,
      renderedPage: studioState.renderedPage || 0,
      numPages: curDoc ? curDoc.numPages : 0,
      isScannedPage: isCurrentDocPageScanned(curDoc, studioState.currentPage),
      pdfJsActive: Boolean(curDoc && curDoc.pdfDoc),
      fflateActive: Boolean(_fflateLoaded),
      lastError: studioState.lastError || '',
      lastErrorCode: studioState.lastErrorCode || '',
      zoom: studioState.zoom,
      fitWidth: studioState.fitWidth,
      tool: studioState.tool,
      color: studioState.color,
      pinSeq: studioState.pinSeq,
      activeSideTab: studioState.activeSideTab,
      activeAiPointer: studioState.activeAiPointer ? Object.assign({}, studioState.activeAiPointer) : null,
      pageAnnotations: curAnnots,
      annotationsCount: curAnnots.length,
      chatHistory: studioState.chatHistory.slice()
    };
  }

  /* -------------------- Public Studio API -------------------- */
  function setAnalysis(analysis, meta) {
    meta = meta || {};
    if (meta.slug != null) studioState.slug = String(meta.slug || '').trim();
    if (meta.address != null) studioState.address = String(meta.address || '').trim();
    if (meta.role) studioState.role = String(meta.role).trim().toLowerCase();
    studioState.analysisRaw = analysis || null;
    studioState.analysisVm = extractAnalysisViewModel(analysis);
    if (document.getElementById('oePdfStudioBackdrop')) {
      refreshChromeLabels();
    }
  }

  async function openStudio(opts) {
    opts = opts || {};
    var bd = buildStudioModal();

    if (opts.slug != null) studioState.slug = String(opts.slug || '').trim();
    if (opts.address != null) studioState.address = String(opts.address || '').trim();
    if (opts.role) {
      var nr = String(opts.role).trim().toLowerCase();
      studioState.role = (nr === 'buyer' || nr === 'seller' || nr === 'agent') ? nr : getActiveRole();
    } else {
      studioState.role = getActiveRole();
    }
    if (typeof opts.onAskChat === 'function') {
      studioState.onAskChat = opts.onAskChat;
    } else if (!studioState.onAskChat) {
      studioState.onAskChat = null;
    }

    if (opts.analysis !== undefined) {
      setAnalysis(opts.analysis, { slug: studioState.slug, address: studioState.address, role: studioState.role });
    }

    var ingestedNew = null;
    if (opts.files && (opts.files.length > 0 || opts.files instanceof File || opts.files instanceof Blob)) {
      ingestedNew = await ingestFiles(opts.files, studioState.slug);
    }

    var resolved = getBucketForSlug(studioState.slug);
    studioState.bucketKey = resolved.key;
    if (resolved.bucket.pending) {
      await resolved.bucket.pending;
    }
    studioState.docs = resolved.bucket.docs.slice();
    if (ingestedNew && opts.files && !opts.citation) {
      var firstRaw = opts.files[0] || opts.files;
      var firstName = firstRaw && firstRaw.name ? baseName(firstRaw.name).toLowerCase() : '';
      if (firstName) {
        var foundIdx = studioState.docs.findIndex(function (d) {
          return d.name.toLowerCase() === firstName || (d.sourceZipName && d.sourceZipName.toLowerCase() === firstName);
        });
        if (foundIdx >= 0) studioState.docIndex = foundIdx;
      }
    }
    if (studioState.docIndex >= studioState.docs.length) studioState.docIndex = 0;

    studioState.isOpen = true;
    bd.classList.add('on');
    refreshChromeLabels();
    syncFileSelect();
    renderChatLog();

    if (opts.initialTab === 'chat') {
      var chatBtn = bd.querySelector('#psTabBtnChat');
      if (chatBtn) chatBtn.click();
    }

    if (opts.citation) {
      await jumpToCitation(opts.citation);
    } else {
      await loadAndRenderCurrentDoc();
    }
  }

  function closeStudio() {
    studioState.isOpen = false;
    if (studioState.pulseRaf) {
      cancelAnimationFrame(studioState.pulseRaf);
      studioState.pulseRaf = 0;
    }
    var bd = document.getElementById('oePdfStudioBackdrop');
    if (bd) bd.classList.remove('on');
  }

  window.OpenEstatePdfStudio = {
    __v: 1,
    ingestFiles: ingestFiles,
    setAnalysis: setAnalysis,
    openStudio: openStudio,
    closeStudio: closeStudio,
    jumpToCitation: jumpToCitation,
    hasLocalFiles: hasLocalFiles,
    getState: getState,
    addAnnotation: addAnnotation,
    undoAnnotation: undoAnnotation,
    clearPageAnnotations: clearPageAnnotations
  };
})();
