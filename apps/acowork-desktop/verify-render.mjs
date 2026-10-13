import { JSDOM } from 'jsdom';
const dom = new JSDOM('<!doctype html><html><body></body></html>', { pretendToBeVisual: true });
const w = dom.window;
for (const k of ['window','document','navigator','Element','SVGElement','HTMLElement','Node','MutationObserver','CSSStyleSheet','getComputedStyle','DocumentFragment','NodeFilter','XMLSerializer','SVGGraphicsElement']) if (w[k] !== undefined) globalThis[k] = w[k];
globalThis.requestAnimationFrame = (cb) => setTimeout(cb, 0);
const approx = (el) => { const t=(el&&(el.textContent||''))||''; const L=t.split('\n'); return {x:0,y:0,width:Math.max(1,...L.map(l=>l.length))*7.2,height:L.length*14.4}; };
w.SVGElement.prototype.getBBox = approx;
w.SVGElement.prototype.getComputedTextLength = function(){return approx(this).width;};
w.SVGElement.prototype.getScreenCTM = () => ({a:1,b:0,c:0,d:1,e:0,f:0});
w.Element.prototype.getBoundingClientRect = function(){const b=approx(this);return{...b,top:b.y,left:b.x,right:b.x+b.width,bottom:b.y+b.height};};

const mermaid = (await import('mermaid')).default;
const { LIGHT_TOKENS, DARK_TOKENS } = await import('./src/lib/mermaidTheme.ts').catch(async () => {
  // strip types via a tiny transform
  const src = await (await import('node:fs/promises')).readFile('src/lib/mermaidTheme.ts','utf8');
  const js = src
    .replace(/\/\*\*[\s\S]*?\*\//g, '')
    .replace(/^export type MermaidThemeName =[^\n]*\n/gm, '')
    .replace(/^export type ThemePreference =[^\n]*\n/gm, '')
    .replace(/^export interface MermaidTokens \{[\s\S]*?^\}\s*\n/gm, '')
    .replace(/:\s*MermaidTokens/g, '')
    .replace(/:\s*MermaidThemeName/g, '')
    .replace(/:\s*ThemePreference/g, '')
    .replace(/:\s*Record<string, string \| number>/g, '')
    .replace(/(function subgraphCss\([^)]*)\): string/g, '$1)')
    .replace(/\(fill: string, stroke: string, depth: number\)/g, '(fill, stroke, depth)')
    .replace(/\((\w+): string\)/g, '($1)')
    .replace(/\((\w+): number\)/g, '($1)')
    .replace(/\(theme, osTheme: "light" \| "dark"\)/g, '(theme, osTheme)')
    .replace(/\): MermaidTokens/g, ')')
    .replace(/\): boolean/g, ')')
    .replace(/export /g, '')
    .replace(/^import.*$/gm, '');
  const f='./mermaidTheme.mjs'; await (await import('node:fs/promises')).writeFile(f, `${js}\nexport { LIGHT_TOKENS, DARK_TOKENS };`);
  return import(f);
});

const chart = `flowchart TD
  A[Input] --> B[Process]
  B --> C[Output]
  B -- yes --> D{Choice}
  D -->|a| E[Alt]
  subgraph S1[Group]
    F[Inner] --> G[Inner2]
  end`;

async function render(name, tokens) {
  mermaid.initialize({ startOnLoad:false, theme: tokens.theme, themeVariables: tokens.themeVariables, themeCSS: tokens.themeCSS,
    flowchart:{ useMaxWidth:false, htmlLabels:true, curve:'basis', padding:6, nodeSpacing:35, rankSpacing:35 } });
  const { svg } = await mermaid.render(name, chart);
  return svg;
}
const light = await render('v-light', LIGHT_TOKENS);
const dark  = await render('v-dark',  DARK_TOKENS);
const fs = await import('node:fs/promises');
await fs.writeFile('out-light.svg', light);
await fs.writeFile('out-dark.svg', dark);

// The two rules that caused the reported bug:
const probe = (svg, label) => {
  const css = svg.slice(svg.indexOf('<style>'), svg.indexOf('</style>'));
  const pick = (sel) => { const escaped = sel.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'); const m = new RegExp(escaped + '\\{([^}]*)\\}').exec(css); return m ? m[1] : '(absent)'; };
  console.log(`--- ${label} ---`);
  console.log('  .flowchart-link  :', pick('.flowchart-link'));
  console.log('  .marker          :', pick('.marker'));
  console.log('  .arrowheadPath   :', pick('.arrowheadPath'));
  console.log('  .edgeLabel rect  :', pick('.edgeLabel rect'));
  console.log('  .node rect       :', pick('.node rect'));
};
probe(light,'LIGHT'); probe(dark,'DARK');
