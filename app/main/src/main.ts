import { devtools } from '@vue/devtools'
import { createApp } from 'vue'
import { createPinia } from 'pinia'

import './vue_lib/assets/main.postcss'

import App from './App.vue'

// Apply the persisted (or system) theme to <html> before mount, so the first
// paint is already in the right theme instead of flashing light then dark.
(function applyInitialTheme() {
	try {
		const stored = localStorage.getItem('theme') // 'light' | 'dark' | 'system' | null
		const prefersDark = window.matchMedia('(prefers-color-scheme: dark)').matches
		const dark = stored === 'dark' || ((stored === 'system' || stored === null) && prefersDark)
		document.documentElement.classList.toggle('dark', dark)
	} catch { /* keep default light */ }
})()

if (process.env.NODE_ENV === 'development') {
	devtools.connect('http://localhost', 8098)
}

const pinia = createPinia();
const app = createApp(App)

app.use(pinia);

app.mount('#app')
