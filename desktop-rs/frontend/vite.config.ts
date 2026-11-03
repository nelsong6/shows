import {defineConfig} from 'vite'
import react from '@vitejs/plugin-react'
import {showsDevelopment} from './dev-server.mjs'

// https://vitejs.dev/config/
export default defineConfig(async ({command}) => ({
  // Relative asset URLs so the bundle can be served from the desktop-qt
  // control server's root (it serves index.html + /assets directly).
  base: './',
  plugins: [react(), ...(command === 'serve' ? [await showsDevelopment()] : [])]
}))
