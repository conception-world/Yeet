import { defineConfig } from "vitepress";

// VitePress site config. Deployed at https://conception-world.github.io/Yeet/
// via .github/workflows/docs.yml on every push to main that touches
// docs-site/**. Local preview: `npm run docs:dev` from docs-site/.
export default defineConfig({
	title: "Yeet",
	description:
		"Bidirectional file sync between Roblox Studio and your IDE (VS Code, Cursor, Antigravity).",
	// Project page on GitHub Pages → assets resolve under /Yeet/.
	// Bump this if the repo is ever renamed.
	base: "/Yeet/",
	// Avoid 404s on missing locale-specific assets in dev mode.
	cleanUrls: true,
	lastUpdated: true,

	head: [
		["link", { rel: "icon", href: "/Yeet/icon.png" }],
		["meta", { name: "theme-color", content: "#5b6dff" }],
		[
			"meta",
			{ property: "og:image", content: "https://conception-world.github.io/Yeet/icon.png" },
		],
		["meta", { property: "og:title", content: "Yeet — Roblox Studio ↔ IDE sync" }],
		[
			"meta",
			{
				property: "og:description",
				content:
					"Bidirectional sync between Roblox Studio and your IDE. Edit anywhere, see it everywhere within ~200 ms.",
			},
		],
	],

	themeConfig: {
		logo: "/icon.png",
		siteTitle: "Yeet",

		nav: [
			{ text: "Home", link: "/" },
			{ text: "Get started", link: "/getting-started" },
			{
				text: "Reference",
				items: [
					{ text: "Settings", link: "/settings" },
					{ text: "Architecture", link: "/architecture" },
				],
			},
			{
				text: "v0.3.0",
				items: [
					{
						text: "Changelog",
						link: "https://github.com/conception-world/Yeet/blob/main/yeet-extension/CHANGELOG.md",
					},
					{
						text: "Releases",
						link: "https://github.com/conception-world/Yeet/releases",
					},
				],
			},
		],

		sidebar: [
			{
				text: "Getting started",
				items: [
					{ text: "Overview", link: "/" },
					{ text: "Installation", link: "/getting-started" },
				],
			},
			{
				text: "Guide",
				items: [
					{ text: "Daily workflow", link: "/workflow" },
					{ text: "Troubleshooting", link: "/troubleshooting" },
				],
			},
			{
				text: "Reference",
				items: [
					{ text: "Settings", link: "/settings" },
					{ text: "How it works", link: "/architecture" },
				],
			},
		],

		socialLinks: [
			{ icon: "github", link: "https://github.com/conception-world/Yeet" },
		],

		footer: {
			message: "Released under the MIT License.",
			copyright: "© 2026 conception-world",
		},

		// Local search uses MiniSearch on top of the built JSON.
		// No external API key, no Algolia signup; works for the size
		// of this docs site. Swap to algolia provider later if the
		// content grows enough that local search feels slow.
		search: {
			provider: "local",
		},

		editLink: {
			pattern:
				"https://github.com/conception-world/Yeet/edit/main/docs-site/:path",
			text: "Edit this page on GitHub",
		},
	},
});
