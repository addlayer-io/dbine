// Tests for the Markdown renderer. Run: `node --test src/composables/markdown.test.mjs`
// from web/ (Node 23.6+ strips the TypeScript types itself). Not part of the
// app's build: nothing imports this file.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { esc, inline, markdownParts, renderMarkdown, safeHref } from './markdown.ts';

test('raw HTML is always escaped', () => {
  const html = renderMarkdown('<script>alert(1)</script>\n\n<img src=x onerror="alert(1)">');
  assert.ok(!html.includes('<script'));
  assert.ok(!html.includes('<img'));
  assert.ok(html.includes('&lt;script&gt;alert(1)&lt;/script&gt;'));
  assert.ok(html.includes('onerror=&quot;alert(1)&quot;'));
});

test('HTML inside code, headings, lists and tables is escaped too', () => {
  const html = renderMarkdown('# <b>x</b>\n\n- <i>y</i>\n\n`<u>`\n\n```\n<script>\n```\n\n| a |\n|---|\n| <s> |');
  for (const tag of ['<b>', '<i>', '<u>', '<script>', '<s>']) assert.ok(!html.includes(tag), tag);
});

test('only http, https and mailto links are kept', () => {
  assert.equal(inline('[a](https://dbine.com)'), '<a href="https://dbine.com" rel="noopener noreferrer" target="_blank">a</a>');
  assert.ok(inline('[m](mailto:x@y.z)').includes('href="mailto:x@y.z"'));
  for (const bad of ['javascript:alert(1)', 'JaVaScRiPt:alert(1)', 'data:text/html,<b>', 'vbscript:x', 'file:///etc/passwd', '/relative']) {
    const out = inline(`[click](${bad})`);
    assert.ok(!out.includes('<a'), bad);
    assert.ok(out.includes('click'), bad);
  }
  assert.equal(safeHref('java\tscript:alert(1)'), null);
  assert.equal(safeHref('https://ok.com'), 'https://ok.com');
});

test('link attributes cannot be broken out of', () => {
  const out = inline('[x](https://a.com/"onmouseover="alert(1))');
  assert.ok(!out.includes('" onmouseover'));
  assert.ok(!/href="[^"]*"[^>]*onmouseover/.test(out));
});

test('headings, paragraphs and emphasis', () => {
  assert.equal(renderMarkdown('# Title'), '<h1>Title</h1>');
  assert.equal(renderMarkdown('###### Six'), '<h6>Six</h6>');
  assert.equal(renderMarkdown('one\ntwo'), '<p>one two</p>');
  assert.equal(renderMarkdown('one\ntwo', { breaks: true }), '<p>one<br>two</p>');
  assert.equal(inline('**b** *i* ~~s~~ `c`'), '<strong>b</strong> <em>i</em> <del>s</del> <code>c</code>');
  assert.equal(inline('snake_case_name'), 'snake_case_name');
  assert.equal(inline('`**not bold**`'), '<code>**not bold**</code>');
  assert.equal(renderMarkdown('# T', { flatHeadings: true }), '<p class="md-h">T</p>');
});

test('lists: unordered, ordered with start, nested', () => {
  assert.equal(renderMarkdown('- a\n- b'), '<ul><li>a</li><li>b</li></ul>');
  assert.equal(renderMarkdown('1. a\n2. b'), '<ol><li>a</li><li>b</li></ol>');
  assert.equal(renderMarkdown('3. a\n4. b'), '<ol start="3"><li>a</li><li>b</li></ol>');
  assert.equal(renderMarkdown('- a\n  - a1\n  - a2\n- b'), '<ul><li>a<ul><li>a1</li><li>a2</li></ul></li><li>b</li></ul>');
  assert.equal(renderMarkdown('- a\n\ntext'), '<ul><li>a</li></ul><p>text</p>');
});

test('fenced code blocks keep their text and language', () => {
  assert.equal(renderMarkdown('```sql\nSELECT 1 < 2;\n```'), '<pre><code class="language-sql">SELECT 1 &lt; 2;</code></pre>');
  assert.equal(renderMarkdown('~~~\n# not a heading\n~~~'), '<pre><code># not a heading</code></pre>');
  // An unclosed fence runs to the end.
  assert.equal(renderMarkdown('```\na'), '<pre><code>a</code></pre>');
  assert.equal(renderMarkdown('```"><script>\nx\n```'), '<pre><code>x</code></pre>');
});

test('pipe tables with alignment', () => {
  const html = renderMarkdown('| a | b |\n|:--|--:|\n| 1 | **2** |\n| x \\| y | |');
  assert.equal(html, '<table><thead><tr><th style="text-align:left">a</th><th style="text-align:right">b</th></tr></thead>'
    + '<tbody><tr><td style="text-align:left">1</td><td style="text-align:right"><strong>2</strong></td></tr>'
    + '<tr><td style="text-align:left">x | y</td><td style="text-align:right"></td></tr></tbody></table>');
  // A line with pipes but no separator under it is a paragraph.
  assert.equal(renderMarkdown('a | b'), '<p>a | b</p>');
});

test('blockquotes and horizontal rules', () => {
  assert.equal(renderMarkdown('> quoted\n> more'), '<blockquote><p>quoted more</p></blockquote>');
  assert.equal(renderMarkdown('a\n\n---\n\nb'), '<p>a</p><hr><p>b</p>');
  assert.equal(renderMarkdown('***'), '<hr>');
});

test('the assistant parts: text and code apart, streaming code open', () => {
  const parts = markdownParts('Hi\n```sql\nSELECT 1\n```\nbye\n```js\nopen');
  assert.deepEqual(parts.map((p) => p.kind), ['text', 'code', 'text', 'code']);
  assert.equal(parts[1].code, 'SELECT 1');
  assert.equal(parts[1].open, false);
  assert.equal(parts[3].open, true);
  assert.equal(esc('<"&\'>'), '&lt;&quot;&amp;&#39;&gt;');
});

test('crafted lines render in linear time', () => {
  // Each of these took quadratic time with backtracking regexes (seconds at
  // this size); under the length cap, so the tokenizers themselves run.
  const n = 99_000;
  const cases = [
    'x' + '`'.repeat(n),
    '['.repeat(n),
    ' **a'.repeat(n / 4),
    ' __a'.repeat(n / 4),
    ' ~~a'.repeat(n / 4),
    '[a]('.repeat(n / 4),
    'a | b\n' + ' '.repeat(n) + 'x', // a table separator that isn't one
    '```' + ' '.repeat(n) + '`', // a fence that isn't one
    '~'.repeat(n) + '`', // nor this
    '# a' + ' '.repeat(n) + 'x', // a heading's text between spaces
    '#' + ' '.repeat(n) + '#' + ' '.repeat(n), // and its closing run
    '- ' + ' '.repeat(n) + '\u2028', // an item `(.*)` can't finish
    '```\n' + ' '.repeat(n) + '`', // a closing fence that isn't one
    '>' + ' '.repeat(n) + '\u2028',
    '-' + ' -'.repeat(n / 2) + 'x', // a rule that isn't one
    '|' + ' -|'.repeat(n / 3) + 'x', // a table separator that isn't one
  ];
  // Cubic before: `(.*?)` between two runs of spaces that can't reach `$`.
  cases.push('# ' + ' '.repeat(3000) + '\u2028');
  for (const md of cases) {
    for (const render of [renderMarkdown, markdownParts]) {
      const t = performance.now();
      render(md);
      const ms = performance.now() - t;
      assert.ok(ms < 250, `${render.name} ${JSON.stringify(md.slice(0, 12))}…: ${Math.round(ms)} ms`);
    }
  }
  // The chat answers' code blocks: an opener with no line break after it.
  for (const md of ['```' + 'a'.repeat(n), '`'.repeat(n)]) {
    const t = performance.now();
    markdownParts(md);
    assert.ok(performance.now() - t < 250, JSON.stringify(md.slice(0, 12)));
  }
  assert.equal(inline('x' + '`'.repeat(5)), 'x`````');
  assert.equal(inline('['.repeat(5)), '[[[[[');
});

test('lines over the length cap render in linear time, as escaped text', () => {
  const n = 1_000_000;
  const cases = [
    '#' + ' '.repeat(n) + 'x',
    '# a' + ' '.repeat(n) + 'x',
    '~'.repeat(n) + '`',
    '`'.repeat(n),
    '```' + 'a'.repeat(n),
    '- ' + ' '.repeat(n) + '\u2028',
    '<b>' + ' '.repeat(n) + '**x**',
  ];
  for (const md of cases) {
    for (const render of [renderMarkdown, markdownParts]) {
      const t = performance.now();
      render(md);
      const ms = performance.now() - t;
      assert.ok(ms < 500, `${render.name} ${JSON.stringify(md.slice(0, 12))}…: ${Math.round(ms)} ms`);
    }
  }
  assert.equal(renderMarkdown('<b>' + ' '.repeat(n) + '**x**'), `<p>&lt;b&gt;${' '.repeat(n)}**x**</p>`);
  // The lines around it keep their Markdown.
  assert.equal(renderMarkdown('# T\n' + 'x'.repeat(n) + '\n- a'), `<h1>T</h1><p>${'x'.repeat(n)}</p><ul><li>a</li></ul>`);
});

test('heading and fence edge cases keep their output', () => {
  assert.equal(renderMarkdown('## a ##'), '<h2>a</h2>');
  assert.equal(renderMarkdown('# a #  '), '<h1>a</h1>');
  assert.equal(renderMarkdown('# a#'), '<h1>a#</h1>');
  assert.equal(renderMarkdown('# #'), '<h1>#</h1>');
  assert.equal(renderMarkdown('#'), '<h1></h1>');
  assert.equal(renderMarkdown('####### seven'), '<p>####### seven</p>');
  assert.equal(renderMarkdown('#x'), '<p>#x</p>');
  assert.equal(renderMarkdown('    # indented'), '<p># indented</p>');
  assert.equal(renderMarkdown('~~~~ js `x`\na\n~~~~'), '<p>~~~~ js <code>x</code> a</p><pre><code></code></pre>');
  assert.equal(renderMarkdown('~~~~ js\na\n~~~\n~~~~'), '<pre><code class="language-js">a\n~~~</code></pre>');
  assert.equal(renderMarkdown('- a\n- \u2028b'), '<ul><li>a - \u2028b</li></ul>'); // `.` stops at U+2028: no second item
  assert.deepEqual(markdownParts('a\n```sql x\nSELECT\n```b'), [
    { kind: 'text', html: '<p>a</p>' }, { kind: 'code', lang: 'sql', code: 'SELECT', open: false }, { kind: 'text', html: '<p>b</p>' },
  ]);
});

test('code spans pair runs of the same length', () => {
  assert.equal(inline('``a ` b``'), '<code>a ` b</code>');
  assert.equal(inline('` a `'), '<code>a</code>');
  assert.equal(inline('`a` and `b`'), '<code>a</code> and <code>b</code>');
  // An unclosed run is literal, as in CommonMark.
  assert.equal(inline('``a`'), '``a`');
});

test('text over the length cap is shown escaped, without Markdown', () => {
  const md = '**<b>**' + '`'.repeat(200_000);
  const t = performance.now();
  const html = inline(md);
  assert.ok(performance.now() - t < 250);
  assert.ok(html.startsWith('**&lt;b&gt;**'));
});
