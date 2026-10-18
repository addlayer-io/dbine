import type { Extension } from '@codemirror/state';
import { EditorView } from '@codemirror/view';
import { HighlightStyle, syntaxHighlighting } from '@codemirror/language';
import { oneDark } from '@codemirror/theme-one-dark';
// A dependency of @codemirror/language, always installed with it.
import { tags } from '@lezer/highlight';
import type { Theme } from './appearance';

// The SQL / JSON editor's colours per theme. Dark is One Dark (with the
// workbench background, global.scss); light follows VS Code's Light+.

const light = {
  text: '#000000',
  keyword: '#0000ff',
  control: '#af00db',
  string: '#a31515',
  number: '#098658',
  type: '#267f99',
  func: '#795e26',
  variable: '#001080',
  property: '#001080',
  comment: '#008000',
  operator: '#000000',
  constant: '#0070c1',
  invalid: '#cd3131',
  selection: '#add6ff',
  selectionUnfocused: '#e5ebf1',
  cursor: '#000000',
  activeLine: '#0000000a',
  tooltip: '#f8f8f8',
  tooltipBorder: '#c8c8c8',
  tooltipSelected: '#0060c0',
};

const lightTheme = EditorView.theme({
  '&': { color: light.text, backgroundColor: '#ffffff' },
  '.cm-content': { caretColor: light.cursor },
  '.cm-cursor, .cm-dropCursor': { borderLeftColor: light.cursor },
  '&.cm-focused > .cm-scroller > .cm-selectionLayer .cm-selectionBackground, .cm-content ::selection': { backgroundColor: light.selection },
  '.cm-selectionBackground': { backgroundColor: light.selectionUnfocused },
  '.cm-panels': { backgroundColor: '#f3f3f3', color: '#3b3b3b' },
  '.cm-panels.cm-panels-top': { borderBottom: '1px solid #d4d4d4' },
  '.cm-panels.cm-panels-bottom': { borderTop: '1px solid #d4d4d4' },
  '.cm-searchMatch': { backgroundColor: '#ea5c0055', outline: '1px solid #ea5c00aa' },
  '.cm-searchMatch.cm-searchMatch-selected': { backgroundColor: '#a8ac94' },
  '.cm-activeLine': { backgroundColor: light.activeLine },
  '.cm-selectionMatch': { backgroundColor: '#add6ff80' },
  '&.cm-focused .cm-matchingBracket, &.cm-focused .cm-nonmatchingBracket': { backgroundColor: '#0064001a', outline: '1px solid #b9b9b9' },
  '.cm-gutters': { backgroundColor: '#ffffff', color: '#237893', border: 'none' },
  '.cm-activeLineGutter': { backgroundColor: 'transparent', color: '#0b216f' },
  '.cm-foldPlaceholder': { backgroundColor: 'transparent', border: 'none', color: '#6f6f6f' },
  '.cm-tooltip': { border: `1px solid ${light.tooltipBorder}`, backgroundColor: light.tooltip, color: '#3b3b3b' },
  '.cm-tooltip .cm-tooltip-arrow:before': { borderTopColor: light.tooltipBorder, borderBottomColor: light.tooltipBorder },
  '.cm-tooltip .cm-tooltip-arrow:after': { borderTopColor: light.tooltip, borderBottomColor: light.tooltip },
  '.cm-tooltip-autocomplete': {
    '& > ul > li[aria-selected]': { backgroundColor: light.tooltipSelected, color: '#ffffff' },
  },
}, { dark: false });

const lightHighlight = HighlightStyle.define([
  { tag: [tags.keyword, tags.operatorKeyword, tags.modifier, tags.bool, tags.null, tags.atom], color: light.keyword },
  { tag: [tags.controlKeyword, tags.moduleKeyword], color: light.control },
  { tag: [tags.name, tags.variableName, tags.labelName], color: light.variable },
  { tag: [tags.propertyName, tags.attributeName], color: light.property },
  { tag: [tags.function(tags.variableName), tags.function(tags.propertyName), tags.macroName], color: light.func },
  { tag: [tags.typeName, tags.className, tags.namespace, tags.standard(tags.name)], color: light.type },
  { tag: [tags.number, tags.integer, tags.float], color: light.number },
  { tag: [tags.string, tags.special(tags.string), tags.character, tags.inserted], color: light.string },
  { tag: [tags.regexp, tags.escape, tags.url], color: '#811f3f' },
  { tag: [tags.constant(tags.name), tags.special(tags.variableName), tags.color], color: light.constant },
  { tag: [tags.operator, tags.punctuation, tags.separator, tags.bracket], color: light.operator },
  { tag: [tags.comment, tags.meta], color: light.comment },
  { tag: tags.deleted, color: '#a31515' },
  { tag: tags.changed, color: '#0451a5' },
  { tag: tags.strong, fontWeight: 'bold' },
  { tag: tags.emphasis, fontStyle: 'italic' },
  { tag: tags.strikethrough, textDecoration: 'line-through' },
  { tag: tags.link, color: '#006ab1', textDecoration: 'underline' },
  { tag: tags.heading, fontWeight: 'bold', color: '#800000' },
  { tag: tags.invalid, color: light.invalid },
]);

const lightExt: Extension = [lightTheme, syntaxHighlighting(lightHighlight)];

/** The editor extension for `theme` (put it in a Compartment to switch). */
export function editorTheme(theme: Theme): Extension {
  return theme === 'light' ? lightExt : oneDark;
}
