import { readFileSync } from 'node:fs';
import { join, dirname, posix } from 'node:path';

const root = process.cwd();
const contentDir = join(root, 'content/docs');
// llms.md lives in docs/, not docs/site/content/ — moved there by the docs
// canonicalization. The build reads it from one level up.
const overviewPath = join(root, '../llms.md');
export const basePath = process.env.NEXT_PUBLIC_BASE_PATH ?? '';
export const site =
  (process.env.NEXT_PUBLIC_SITE_URL ?? `http://localhost:${process.env.PORT ?? 3000}`) + basePath;

function readMeta(dir) {
  return JSON.parse(readFileSync(join(dir, 'meta.json'), 'utf8'));
}

function frontmatter(raw) {
  const m = raw.match(/^---\n([\s\S]*?)\n---\n/);
  const title = m?.[1].match(/^title:\s*["']?(.*?)["']?\s*$/m)?.[1] ?? '';
  return { title, body: m ? raw.slice(m[0].length) : raw };
}

function toTxt(body, pageDir, base) {
  return body
    .replace(/<Mermaid\s+chart=\{`([\s\S]*?)`\}\s*\/>/g, (_, chart) => '```mermaid\n' + chart.trim() + '\n```')
    .replace(/<Callout(?:\s+type="\w+")?(?:\s+title="([^"]*)")?\s*>/g, (_, t) => (t ? `> **${t}.**` : '>'))
    .replace(/<\/Callout>/g, '')
    .replace(/\]\((\.{1,2}\/[^)#]+?)\.mdx(#[^)]*)?\)/g, (_, rel, frag = '') => {
      const slug = posix.normalize(posix.join(pageDir, rel));
      return `](${base}/${slug}.txt${frag})`;
    });
}

export function collectPages(base = site) {
  const pages = [];
  for (const section of readMeta(contentDir).pages.filter((p) => p !== 'index')) {
    const sectionDir = join(contentDir, section);
    const meta = readMeta(sectionDir);
    for (const name of meta.pages) {
      const slug = name === 'index' ? section : `${section}/${name}`;
      const { title, body } = frontmatter(readFileSync(join(sectionDir, `${name}.mdx`), 'utf8'));
      pages.push({
        section: meta.title ?? section,
        title,
        slug,
        txt: `# ${title}\n\n> ${base}/${slug}/\n\n${toTxt(body, dirname(slug), base).trim()}\n`,
      });
    }
  }
  return pages;
}

export function llmsTxt(base = site) {
  const overview = toTxt(readFileSync(overviewPath, 'utf8'), '.', base);
  let index = '## Pages\n';
  let lastSection = '';
  for (const p of collectPages(base)) {
    if (p.section !== lastSection) {
      index += `\n### ${p.section}\n\n`;
      lastSection = p.section;
    }
    index += `- [${p.title}](${base}/${p.slug}.txt)\n`;
  }
  return `${overview.trim()}\n\n${index}`;
}
