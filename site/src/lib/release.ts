// The newest release, read once at build time so every Download button links straight to its
// disk image. The image's name carries the version, so GitHub's /releases/latest/download/
// redirect can't name it ahead of time. Set GITHUB_TOKEN to lift the API's hourly limit.
const REPO = 'Dishant1804/Collate';

interface Asset {
  name: string;
  browser_download_url: string;
}

const headers: Record<string, string> = { Accept: 'application/vnd.github+json' };
if (process.env.GITHUB_TOKEN) headers.Authorization = `Bearer ${process.env.GITHUB_TOKEN}`;

const response = await fetch(`https://api.github.com/repos/${REPO}/releases/latest`, { headers });
if (!response.ok) {
  throw new Error(`Reading the latest Collate release failed: GitHub answered ${response.status} ${response.statusText}`);
}
const latest = (await response.json()) as { tag_name: string; assets: Asset[] };
const dmg = latest.assets.find((a) => a.name.endsWith('.dmg'));
if (!dmg) {
  throw new Error(`The latest Collate release, ${latest.tag_name}, has no .dmg asset`);
}

export const release = {
  version: latest.tag_name.replace(/^v/, ''),
  dmgUrl: dmg.browser_download_url,
};
