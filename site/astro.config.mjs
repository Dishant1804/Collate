import { defineConfig } from 'astro/config';
import { writeFileSync } from 'node:fs';

// The site's public address, for canonical links, absolute share-card URLs and the sitemap.
// SITE_URL overrides it, say for a preview deployment.
const site = process.env.SITE_URL || 'https://collate.dishant.site';

// robots.txt and sitemap.xml, written after the build.
function crawlFiles() {
  return {
    name: 'crawl-files',
    hooks: {
      'astro:build:done': ({ dir, pages, logger }) => {
        const urls = pages
          .map((p) => p.pathname)
          .filter((path) => !path.startsWith('404'))
          .map((path) => new URL(path, site).href);
        const today = new Date().toISOString().slice(0, 10);
        const entries = urls.map((loc) => `  <url><loc>${loc}</loc><lastmod>${today}</lastmod></url>`).join('\n');
        writeFileSync(new URL('sitemap.xml', dir), `<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n${entries}\n</urlset>\n`);
        writeFileSync(new URL('robots.txt', dir), `User-agent: *\nAllow: /\n\nSitemap: ${new URL('sitemap.xml', site).href}\n`);
        logger.info(`sitemap.xml lists ${urls.length} page(s) under ${site}`);
      },
    },
  };
}

export default defineConfig({ site, integrations: [crawlFiles()] });
