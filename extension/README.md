# Open Estate Browser Extension (Manifest V3) — Self-Hosted Property & Deal Room Companion

Zero-build-step **Chrome / Edge / Brave Manifest V3** browser extension for your self-hosted **Open Estate** instance (personal, non-commercial property research).

Designed for strict upstream Terms of Service compliance: **zero third-party listing portal `host_permissions`**, **zero automatic `content_scripts` injection**, and **zero portal DOM scraping**.

## Key Features

1. **Toolbar Quick US Address Lookup (`popup.html` / `popup.js`)**:
   - Enter any complete US street address to fetch cached due-diligence summaries in milliseconds (`/api/extension/summary`) or enqueue a background report build (`/api/report/enqueue` + `/api/job/:id`).
   - Displays **Overall Score (`0–100`)**, **Fair Value Range**, and **True Monthly Cost** (with statutory tax reassessment & hazard insurance).
2. **Right-Click Context-Menu Address Lookup (`background.js`)**:
   - Highlight any US street address on any webpage, right-click, and choose **Analyze "..." (Open Estate)** to open the full research report on your self-hosted Open Estate instance without reading or scraping page DOMs.
3. **Deal Room Shortlist (`0–10` Properties)**:
   - Save properties into your local Deal Room shortlist and launch a side-by-side comparison matrix (`/deal-room?slugs=...`) across valuation, PITI carry, natural hazards, EPA environment, walkability, schools, and Chinese 堪舆朝向.
4. **Trilingual UI (`EN / 中文 / ES`) & Self-Hosted Server Settings (`brand.js`)**:
   - Vendored MV3-CSP-safe `i18next` (`extension/vendor/i18next.min.js` + `extension/locales/{en,zh-CN,es}.json`).
   - Defaults to `http://localhost:8080` (with `http://127.0.0.1:8080` fallback) for local Docker self-hosting, plus optional custom origin permission prompt if you host Open Estate on a private home-lab / LAN host.

## Load Unpacked (Chrome / Edge / Brave)

1. Start your local self-hosted Open Estate server:
   ```bash
   docker compose up -d
   # or: cargo run --release --bin open-estate-server
   ```
2. Open `chrome://extensions` (or `edge://extensions` / `brave://extensions`).
3. Enable **Developer mode** (top-right toggle).
4. Click **Load unpacked** and select this `extension/` folder.
5. Click the Open Estate icon in your browser toolbar to verify the connection (`localhost:8080`), analyze an address, or compare saved homes in the **Deal Room**.

## Least-Privilege Permissions

- `storage`: Caches recent property summaries locally and persists your Deal Room shortlist & server URL settings.
- `contextMenus`: Registers a right-click action on user-selected text so you can send any highlighted US street address to your self-hosted Open Estate server.
- `host_permissions`: Scoped strictly to `http://localhost/*` and `http://127.0.0.1/*` by default (`optional_host_permissions` requested only if you configure a custom self-hosted origin).
