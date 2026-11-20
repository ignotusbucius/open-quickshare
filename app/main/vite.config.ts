import vue from '@vitejs/plugin-vue'
import { resolve } from 'path'
import AutoImport from 'unplugin-auto-import/vite'
import { defineConfig } from 'vitest/config'
import path from 'path'

// See https://vitejs.dev/config/
export default defineConfig({
	plugins: [
		vue(),
		// See https://github.com/unplugin/unplugin-auto-import
		AutoImport({
			imports: ['vue'],
			dts: './src/auto-imports.d.ts',
			eslintrc: {
				enabled: true,
				filepath: resolve(__dirname, '.eslintrc-auto-import.json'),
			},
		}),
	],
	clearScreen: false,
	envPrefix: ['VITE_', 'TAURI_'],
	server: {
		port: 1420,
		strictPort: true,
		fs: {
			allow: [
				// Optionally, include other directories
				path.resolve(__dirname)
			]
		}
	},
	build: {
		outDir: './dist',
		// See https://tauri.app/v1/references/webview-versions for details
		target: process.env.TAURI_PLATFORM == 'windows' ? 'chrome105' : 'safari15',
		// Vite 8 minifies with Oxc; 'esbuild' would need esbuild installed separately.
		minify: !process.env.TAURI_DEBUG ? 'oxc' : false,
		sourcemap: !!process.env.TAURI_DEBUG,
		emptyOutDir: true,
	},
	// See https://vitest.dev/config/
	test: {
		include: ['tests/unit/**/*.{test,spec}.{js,mjs,cjs,ts,mts,cts,jsx,tsx}'],
	},
})
