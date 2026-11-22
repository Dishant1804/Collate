import { defineConfig } from 'astro/config';
import { writeFileSync } from 'node:fs';

// The site's public address, for canonical links, absolute share-card URLs and the sitemap:
//   SITE_URL=https://example.com npm run build
const site = process.env.SITE_URL;

// robots.txt and sitemap.xml, written after the build. A sitemap needs absolute URLs, so
// without SITE_URL the build says so and leaves it out rather than guess an address.
function crawlFiles() {
  return {
    name: 'crawl-files',
    hooks: {
      'astro:build:done': ({ dir, pages, logger }) => {
        const robots = ['User-agent: *', 'Allow: /'];
        if (site) {
          const urls = pages
            .map((p) => p.pathname)
            .filter((path) => !path.startsWith('404'))
            .map((path) => new URL(path, site).href);
          const today = new Date().toISOString().slice(0, 10);
          const entries = urls.map((loc) => `  <url><loc>${loc}</loc><lastmod>${today}</lastmod></url>`).join('\n');
          writeFileSync(new URL('sitemap.xml', dir), `<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n${entries}\n</urlset>\n`);
          robots.push('', `Sitemap: ${new URL('sitemap.xml', site).href}`);
          logger.info(`sitemap.xml lists ${urls.length} page(s) under ${site}`);
        } else {
          logger.warn('SITE_URL is not set: no canonical link, absolute share-card URLs or sitemap. Build with SITE_URL=https://… for production.');
        }
        writeFileSync(new URL('robots.txt', dir), robots.join('\n') + '\n');
      },
    },
  };
}

export default defineConfig({ site, integrations: [crawlFiles()] });
