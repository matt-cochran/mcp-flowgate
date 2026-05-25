// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import sitemap from '@astrojs/sitemap';
import tailwindcss from '@tailwindcss/vite';

export default defineConfig({
	site: 'https://mcp-flowgate.dev',
	vite: {
		plugins: [tailwindcss()],
	},
	integrations: [
		starlight({
			title: 'mcp-flowgate',
			description: 'Your LLM reads your entire tool list on every call. mcp-flowgate replaces it with seven.',
			logo: {
				light: './src/assets/logo-light.svg',
				dark: './src/assets/logo-dark.svg',
				replacesTitle: false,
			},
			social: [
				{ icon: 'github', label: 'GitHub', href: 'https://github.com/matt-cochran/mcp-flowgate' },
			],
			head: [
				{
					tag: 'meta',
					attrs: { property: 'og:image', content: 'https://mcp-flowgate.dev/og-image.png' },
				},
				{
					tag: 'meta',
					attrs: { name: 'twitter:card', content: 'summary_large_image' },
				},
			],
			customCss: ['./src/styles/custom.css'],
			sidebar: [
				{
					label: 'Getting Started',
					items: [
						{ label: 'What is mcp-flowgate?', slug: 'introduction' },
						{ label: 'Installation', slug: 'installation' },
						{ label: 'Quick start', slug: 'quick-start' },
					],
				},
				{
					label: 'Guides',
					items: [
						{ label: 'Discovery & search', slug: 'guides/discovery' },
						{ label: 'Governance', slug: 'guides/governance' },
						{ label: 'Workflows', slug: 'guides/workflows' },
						{ label: 'Connections', slug: 'guides/connections' },
						{ label: 'Deterministic chaining', slug: 'guides/chaining' },
						{ label: 'Phase guidance', slug: 'guides/phase-guidance' },
						{ label: 'Skills & cognitive architectures', slug: 'guides/skills-and-architectures' },
						{ label: 'Composing an architecture', slug: 'guides/composing-an-architecture' },
						{ label: 'Hot reload', slug: 'guides/hot-reload' },
						{ label: 'Going to production', slug: 'guides/production' },
					],
				},
				{
					label: 'Reference',
					items: [
						{ label: 'Configuration', slug: 'reference/configuration' },
						{ label: 'MCP tools', slug: 'reference/tools' },
						{ label: 'Guards', slug: 'reference/guards' },
						{ label: 'Executors', slug: 'reference/executors' },
						{ label: 'Stores', slug: 'reference/stores' },
						{ label: 'Cognitive verbs', slug: 'reference/cognitive-verbs' },
						{ label: 'Script verbs', slug: 'reference/script-verbs' },
						{ label: 'Audit events', slug: 'reference/audit' },
					],
				},
				{
					label: 'Advanced',
					items: [
						{ label: 'Embedding the library', slug: 'advanced/embedding' },
						{ label: 'Control architecture', slug: 'advanced/architecture' },
					],
				},
			],
		}),
		sitemap(),
	],
});
