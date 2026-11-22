# Collate site

The landing page for Collate, built with [Astro](https://astro.build).

```sh
npm install
npm run dev       # http://localhost:4321
npm run build     # static site in dist/
npm run preview   # serve the build
```

Astro 7.3 needs Node.js 22.12 or newer.

For production, give the build the site's address:

```sh
SITE_URL=https://your-domain npm run build
```

It sets the canonical link, the absolute share-card URLs and `sitemap.xml`; `robots.txt` is
always written and points to the sitemap when there is one. Without `SITE_URL` the build warns
and leaves those out rather than guess an address.

## Search and sharing

- `src/layouts/Layout.astro`: title, description, canonical, Open Graph and Twitter card tags,
  and the page's JSON-LD. `src/pages/index.astro` describes Collate as a `SoftwareApplication`
  (free, macOS 13 or later, its version and download from the latest release, screenshots).
- `public/og.png` is the 1200x630 share card; `public/apple-touch-icon.png` the 180px icon.
- `src/pages/404.astro` is kept out of search results (`noindex`).
- Screenshots are WebP at 900, 1200 and 1800px; the hero loads first, the rest when scrolled to.
- Geist and Geist Mono are served from `public/fonts/` (`src/styles/fonts.css`), not Google Fonts.

- `src/pages/index.astro` puts the page together from the sections in `src/components/`.
- `src/layouts/Layout.astro` holds the head, the fonts and the responsive rules.
- `src/components/Install.astro` is the only section with a script: the brew and npx tabs
  and the copy button.
- App screenshots are in `public/images/` (dark) and `public/images/light/` (the same views in
  the app's Light theme); `src/components/ThemedShot.astro` shows the one matching the page's
  theme, and the dark one in both when a shot has no light capture.
- Images are in `public/images/`. The logo comes in two finishes from `logo-work/variants/`:
  `public/logo-porcelain.svg` shows in light mode and `public/logo-graphite.svg` in dark mode
  (`src/components/Logo.astro`).
