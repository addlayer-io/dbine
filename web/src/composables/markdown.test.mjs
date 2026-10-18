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
