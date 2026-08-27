/* eslint-disable @typescript-eslint/no-require-imports */
/* eslint-disable no-undef */
const resolve = require('path').resolve

module.exports = {
	darkMode: 'class',
	content: [
		resolve(__dirname, 'index.html'),
		resolve(__dirname, 'src/**/*.{vue,ts}')
	],
	plugins: [
		require('@tailwindcss/typography'),
	],
}
