// DBine's style guide for the web UI (TypeScript and Vue 3), enforced with
// `npm run lint` (eslint . --max-warnings 0).
//
// It is a correctness and style-guide linter, not a formatter: the project
// doesn't use Prettier, so layout rules (indentation, quotes, line breaks,
// attribute order inside templates) are left to the existing code style.
//
// Base rule sets:
// - @eslint/js `recommended`
// - typescript-eslint `recommended` (the syntactic set, not the type-checked
//   one, so the lint stays fast and doesn't need a TS program per file)
// - eslint-plugin-vue `flat/recommended` (Vue 3)
import js from '@eslint/js'
import tseslint from 'typescript-eslint'
import pluginVue from 'eslint-plugin-vue'
import vueParser from 'vue-eslint-parser'
import globals from 'globals'

export default tseslint.config(
  {
    // Build output, dependencies, dev-only component previews (also out of
    // Codacy, see .codacy.yml), static assets, translations (JSON data) and
    // the generated engine-logo data URIs.
    ignores: [
      'dist/**',
      'node_modules/**',
      'public/**',
      'src/preview/**',
      'src/locales/**',
      'src/assets/engine-icons.ts',
    ],
  },
  js.configs.recommended,
  ...tseslint.configs.recommended,
  ...pluginVue.configs['flat/recommended'],
  {
    files: ['**/*.{ts,vue,js,mjs}'],
    languageOptions: {
      ecmaVersion: 'latest',
      sourceType: 'module',
      globals: { ...globals.browser },
    },
  },
  {
    // <script lang="ts"> inside .vue files goes through the TypeScript parser.
    files: ['**/*.vue'],
    languageOptions: {
      parser: vueParser,
      parserOptions: { parser: tseslint.parser, extraFileExtensions: ['.vue'] },
    },
  },
  {
    // TypeScript already reports undefined names (and knows DOM types such
    // as `EventListener`, which `no-undef` doesn't), as typescript-eslint
    // recommends.
    files: ['**/*.{ts,vue}'],
    rules: { 'no-undef': 'off' },
  },
  {
    // Unit tests and config files run in Node, not in the WebView.
    files: ['**/*.test.mjs', '*.config.{js,ts}'],
    languageOptions: { globals: { ...globals.node } },
  },
  {
    rules: {
      // Formatting rules from vue `strongly-recommended`/`recommended`: they
      // only reflow templates and would conflict with the existing layout.
      'vue/max-attributes-per-line': 'off',
      'vue/singleline-html-element-content-newline': 'off',
      'vue/multiline-html-element-content-newline': 'off',
      'vue/html-self-closing': 'off',
      'vue/html-indent': 'off',
      'vue/html-closing-bracket-newline': 'off',
      'vue/html-closing-bracket-spacing': 'off',
      'vue/attributes-order': 'off',
      'vue/first-attribute-linebreak': 'off',
      'vue/attribute-hyphenation': 'off',
      'vue/v-on-event-hyphenation': 'off',
      'vue/mustache-interpolation-spacing': 'off',
      // Single-word component names: App (always allowed) and Sparkline,
      // which predates the rule; every other component is multi-word.
      'vue/multi-word-component-names': ['error', { ignores: ['Sparkline'] }],
      // Editors for one step of a scheduled task, the SSH tunnel section and a
      // few tab views edit the reactive object the parent passes in (a store
      // entry or a draft the parent owns and saves). That is the established
      // pattern here; rewriting each to emit-and-copy would change how drafts
      // are saved for no gain.
      'vue/no-mutating-props': 'off',
      // Short loop names in templates (`t`, `tab`) shadow the script's i18n
      // `t` or the `tab` prop; templates call `$t`, so there's no ambiguity
      // and renaming would only churn the markup.
      'vue/no-template-shadow': 'off',
      // `_`-prefixed names mark a value left out on purpose (destructuring a
      // field away, a callback argument in position).
      '@typescript-eslint/no-unused-vars': ['error', {
        argsIgnorePattern: '^_',
        varsIgnorePattern: '^_',
        destructuredArrayIgnorePattern: '^_',
        caughtErrors: 'none',
      }],
    },
  },
)
